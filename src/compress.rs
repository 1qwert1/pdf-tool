//! Нативное сжатие PDF без внешних программ.
//!
//! Как и Ghostscript: изображения уменьшаются по реальному разрешению на странице
//! (dpi) и пересжимаются в JPEG, одинаковые изображения склеиваются, потоки
//! и структура документа сжимаются.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use anyhow::Result;
use image::{DynamicImage, GrayImage, ImageFormat, RgbImage, imageops::FilterType};
use lopdf::content::Content;
use lopdf::{Dictionary, Document, Object, ObjectId, SaveOptions, Stream, dictionary};

use crate::i18n::tr;
use crate::{images, pdf_ops};

/// Параметры сжатия для значения ползунка.
struct Preset {
    /// Изображения с большим разрешением уменьшаются до этого dpi.
    target_dpi: f32,
    jpeg_quality: u8,
    /// Сканы документов с малым числом оттенков (1–4 бита): ниже 150 dpi
    /// текст становится нечитаемым, поэтому у них своя шкала.
    scan_dpi: f32,
    /// Число бит на пиксель для таких сканов; None — как в исходнике.
    scan_bits: Option<u8>,
    /// Серые 8-битные сканы документов (в основном белый фон) переводятся
    /// в столько бит на пиксель; None — остаются JPEG.
    doc_bits: Option<u8>,
}

fn preset(quality: u8) -> Preset {
    let (target_dpi, jpeg_quality, scan_dpi, scan_bits, doc_bits) = match quality {
        0..=25 => (72.0, 45, 150.0, Some(1), Some(1)),
        26..=50 => (150.0, 60, 150.0, None, Some(2)),
        51..=75 => (220.0, 75, 200.0, None, Some(4)),
        _ => (300.0, 85, 300.0, None, None),
    };
    Preset { target_dpi, jpeg_quality, scan_dpi, scan_bits, doc_bits }
}

pub fn preset_hint(quality: u8) -> &'static str {
    match quality {
        0..=25 => tr(
            "Максимальное сжатие — фото до ~72 dpi, сканы документов чёрно-белые 150 dpi",
            "Maximum compression — photos up to ~72 dpi, document scans black-and-white at 150 dpi",
        ),
        26..=50 => tr(
            "Для чтения с экрана — фото до ~150 dpi, сканы документов в 4 оттенках серого",
            "For on-screen reading — photos up to ~150 dpi, document scans in 4 shades of gray",
        ),
        51..=75 => tr(
            "Сбалансированное — фото до ~220 dpi, сканы документов в 16 оттенках серого",
            "Balanced — photos up to ~220 dpi, document scans in 16 shades of gray",
        ),
        _ => tr("Для печати — изображения до ~300 dpi", "For printing — images up to ~300 dpi"),
    }
}

/// Уменьшаем, только если разрешение заметно выше целевого (как Ghostscript).
const DOWNSAMPLE_THRESHOLD: f32 = 1.5;

/// Сжимает PDF. quality: 1 (макс. сжатие) — 100 (макс. качество).
/// Возвращает None, если файл не удалось уменьшить (документ тогда не меняют).
pub fn compress(bytes: &[u8], quality: u8) -> Result<Option<Vec<u8>>> {
    let preset = preset(quality.clamp(1, 100));
    let mut doc = pdf_ops::load(bytes, tr("документ", "document"))?;

    merge_duplicate_images(&mut doc);
    let placements = image_placements(&doc);
    recompress_images(&mut doc, &placements, &preset);

    doc.compress();
    doc.prune_objects();
    let mut out = Vec::new();
    let options = SaveOptions::builder()
        .use_object_streams(true)
        .use_xref_streams(true)
        .compression_level(9)
        .build();
    doc.save_with_options(&mut out, options)?;
    Ok((out.len() < bytes.len()).then_some(out))
}

// ---------- Дубликаты ----------

fn is_image(stream: &Stream) -> bool {
    stream.dict.get(b"Subtype").and_then(Object::as_name).is_ok_and(|n| n == b"Image")
}

