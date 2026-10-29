//! Подготовка изображений к вставке в PDF: EXIF-поворот, приведение режимов, кодирование.

use std::io::Cursor;
use std::path::Path;

use anyhow::{Context, Result};
use image::codecs::jpeg::JpegEncoder;
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader};
use lopdf::{Object, Stream, dictionary};

pub const IMAGE_EXTENSIONS: [&str; 8] = ["png", "jpg", "jpeg", "bmp", "gif", "tiff", "tif", "webp"];

/// Изображение, готовое к записи в PDF как XObject.
pub struct PreparedImage {
    width: u32,
    height: u32,
    color_space: &'static str,
    /// Данные: JPEG (DCTDecode) или несжатые пиксели (сожмутся Flate).
    data: Vec<u8>,
    jpeg: bool,
}

/// Наибольшая сторона страницы — длинная сторона A4 в пунктах.
const MAX_PAGE_SIDE_PT: f32 = 842.0;
/// Разрешение, с которым изображение ложится на страницу, если оно меньше A4.
const BASE_DPI: f32 = 96.0;

impl PreparedImage {
    /// Размер страницы в пунктах: изображение при 96 dpi, но не больше A4.
    pub fn page_size_pt(&self) -> (f32, f32) {
        let w = self.width as f32 * 72.0 / BASE_DPI;
        let h = self.height as f32 * 72.0 / BASE_DPI;
        let scale = (MAX_PAGE_SIDE_PT / w.max(h)).min(1.0);
        (w * scale, h * scale)
    }

    pub fn into_stream(self) -> Stream {
        let dict = dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => self.width as i64,
            "Height" => self.height as i64,
            "ColorSpace" => self.color_space,
            "BitsPerComponent" => 8,
        };
        if self.jpeg {
            let mut dict = dict;
            dict.set("Filter", Object::Name(b"DCTDecode".to_vec()));
            Stream::new(dict, self.data).with_compression(false)
        } else {
            let mut stream = Stream::new(dict, self.data);
            let _ = stream.compress();
            stream
        }
    }
}

/// Число цветовых компонент JPEG по маркеру SOF (1 — серый, 3 — RGB/YCbCr, 4 — CMYK).
fn jpeg_components(data: &[u8]) -> Option<u8> {
    let mut i = 2;
    while i + 9 < data.len() {
        if data[i] != 0xFF {
            return None;
        }
        let marker = data[i + 1];
        let len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
        let is_sof = matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
        if is_sof {
            return Some(data[i + 9]);
        }
        i += 2 + len;
    }
    None
}

/// Приводит изображение к 8-битному RGB или оттенкам серого.
/// Прозрачные области накладываются на белый фон.
pub fn normalize(img: DynamicImage) -> DynamicImage {
    use DynamicImage::*;
    match img {
        ImageLuma8(_) | ImageRgb8(_) => img,
        ImageLuma16(_) => ImageLuma8(img.to_luma8()),
        ImageLumaA8(_) | ImageLumaA16(_) => {
            let rgba = img.to_rgba8();
            ImageLuma8(DynamicImage::ImageRgb8(flatten_alpha(&rgba)).to_luma8())
        }
        _ if img.color().has_alpha() => ImageRgb8(flatten_alpha(&img.to_rgba8())),
        _ => ImageRgb8(img.to_rgb8()),
    }
}

fn flatten_alpha(rgba: &image::RgbaImage) -> image::RgbImage {
    image::RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let [r, g, b, a] = rgba.get_pixel(x, y).0;
        let a = a as u32;
        let blend = |c: u8| ((c as u32 * a + 255 * (255 - a) + 127) / 255) as u8;
        image::Rgb([blend(r), blend(g), blend(b)])
    })
}

/// Читает изображение, учитывает EXIF-ориентацию и готовит его к вставке.
pub fn prepare(path: &Path) -> Result<PreparedImage> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let bytes = std::fs::read(path).with_context(|| format!("Не удалось прочитать «{name}»"))?;
    prepare_bytes(&bytes).with_context(|| format!("Не удалось открыть изображение «{name}»"))
}

