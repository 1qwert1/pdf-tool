//! Сжатие PDF: через Ghostscript, если он установлен, иначе встроенным пересжатием изображений.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use image::{DynamicImage, GrayImage, ImageFormat, RgbImage, imageops::FilterType};
use lopdf::{Document, Object, ObjectId, Stream};

use crate::{images, pdf_ops};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Engine {
    Ghostscript,
    Builtin,
}

impl Engine {
    pub fn name(self) -> &'static str {
        match self {
            Engine::Ghostscript => "Ghostscript",
            Engine::Builtin => "встроенный",
        }
    }
}

/// Путь к Ghostscript: gs (Linux/macOS) или gswin64c/gswin32c (Windows).
pub fn ghostscript_path() -> Option<PathBuf> {
    for name in ["gs", "gswin64c", "gswin32c"] {
        if let Ok(path) = which::which(name) {
            return Some(path);
        }
    }
    // Приложения, запущенные из Finder, не видят PATH из терминала
    [
        "/opt/homebrew/bin/gs",
        "/usr/local/bin/gs",
        "/opt/local/bin/gs",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|p| p.is_file())
}

/// Пресет Ghostscript (-dPDFSETTINGS) для значения ползунка 1–100.
fn gs_settings(quality: u8) -> &'static str {
    match quality {
        0..=25 => "/screen",
        26..=50 => "/ebook",
        51..=75 => "/printer",
        _ => "/prepress",
    }
}

pub fn preset_hint(quality: u8, ghostscript: bool) -> &'static str {
    if ghostscript {
        match gs_settings(quality) {
            "/screen" => "screen — макс. сжатие (~72 dpi)",
            "/ebook" => "ebook — для чтения (~150 dpi)",
            "/printer" => "printer — для печати (~300 dpi)",
            _ => "prepress — высокое качество",
        }
    } else {
        match quality {
            90.. => "Высокое качество, минимальное сжатие",
            60..=89 => "Сбалансированное качество",
            30..=59 => "Среднее сжатие",
            _ => "Максимальное сжатие, низкое качество",
        }
    }
}

/// Сжимает PDF. quality: 1 (макс. сжатие) — 100 (макс. качество).
pub fn compress(bytes: &[u8], quality: u8) -> Result<(Vec<u8>, Engine)> {
    let quality = quality.clamp(1, 100);
    match ghostscript_path() {
        Some(gs) => Ok((compress_ghostscript(&gs, bytes, quality)?, Engine::Ghostscript)),
        None => Ok((compress_builtin(bytes, quality)?, Engine::Builtin)),
    }
}

fn compress_ghostscript(gs: &Path, bytes: &[u8], quality: u8) -> Result<Vec<u8>> {
    let dir = tempfile::tempdir().context("Не удалось создать временную папку")?;
    let input = dir.path().join("in.pdf");
    let output = dir.path().join("out.pdf");
    std::fs::write(&input, bytes)?;

    let result = Command::new(gs)
        .args([
            "-sDEVICE=pdfwrite",
            "-dCompatibilityLevel=1.4",
            &format!("-dPDFSETTINGS={}", gs_settings(quality)),
            "-dNOPAUSE",
            "-dQUIET",
            "-dBATCH",
            "-dSAFER",
            "-dDetectDuplicateImages=true",
            "-dCompressFonts=true",
            "-dSubsetFonts=true",
        ])
        .arg(format!("-sOutputFile={}", output.display()))
        .arg(&input)
        .output()
        .context("Не удалось запустить Ghostscript")?;

    let out = std::fs::read(&output).unwrap_or_default();
    if !result.status.success() || out.is_empty() {
        let stderr = String::from_utf8_lossy(&result.stderr);
        let stdout = String::from_utf8_lossy(&result.stdout);
        let msg = if stderr.trim().is_empty() { stdout } else { stderr };
        bail!("Ghostscript завершился с ошибкой: {}", msg.trim());
    }
    Ok(out)
}

/// Максимальная сторона изображения в пикселях и качество JPEG для встроенного движка.
fn builtin_params(quality: u8) -> (Option<u32>, u8) {
    let max_side = match quality {
        0..=30 => Some(1200),
        31..=60 => Some(1800),
        61..=85 => Some(2600),
        _ => None,
    };
    (max_side, (25 + quality as u32 * 67 / 100) as u8)
}