/// Заменяет ссылки на одинаковые изображения ссылкой на одно из них.
fn merge_duplicate_images(doc: &mut Document) {
    let mut seen: HashMap<u64, Vec<ObjectId>> = HashMap::new();
    let mut replace: HashMap<ObjectId, ObjectId> = HashMap::new();
    for (&id, object) in &doc.objects {
        let Ok(stream) = object.as_stream() else { continue };
        if !is_image(stream) {
            continue;
        }
        let mut hasher = DefaultHasher::new();
        stream.content.hash(&mut hasher);
        format!("{:?}", stream.dict).hash(&mut hasher);
        let candidates = seen.entry(hasher.finish()).or_default();
        let same = candidates.iter().find(|&&other| {
            doc.objects.get(&other).and_then(|o| o.as_stream().ok()).is_some_and(|o| {
                o.content == stream.content && format!("{:?}", o.dict) == format!("{:?}", stream.dict)
            })
        });
        match same {
            Some(&original) => {
                replace.insert(id, original);
            }
            None => candidates.push(id),
        }
    }
    if replace.is_empty() {
        return;
    }
    doc.traverse_objects(|object| {
        if let Object::Reference(id) = object
            && let Some(&original) = replace.get(id)
        {
            *id = original;
        }
    });
}

// ---------- Размещение изображений на страницах ----------

type Matrix = [f32; 6];
const IDENTITY: Matrix = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

/// m × n в терминах PDF (сначала m, затем n).
fn multiply(m: &Matrix, n: &Matrix) -> Matrix {
    [
        m[0] * n[0] + m[1] * n[2],
        m[0] * n[1] + m[1] * n[3],
        m[2] * n[0] + m[3] * n[2],
        m[2] * n[1] + m[3] * n[3],
        m[4] * n[0] + m[5] * n[2] + n[4],
        m[4] * n[1] + m[5] * n[3] + n[5],
    ]
}

fn matrix_from(objects: &[Object]) -> Option<Matrix> {
    let values: Vec<f32> = objects.iter().filter_map(|o| o.as_float().ok()).collect();
    values.try_into().ok()
}

/// Наибольший размер, в котором изображение показано (ширина и высота в пунктах).
type Placements = HashMap<ObjectId, (f32, f32)>;

fn image_placements(doc: &Document) -> Placements {
    let mut placements = Placements::new();
    for page_id in pdf_ops::page_ids(doc) {
        let Some(resources) = pdf_ops::inherited_attr(doc, page_id, b"Resources") else { continue };
        let Some(resources) = deref_dict(doc, &resources) else { continue };
        let content = doc.get_page_content(page_id);
        let mut visited = HashSet::new();
        walk_content(doc, &content, resources, IDENTITY, &mut placements, &mut visited);
    }
    placements
}

fn deref_dict<'a>(doc: &'a Document, object: &'a Object) -> Option<&'a Dictionary> {
    match object {
        Object::Reference(id) => doc.get_dictionary(*id).ok(),
        Object::Dictionary(dict) => Some(dict),
        _ => None,
    }
}