pub fn prepare_bytes(bytes: &[u8]) -> Result<PreparedImage> {
    let reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let format = reader.format();
    let mut decoder = reader.into_decoder()?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);

    // JPEG без поворота вставляем как есть — без потери качества и с исходным размером
    if format == Some(ImageFormat::Jpeg) && orientation == Orientation::NoTransforms {
        let color_space = match jpeg_components(bytes) {
            Some(1) => Some("DeviceGray"),
            Some(3) => Some("DeviceRGB"),
            _ => None, // CMYK и экзотика — перекодируем
        };
        if let Some(color_space) = color_space {
            let (width, height) = decoder.dimensions();
            return Ok(PreparedImage { width, height, color_space, data: bytes.to_vec(), jpeg: true });
        }
    }

    let mut img = DynamicImage::from_decoder(decoder)?;
    img.apply_orientation(orientation);
    let img = normalize(img);
    let (width, height) = (img.width(), img.height());
    let color_space = if matches!(img, DynamicImage::ImageLuma8(_)) { "DeviceGray" } else { "DeviceRGB" };

    if format == Some(ImageFormat::Jpeg) {
        // Повёрнутое фото: сохраняем снова в JPEG, иначе файл вырастет в разы
        let data = encode_jpeg(&img, 92)?;
        return Ok(PreparedImage { width, height, color_space, data, jpeg: true });
    }
    Ok(PreparedImage { width, height, color_space, data: img.into_bytes(), jpeg: false })
}

pub fn encode_jpeg(img: &DynamicImage, quality: u8) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, quality).encode_image(img)?;
    Ok(out)
}

pub fn is_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMAGE_EXTENSIONS.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Luma, LumaA, Rgba};

    fn png_bytes(img: DynamicImage) -> Vec<u8> {
        let mut out = Vec::new();
        img.write_to(&mut Cursor::new(&mut out), ImageFormat::Png).unwrap();
        out
    }

    #[test]
    fn handles_all_modes() {
        let images = [
            DynamicImage::ImageLuma16(ImageBuffer::from_pixel(4, 4, Luma([40000u16]))),
            DynamicImage::ImageLumaA8(ImageBuffer::from_pixel(4, 4, LumaA([10, 0]))),
            DynamicImage::ImageRgba8(ImageBuffer::from_pixel(4, 4, Rgba([255, 0, 0, 0]))),
            DynamicImage::new_rgb16(4, 4),
            DynamicImage::new_rgba16(4, 4),
        ];
        for img in images {
            let prepared = prepare_bytes(&png_bytes(img)).unwrap();
            assert_eq!(prepared.width, 4);
        }
    }

    #[test]
    fn transparency_becomes_white() {
        let img = DynamicImage::ImageRgba8(ImageBuffer::from_pixel(2, 2, Rgba([255, 0, 0, 0])));
        let prepared = prepare_bytes(&png_bytes(img)).unwrap();
        assert_eq!(&prepared.data[..3], &[255, 255, 255]);
    }

    #[test]
    fn sixteen_bit_gray_is_scaled() {
        let img = DynamicImage::ImageLuma16(ImageBuffer::from_pixel(2, 2, Luma([65535u16])));
        let prepared = prepare_bytes(&png_bytes(img)).unwrap();
        assert_eq!(prepared.color_space, "DeviceGray");
        assert_eq!(prepared.data[0], 255);
    }

    #[test]
    fn jpeg_is_passed_through() {
        let img = DynamicImage::new_rgb8(30, 20);
        let jpeg = encode_jpeg(&img, 80).unwrap();
        let prepared = prepare_bytes(&jpeg).unwrap();
        assert!(prepared.jpeg);
        assert_eq!(prepared.data, jpeg);
        assert_eq!((prepared.width, prepared.height), (30, 20));
    }

    #[test]
    fn page_fits_a4() {
        let big = PreparedImage { width: 4000, height: 3000, color_space: "DeviceRGB", data: vec![], jpeg: true };
        let (w, h) = big.page_size_pt();
        assert!((w - 842.0).abs() < 0.01 && h < w);
    }
}