/// Встроенное сжатие: пересжимает изображения в JPEG с уменьшением, сжимает остальные потоки.
fn compress_builtin(bytes: &[u8], quality: u8) -> Result<Vec<u8>> {
    let mut doc = pdf_ops::load(bytes, "документ")?;
    let (max_side, jpeg_quality) = builtin_params(quality);

    // Маски прозрачности не трогаем: JPEG исказит края
    let masks: HashSet<ObjectId> = doc
        .objects
        .values()
        .filter_map(|o| o.as_stream().ok())
        .filter_map(|s| s.dict.get(b"SMask").and_then(Object::as_reference).ok())
        .collect();

    let image_ids: Vec<ObjectId> = doc
        .objects
        .iter()
        .filter(|(id, o)| {
            !masks.contains(id)
                && o.as_stream()
                    .is_ok_and(|s| s.dict.get(b"Subtype").and_then(Object::as_name).is_ok_and(|n| n == b"Image"))
        })
        .map(|(id, _)| *id)
        .collect();

    for id in image_ids {
        let Some(stream) = doc.objects.get(&id).and_then(|o| o.as_stream().ok()) else { continue };
        if let Some((data, w, h)) = recompress_image(&doc, stream, max_side, jpeg_quality) {
            let Some(Object::Stream(stream)) = doc.objects.get_mut(&id) else { continue };
            stream.dict.set("Filter", Object::Name(b"DCTDecode".to_vec()));
            stream.dict.remove(b"DecodeParms");
            stream.dict.set("Width", w as i64);
            stream.dict.set("Height", h as i64);
            stream.dict.set("BitsPerComponent", 8);
            stream.set_content(data);
            stream.allows_compression = false;
        }
    }

    doc.compress();
    doc.prune_objects();
    pdf_ops::save(&mut doc)
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
        Object::Array(arr) => {
            let kind = arr.first()?.as_name().ok()?;
            if kind == b"ICCBased" {
                let profile = doc.get_object(arr.get(1)?.as_reference().ok()?).ok()?.as_stream().ok()?;
                let n = profile.dict.get(b"N").and_then(Object::as_i64).ok()?;
                matches!(n, 1 | 3).then_some(n as usize)
            } else if kind == b"CalRGB" {
                Some(3)
            } else if kind == b"CalGray" {
                Some(1)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Пересжимает одно изображение. Возвращает новые данные, если они меньше исходных.
fn recompress_image(
    doc: &Document,
    stream: &Stream,
    max_side: Option<u32>,
    jpeg_quality: u8,
) -> Option<(Vec<u8>, u32, u32)> {
    let dict = &stream.dict;
    if dict.get(b"ImageMask").and_then(Object::as_bool).unwrap_or(false) || dict.has(b"Decode") {
        return None;
    }
    if dict.get(b"BitsPerComponent").and_then(Object::as_i64).ok()? != 8 {
        return None;
    }
    let w = u32::try_from(dict.get(b"Width").and_then(Object::as_i64).ok()?).ok()?;
    let h = u32::try_from(dict.get(b"Height").and_then(Object::as_i64).ok()?).ok()?;
    if w < 32 || h < 32 {
        return None;
    }
    let comps = color_components(doc, dict.get(b"ColorSpace").ok()?)?;

    let filters: Vec<&[u8]> = match dict.get(b"Filter") {
        Ok(Object::Name(n)) => vec![n.as_slice()],
        Ok(Object::Array(arr)) => arr.iter().filter_map(|f| f.as_name().ok()).collect(),
        _ => vec![],
    };
    let expected = w as usize * h as usize * comps;

    let img = if filters == [b"DCTDecode".as_slice()] {
        let decoded = image::load_from_memory_with_format(&stream.content, ImageFormat::Jpeg).ok()?;
        if decoded.width() != w || decoded.height() != h {
            return None;
        }
        if comps == 1 { DynamicImage::ImageLuma8(decoded.to_luma8()) } else { DynamicImage::ImageRgb8(decoded.to_rgb8()) }
    } else if filters.iter().all(|f| matches!(*f, b"FlateDecode" | b"LZWDecode" | b"RunLengthDecode")) {
        let raw = stream.decompressed_content_with_limit(expected + 1024).ok()?;
        if raw.len() < expected {
            return None;
        }
        let raw = raw[..expected].to_vec();
        if comps == 1 {
            DynamicImage::ImageLuma8(GrayImage::from_raw(w, h, raw)?)
        } else {
            DynamicImage::ImageRgb8(RgbImage::from_raw(w, h, raw)?)
        }
    } else {
        return None;
    };

    let img = match max_side {
        Some(max) if w.max(h) > max => img.resize(max, max, FilterType::Triangle),
        _ => img,
    };
    let data = images::encode_jpeg(&img, jpeg_quality).ok()?;
    (data.len() < stream.content.len()).then(|| (data, img.width(), img.height()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PDF с одной страницей-картинкой без сжатия (PNG → Flate).
    fn sample_pdf() -> Vec<u8> {
        let img = DynamicImage::ImageRgb8(RgbImage::from_fn(3000, 2000, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x * y) % 256) as u8])
        }));
        let mut png = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut png), ImageFormat::Png).unwrap();
        let prepared = images::prepare_bytes(&png).unwrap();
        let mut doc = pdf_ops::images_to_document(vec![prepared]).unwrap();
        pdf_ops::save(&mut doc).unwrap()
    }

    #[test]
    fn builtin_shrinks_images() {
        let original = sample_pdf();
        let compressed = compress_builtin(&original, 40).unwrap();
        assert!(compressed.len() < original.len() / 2, "{} -> {}", original.len(), compressed.len());
        let doc = pdf_ops::load(&compressed, "t").unwrap();
        assert_eq!(pdf_ops::page_count(&doc), 1);
    }

    #[test]
    fn ghostscript_if_available() {
        let Some(gs) = ghostscript_path() else { return };
        let out = compress_ghostscript(&gs, &sample_pdf(), 30).unwrap();
        assert_eq!(pdf_ops::page_count(&pdf_ops::load(&out, "t").unwrap()), 1);
    }
}