fn walk_content(
    doc: &Document,
    content: &[u8],
    resources: &Dictionary,
    base: Matrix,
    placements: &mut Placements,
    visited_forms: &mut HashSet<ObjectId>,
) {
    let Ok(content) = Content::decode(content) else { return };
    let xobjects = resources.get(b"XObject").ok().and_then(|x| deref_dict(doc, x));
    let mut ctm = base;
    let mut stack = Vec::new();

    for op in &content.operations {
        match op.operator.as_str() {
            "q" => stack.push(ctm),
            "Q" => ctm = stack.pop().unwrap_or(base),
            "cm" => {
                if let Some(m) = matrix_from(&op.operands) {
                    ctm = multiply(&m, &ctm);
                }
            }
            "Do" => {
                let Some(name) = op.operands.first().and_then(|o| o.as_name().ok()) else { continue };
                let Some(id) = xobjects.and_then(|x| x.get(name).ok()).and_then(|o| o.as_reference().ok())
                else {
                    continue;
                };
                let Ok(stream) = doc.get_object(id).and_then(Object::as_stream) else { continue };
                if is_image(stream) {
                    let w = ctm[0].hypot(ctm[1]);
                    let h = ctm[2].hypot(ctm[3]);
                    let entry = placements.entry(id).or_insert((0.0, 0.0));
                    *entry = (entry.0.max(w), entry.1.max(h));
                } else if visited_forms.insert(id) {
                    // Form XObject: свои ресурсы и матрица
                    let matrix = stream
                        .dict
                        .get(b"Matrix")
                        .and_then(Object::as_array)
                        .ok()
                        .and_then(|m| matrix_from(m))
                        .unwrap_or(IDENTITY);
                    let form_resources = stream
                        .dict
                        .get(b"Resources")
                        .ok()
                        .and_then(|r| deref_dict(doc, r))
                        .unwrap_or(resources);
                    if let Ok(form_content) = stream.decompressed_content() {
                        let form_ctm = multiply(&matrix, &ctm);
                        walk_content(doc, &form_content, form_resources, form_ctm, placements, visited_forms);
                    }
                    visited_forms.remove(&id);
                }
            }
            _ => {}
        }
    }
}

// ---------- Пересжатие изображений ----------

fn recompress_images(doc: &mut Document, placements: &Placements, preset: &Preset) {
    // Маски прозрачности уменьшаются вместе со своим изображением, отдельно их не трогаем
    let masks: HashSet<ObjectId> = doc
        .objects
        .values()
        .filter_map(|o| o.as_stream().ok())
        .filter_map(|s| s.dict.get(b"SMask").and_then(Object::as_reference).ok())
        .collect();
    let image_ids: Vec<ObjectId> = doc
        .objects
        .iter()
        .filter(|(id, o)| !masks.contains(id) && o.as_stream().is_ok_and(is_image))
        .map(|(id, _)| *id)
        .collect();

    for id in image_ids {
        let Some(stream) = doc.objects.get(&id).and_then(|o| o.as_stream().ok()) else { continue };
        let Some(result) = recompress_image(doc, stream, placements.get(&id).copied(), preset) else { continue };
        let mask_id = stream.dict.get(b"SMask").and_then(Object::as_reference).ok();

        if let Some(Object::Stream(stream)) = doc.objects.get_mut(&id) {
            stream.dict.set("Width", result.width as i64);
            stream.dict.set("Height", result.height as i64);
            if result.to_gray {
                stream.dict.set("ColorSpace", Object::Name(b"DeviceGray".to_vec()));
            }
            match result.encoding {
                Encoding::Jpeg => {
                    stream.dict.set("Filter", Object::Name(b"DCTDecode".to_vec()));
                    stream.dict.remove(b"DecodeParms");
                    stream.dict.set("BitsPerComponent", 8);
                }
                Encoding::Flate { bits } => {
                    stream.dict.set("Filter", Object::Name(b"FlateDecode".to_vec()));
                    stream.dict.set(
                        "DecodeParms",
                        dictionary! {
                            "Predictor" => 12,
                            "Colors" => 1,
                            "BitsPerComponent" => bits as i64,
                            "Columns" => result.width as i64,
                        },
                    );
                    stream.dict.set("BitsPerComponent", bits as i64);
                }
            }
            stream.set_content(result.data);
            stream.allows_compression = false;
        }
        if let Some(mask_id) = mask_id {
            resize_mask(doc, mask_id, result.width, result.height);
        }
    }
}

enum Encoding {
    Jpeg,
    /// Серое изображение с `bits` бит на пиксель, Flate с PNG-предсказанием Up.
    Flate { bits: u8 },
}

struct Recompressed {
    data: Vec<u8>,
    width: u32,
    height: u32,
    encoding: Encoding,
    /// Цветное изображение без цвета сохранено как серое.
    to_gray: bool,
}

/// Число цветовых компонент для поддерживаемых пространств (Gray, RGB, ICCBased).
fn color_components(doc: &Document, cs: &Object) -> Option<usize> {
    let cs = match cs {
        Object::Reference(id) => doc.get_object(*id).ok()?,
        other => other,
    };
    match cs {
        Object::Name(n) if n == b"DeviceRGB" || n == b"CalRGB" => Some(3),
        Object::Name(n) if n == b"DeviceGray" || n == b"CalGray" => Some(1),
        Object::Array(arr) => match arr.first()?.as_name().ok()? {
            b"ICCBased" => {
                let profile = doc.get_object(arr.get(1)?.as_reference().ok()?).ok()?.as_stream().ok()?;
                let n = profile.dict.get(b"N").and_then(Object::as_i64).ok()?;
                matches!(n, 1 | 3).then_some(n as usize)
            }
            b"CalRGB" => Some(3),
            b"CalGray" => Some(1),
            _ => None,
        },
        _ => None,
    }
}

/// Раскодирует 8-битное изображение в серое или RGB.
fn decode_image(stream: &Stream, w: u32, h: u32, comps: usize) -> Option<DynamicImage> {
    let filters: Vec<&[u8]> = match stream.dict.get(b"Filter") {
        Ok(Object::Name(n)) => vec![n.as_slice()],
        Ok(Object::Array(arr)) => arr.iter().filter_map(|f| f.as_name().ok()).collect(),
        _ => vec![],
    };
    if filters == [b"DCTDecode".as_slice()] {
        let decoded = image::load_from_memory_with_format(&stream.content, ImageFormat::Jpeg).ok()?;
        if decoded.width() != w || decoded.height() != h {
            return None;
        }
        return Some(if comps == 1 {
            DynamicImage::ImageLuma8(decoded.to_luma8())
        } else {
            DynamicImage::ImageRgb8(decoded.to_rgb8())
        });
    }
    if !filters.iter().all(|f| matches!(*f, b"FlateDecode" | b"LZWDecode" | b"RunLengthDecode")) {
        return None;
    }
    let expected = w as usize * h as usize * comps;
    let raw = stream.decompressed_content_with_limit(expected + 1024).ok()?;
    let raw = raw.get(..expected)?.to_vec();
    Some(if comps == 1 {
        DynamicImage::ImageLuma8(GrayImage::from_raw(w, h, raw)?)
    } else {
        DynamicImage::ImageRgb8(RgbImage::from_raw(w, h, raw)?)
    })
}

/// Новый размер изображения: уменьшаем, если реальное разрешение выше целевого.
fn target_size(w: u32, h: u32, placement: Option<(f32, f32)>, target_dpi: f32) -> (u32, u32) {
    let Some((w_pt, h_pt)) = placement.filter(|(a, b)| *a > 0.5 && *b > 0.5) else {
        return (w, h);
    };
    // Разрешение по менее плотной оси — его и сохраняем не ниже целевого
    let dpi = (w as f32 * 72.0 / w_pt).min(h as f32 * 72.0 / h_pt);
    if dpi <= target_dpi * DOWNSAMPLE_THRESHOLD {
        return (w, h);
    }
    let scale = target_dpi / dpi;
    (((w as f32 * scale).round() as u32).max(1), ((h as f32 * scale).round() as u32).max(1))
}

fn recompress_image(
    doc: &Document,
    stream: &Stream,
    placement: Option<(f32, f32)>,
    preset: &Preset,
) -> Option<Recompressed> {
    let dict = &stream.dict;
    if dict.get(b"ImageMask").and_then(Object::as_bool).unwrap_or(false) || dict.has(b"Decode") || dict.has(b"Mask") {
        return None;
    }
    let bpc = dict.get(b"BitsPerComponent").and_then(Object::as_i64).ok()?;
    let w = u32::try_from(dict.get(b"Width").and_then(Object::as_i64).ok()?).ok()?;
    let h = u32::try_from(dict.get(b"Height").and_then(Object::as_i64).ok()?).ok()?;
    if w < 16 || h < 16 {
        return None;
    }
    let comps = color_components(doc, dict.get(b"ColorSpace").ok()?)?;
    if matches!(bpc, 1 | 2 | 4) && comps == 1 {
        return recompress_scan(stream, w, h, bpc as u8, placement, preset);
    }
    if bpc != 8 {
        return None;
    }
    let img = decode_image(stream, w, h, comps)?;
    let original_len = stream.content.len();

    // Цветной скан без цвета (частый случай у сканеров) храним как серый
    let to_gray = comps == 3 && looks_gray(&img);
    let img = if to_gray { DynamicImage::ImageLuma8(img.to_luma8()) } else { img };

    // Серый скан документа: меньше оттенков и Flate дают файл в разы меньше JPEG
    if (comps == 1 || to_gray)
        && let Some(bits) = preset.doc_bits
        && is_document(&img)
    {
        let (sw, sh) = target_size(w, h, placement, preset.scan_dpi);
        let gray = img.to_luma8();
        let gray = if (sw, sh) != (w, h) { image::imageops::resize(&gray, sw, sh, FilterType::Lanczos3) } else { gray };
        let data = pack_gray_flate(&gray, bits);
        if data.len() < original_len {
            return Some(Recompressed {
                data,
                width: sw,
                height: sh,
                encoding: Encoding::Flate { bits },
                to_gray: comps == 3,
            });
        }
    }

    let (new_w, new_h) = target_size(w, h, placement, preset.target_dpi);
    let img = if (new_w, new_h) != (w, h) { img.resize_exact(new_w, new_h, FilterType::Lanczos3) } else { img };
    let data = images::encode_jpeg(&img, preset.jpeg_quality).ok()?;
    (data.len() < original_len).then_some(Recompressed {
        data,
        width: img.width(),
        height: img.height(),
        encoding: Encoding::Jpeg,
        to_gray,
    })
}

/// Цветное изображение, в котором почти нет цвета (оттенки серого, сохранённые в RGB).
fn looks_gray(img: &DynamicImage) -> bool {
    let DynamicImage::ImageRgb8(rgb) = img else { return false };
    let total = rgb.pixels().len().max(1);
    let colored = rgb
        .pixels()
        .step_by(3)
        .filter(|p| {
            let [r, g, b] = p.0;
            r.abs_diff(g).max(g.abs_diff(b)).max(r.abs_diff(b)) > 24
        })
        .count();
    colored * 3 * 1000 < total * 2
}

/// Скан документа: большая часть изображения — светлый фон.
fn is_document(img: &DynamicImage) -> bool {
    let gray = img.to_luma8();
    let total = gray.pixels().len().max(1);
    let light = gray.pixels().step_by(3).filter(|p| p.0[0] >= 200).count();
    light * 3 * 10 >= total * 6
}

/// Скан документа с 1–4 битами серого: уменьшение до разрешения сканов
/// и упаковка с тем же (или меньшим) числом оттенков. JPEG здесь только хуже.
fn recompress_scan(
    stream: &Stream,
    w: u32,
    h: u32,
    bits: u8,
    placement: Option<(f32, f32)>,
    preset: &Preset,
) -> Option<Recompressed> {
    let filters: Vec<&[u8]> = match stream.dict.get(b"Filter") {
        Ok(Object::Name(n)) => vec![n.as_slice()],
        Ok(Object::Array(arr)) => arr.iter().filter_map(|f| f.as_name().ok()).collect(),
        _ => vec![],
    };
    // CCITT и JBIG2 уже сжимают чёрно-белые сканы лучше, их не трогаем
    if !filters.iter().all(|f| matches!(*f, b"FlateDecode" | b"LZWDecode" | b"RunLengthDecode")) {
        return None;
    }
    let row_bytes = (w as usize * bits as usize).div_ceil(8);
    let raw = stream.decompressed_content_with_limit(row_bytes * h as usize + 1024).ok()?;
    let img = unpack_gray(raw.get(..row_bytes * h as usize)?, w, h, bits)?;

    let (new_w, new_h) = target_size(w, h, placement, preset.scan_dpi);
    let img = if (new_w, new_h) != (w, h) {
        image::imageops::resize(&img, new_w, new_h, FilterType::Lanczos3)
    } else {
        img
    };
    let out_bits = preset.scan_bits.map_or(bits, |b| b.min(bits));
    let data = pack_gray_flate(&img, out_bits);
    (data.len() < stream.content.len()).then_some(Recompressed {
        data,
        width: img.width(),
        height: img.height(),
        encoding: Encoding::Flate { bits: out_bits },
        to_gray: false,
    })
}

/// Распаковывает 1/2/4-битные строки в 8-битное серое изображение.
fn unpack_gray(raw: &[u8], w: u32, h: u32, bits: u8) -> Option<GrayImage> {
    let row_bytes = (w as usize * bits as usize).div_ceil(8);
    let max = (1u16 << bits) - 1;
    let per_byte = 8 / bits as usize;
    let mut pixels = Vec::with_capacity(w as usize * h as usize);
    for row in raw.chunks(row_bytes).take(h as usize) {
        for x in 0..w as usize {
            let byte = row[x / per_byte];
            let shift = 8 - bits as usize * (x % per_byte + 1);
            let value = (byte >> shift) as u16 & max;
            pixels.push((value * 255 / max) as u8);
        }
    }
    GrayImage::from_raw(w, h, pixels)
}

/// Квантует до `bits` бит, упаковывает строки, применяет PNG-фильтр Up и сжимает Flate.
fn pack_gray_flate(img: &GrayImage, bits: u8) -> Vec<u8> {
    use std::io::Write;
    let (w, h) = (img.width() as usize, img.height() as usize);
    let row_bytes = (w * bits as usize).div_ceil(8);
    let max = (1u16 << bits) - 1;
    let level = |v: u8| -> u8 {
        if bits == 1 {
            // Порог чуть выше середины: после уменьшения тонкие штрихи серые, их сохраняем
            u8::from(v >= 160)
        } else {
            ((v as u16 * max + 127) / 255) as u8
        }
    };

    let mut filtered = Vec::with_capacity((row_bytes + 1) * h);
    let mut prev = vec![0u8; row_bytes];
    let mut row = vec![0u8; row_bytes];
    for y in 0..h {
        row.fill(0);
        for x in 0..w {
            let bit_pos = x * bits as usize;
            row[bit_pos / 8] |= level(img.get_pixel(x as u32, y as u32).0[0]) << (8 - bits as usize - bit_pos % 8);
        }
        filtered.push(2); // PNG Up
        filtered.extend(row.iter().zip(&prev).map(|(a, b)| a.wrapping_sub(*b)));
        std::mem::swap(&mut prev, &mut row);
    }
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    let _ = encoder.write_all(&filtered);
    encoder.finish().unwrap_or_default()
}

/// Уменьшает маску прозрачности до размеров изображения (без потерь, Flate).
fn resize_mask(doc: &mut Document, mask_id: ObjectId, width: u32, height: u32) {
    let Some(stream) = doc.objects.get(&mask_id).and_then(|o| o.as_stream().ok()) else { return };
    let dict = &stream.dict;
    let (Ok(w), Ok(h)) = (dict.get(b"Width").and_then(Object::as_i64), dict.get(b"Height").and_then(Object::as_i64))
    else {
        return;
    };
    let (w, h) = (w as u32, h as u32);
    if (w, h) == (width, height) || w * h <= width * height {
        return;
    }
    if dict.get(b"BitsPerComponent").and_then(Object::as_i64).ok() != Some(8) || dict.has(b"Decode") {
        return;
    }
    let Some(img) = decode_image(stream, w, h, 1) else { return };
    let resized = img.resize_exact(width, height, FilterType::Triangle).into_bytes();
    if let Some(Object::Stream(stream)) = doc.objects.get_mut(&mask_id) {
        stream.dict.remove(b"Filter");
        stream.dict.remove(b"DecodeParms");
        stream.dict.set("Width", width as i64);
        stream.dict.set("Height", height as i64);
        stream.set_plain_content(resized);
        let _ = stream.compress();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::dictionary;

    fn noise_image(w: u32, h: u32) -> DynamicImage {
        DynamicImage::ImageRgb8(RgbImage::from_fn(w, h, |x, y| {
            let v = (x.wrapping_mul(2654435761) ^ y.wrapping_mul(40503)) as u8;
            image::Rgb([v, v.wrapping_mul(3), (x * y) as u8])
        }))
    }

    /// Одна страница A4 с фото 3000×4000 (≈360 dpi), вставленным как JPEG.
    fn scan_pdf() -> Vec<u8> {
        let jpeg = images::encode_jpeg(&noise_image(3000, 4000), 92).unwrap();
        let prepared = images::prepare_bytes(&jpeg).unwrap();
        let mut doc = pdf_ops::images_to_document(vec![prepared]).unwrap();
        pdf_ops::save(&mut doc).unwrap()
    }

    fn image_sizes(bytes: &[u8]) -> Vec<(i64, i64)> {
        let doc = pdf_ops::load(bytes, "t").unwrap();
        doc.objects
            .values()
            .filter_map(|o| o.as_stream().ok())
            .filter(|s| is_image(s))
            .map(|s| (s.dict.get(b"Width").unwrap().as_i64().unwrap(), s.dict.get(b"Height").unwrap().as_i64().unwrap()))
            .collect()
    }

    #[test]
    fn downsamples_by_real_dpi() {
        let original = scan_pdf();
        let out = compress(&original, 40).unwrap().expect("должно сжаться");
        assert!(out.len() < original.len() / 3, "{} -> {}", original.len(), out.len());
        // Страница ≈ 631×842 pt, при 150 dpi картинка ≈ 1315×1754
        let (w, h) = image_sizes(&out)[0];
        assert!((1250..=1400).contains(&w) && (1700..=1800).contains(&h), "{w}x{h}");
        assert_eq!(pdf_ops::page_count(&pdf_ops::load(&out, "t").unwrap()), 1);
    }

    #[test]
    fn high_quality_keeps_resolution_when_close() {
        // 300 dpi предела × 1.5 — картинка 360 dpi не уменьшается
        let out = compress(&scan_pdf(), 90).unwrap();
        if let Some(out) = out {
            assert_eq!(image_sizes(&out)[0], (3000, 4000));
        }
    }

    #[test]
    fn keeps_document_when_not_smaller() {
        let mut doc = pdf_ops::new_document();
        let out = pdf_ops::save(&mut doc).unwrap();
        assert!(compress(&out, 50).unwrap().is_none());
    }

    #[test]
    fn placement_follows_transforms() {
        let mut doc = pdf_ops::new_document();
        let image = doc.add_object(Stream::new(
            dictionary! { "Type" => "XObject", "Subtype" => "Image", "Width" => 100, "Height" => 100 },
            vec![],
        ));
        // Форма масштабирует в 2 раза, внутри картинка 50×30
        let form = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Form",
                "Matrix" => vec![2.into(), 0.into(), 0.into(), 2.into(), 0.into(), 0.into()],
                "Resources" => dictionary! { "XObject" => dictionary! { "Im" => image } },
            },
            b"q 50 0 0 30 0 0 cm /Im Do Q".to_vec(),
        ));
        let resources = dictionary! { "XObject" => dictionary! { "Fm" => form } };
        let mut placements = Placements::new();
        walk_content(&doc, b"q 1 0 0 1 10 10 cm /Fm Do Q", &resources, IDENTITY, &mut placements, &mut HashSet::new());
        assert_eq!(placements[&image], (100.0, 60.0));
    }

    /// Страница A4 со «сканом» 2480×3508 (300 dpi), 2 бита серого, Flate без предсказания.
    fn two_bit_scan_pdf() -> Vec<u8> {
        use std::io::Write;
        let (w, h) = (2480usize, 3508usize);
        let row_bytes = w.div_ceil(4);
        let mut raw = vec![0xFFu8; row_bytes * h];
        // Строки «текста»: чёрные штрихи и серые края
        for y in (300..3200).step_by(60) {
            for dy in 0..20 {
                for x in (200..2200).step_by(9) {
                    raw[(y + dy) * row_bytes + x / 4] = if dy % 5 == 0 { 0b0101_0101 } else { 0 };
                }
            }
        }
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(&raw).unwrap();
        let mut doc = pdf_ops::new_document();
        let image = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image", "Width" => w as i64, "Height" => h as i64,
                "ColorSpace" => "DeviceGray", "BitsPerComponent" => 2, "Filter" => "FlateDecode",
            },
            z.finish().unwrap(),
        ));
        let content = doc.add_object(Stream::new(Dictionary::new(), b"q 595 0 0 842 0 0 cm /Im Do Q".to_vec()));
        let root = doc.catalog().unwrap().get(b"Pages").unwrap().as_reference().unwrap();
        let page = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => root,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            "Resources" => dictionary! { "XObject" => dictionary! { "Im" => image } },
            "Contents" => content,
        });
        let pages = doc.get_dictionary_mut(root).unwrap();
        pages.set("Kids", vec![Object::Reference(page)]);
        pages.set("Count", 1);
        pdf_ops::save(&mut doc).unwrap()
    }

    fn image_dict(bytes: &[u8]) -> (Dictionary, Vec<u8>) {
        let doc = pdf_ops::load(bytes, "t").unwrap();
        let s = doc.objects.values().filter_map(|o| o.as_stream().ok()).find(|s| is_image(s)).unwrap();
        (s.dict.clone(), s.decompressed_content().unwrap())
    }

    #[test]
    fn pack_and_unpack_roundtrip() {
        let img = GrayImage::from_fn(13, 5, |x, y| image::Luma([((x * 3 + y) % 4 * 85) as u8]));
        for bits in [1u8, 2, 4] {
            let packed = pack_gray_flate(&img, bits);
            let mut z = flate2::read::ZlibDecoder::new(&packed[..]);
            let mut filtered = Vec::new();
            std::io::Read::read_to_end(&mut z, &mut filtered).unwrap();
            // Снимаем PNG-фильтр Up
            let row = (13 * bits as usize).div_ceil(8);
            let mut raw = Vec::new();
            let mut prev = vec![0u8; row];
            for chunk in filtered.chunks(row + 1) {
                assert_eq!(chunk[0], 2);
                let cur: Vec<u8> = chunk[1..].iter().zip(&prev).map(|(a, b)| a.wrapping_add(*b)).collect();
                raw.extend(&cur);
                prev = cur;
            }
            let back = unpack_gray(&raw, 13, 5, bits).unwrap();
            if bits >= 2 {
                assert_eq!(back, img, "{bits} бит");
            } else {
                assert!(back.pixels().zip(img.pixels()).all(|(a, b)| (a.0[0] == 255) == (b.0[0] >= 160)));
            }
        }
    }

    #[test]
    fn two_bit_scans_are_downsampled() {
        let original = two_bit_scan_pdf();
        let out = compress(&original, 40).unwrap().expect("должно сжаться");
        let (dict, data) = image_dict(&out);
        // 300 → 150 dpi, оттенки сохранены, lopdf снимает предсказание при чтении
        assert_eq!(dict.get(b"Width").unwrap().as_i64().unwrap(), 1240);
        assert_eq!(dict.get(b"BitsPerComponent").unwrap().as_i64().unwrap(), 2);
        assert_eq!(data.len(), 1240usize.div_ceil(4) * 1754);

        let out = compress(&original, 20).unwrap().unwrap();
        assert_eq!(image_dict(&out).0.get(b"BitsPerComponent").unwrap().as_i64().unwrap(), 1);
    }

    #[test]
    fn duplicates_are_merged() {
        let jpeg = images::encode_jpeg(&noise_image(64, 64), 80).unwrap();
        let imgs = vec![images::prepare_bytes(&jpeg).unwrap(), images::prepare_bytes(&jpeg).unwrap()];
        let mut doc = pdf_ops::images_to_document(imgs).unwrap();
        merge_duplicate_images(&mut doc);
        doc.prune_objects();
        let count = doc.objects.values().filter(|o| o.as_stream().is_ok_and(is_image)).count();
        assert_eq!(count, 1);
    }
}
