//! Миниатюры страниц через PDFium в отдельном потоке.
//!
//! Библиотека PDFium загружается динамически. Если её нет, приложение работает,
//! но вместо миниатюр показывает номера страниц.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};

use eframe::egui;
use pdfium_render::prelude::*;

/// Задание: отрисовать указанные страницы документа.
struct Job {
    generation: u64,
    bytes: Arc<Vec<u8>>,
    pages: Vec<usize>,
}

pub struct Thumb {
    pub generation: u64,
    pub index: usize,
    pub image: egui::ColorImage,
}

pub struct Renderer {
    jobs: Sender<Job>,
    pub results: Receiver<Thumb>,
    /// None — PDFium загружен; Some(причина) — миниатюры недоступны.
    pub unavailable: Option<String>,
}

/// Где искать библиотеку PDFium: рядом с программой, в бандле macOS, в папке сборки.
fn library_candidates() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(exe_dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(PathBuf::from)) {
        dirs.push(exe_dir.join("../Frameworks"));
        dirs.push(exe_dir.join("../Resources"));
        dirs.push(exe_dir.join("../lib/pdf-tool"));
        dirs.push(exe_dir.join("../lib"));
        dirs.push(exe_dir);
    }
    let vendor = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("pdfium");
    dirs.push(vendor.join("lib"));
    dirs.push(vendor.join("bin"));
    dirs.into_iter().map(|d| Pdfium::pdfium_platform_library_name_at_path(&d)).collect()
}

fn bind() -> Result<Pdfium, String> {
    for path in library_candidates() {
        if path.is_file()
            && let Ok(bindings) = Pdfium::bind_to_library(&path)
        {
            return Ok(Pdfium::new(bindings));
        }
    }
    Pdfium::bind_to_system_library()
        .map(Pdfium::new)
        .map_err(|_| "библиотека PDFium не найдена — миниатюры отключены".to_owned())
}

impl Renderer {
    pub fn start(ctx: egui::Context, thumb_size: u32) -> Self {
        let (jobs_tx, jobs_rx) = channel::<Job>();
        let (results_tx, results_rx) = channel();
        let (status_tx, status_rx) = channel();

        std::thread::Builder::new()
            .name("pdfium".into())
            .spawn(move || {
                let pdfium = match bind() {
                    Ok(p) => {
                        let _ = status_tx.send(None);
                        p
                    }
                    Err(e) => {
                        let _ = status_tx.send(Some(e));
                        return;
                    }
                };
                worker(&pdfium, &jobs_rx, &results_tx, &ctx, thumb_size);
            })
            .expect("не удалось запустить поток отрисовки");

        let unavailable = status_rx.recv().unwrap_or_else(|_| Some("поток отрисовки завершился".into()));
        Renderer { jobs: jobs_tx, results: results_rx, unavailable }
    }

    pub fn request(&self, generation: u64, bytes: Arc<Vec<u8>>, pages: Vec<usize>) {
        if self.unavailable.is_none() && !pages.is_empty() {
            let _ = self.jobs.send(Job { generation, bytes, pages });
        }
    }
}

fn worker(pdfium: &Pdfium, jobs: &Receiver<Job>, results: &Sender<Thumb>, ctx: &egui::Context, size: u32) {
    let mut pending: Option<Job> = None;
    loop {
        let mut job = match pending.take() {
            Some(job) => job,
            None => match jobs.recv() {
                Ok(job) => job,
                Err(_) => return,
            },
        };
        // Берём самое свежее задание, старые больше не нужны
        while let Ok(newer) = jobs.try_recv() {
            job = newer;
        }

        let Ok(doc) = pdfium.load_pdf_from_byte_slice(&job.bytes, None) else { continue };
        let config = PdfRenderConfig::new()
            .set_target_width(size as Pixels)
            .set_maximum_height((size as f32 * 1.42) as Pixels);

        for &index in &job.pages {
            match jobs.try_recv() {
                Ok(newer) => {
                    pending = Some(newer);
                    break;
                }
                Err(TryRecvError::Disconnected) => return,
                Err(TryRecvError::Empty) => {}
            }
            let Ok(page) = doc.pages().get(index as PdfPageIndex) else { continue };
            let Ok(bitmap) = page.render_with_config(&config) else { continue };
            let (w, h) = (bitmap.width() as usize, bitmap.height() as usize);
            let image = egui::ColorImage::from_rgba_unmultiplied([w, h], &bitmap.as_rgba_bytes());
            if results.send(Thumb { generation: job.generation, index, image }).is_err() {
                return;
            }
            ctx.request_repaint();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn renders_thumbnails() {
        let renderer = Renderer::start(egui::Context::default(), 120);
        if renderer.unavailable.is_some() {
            eprintln!("PDFium не найден — тест пропущен");
            return;
        }
        let mut doc = crate::pdf_ops::new_document();
        let png = {
            let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(200, 400, image::Rgb([255, 0, 0])));
            let mut out = Vec::new();
            img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
            out
        };
        crate::pdf_ops::append_images(&mut doc, vec![crate::images::prepare_bytes(&png).unwrap()]).unwrap();
        let bytes = Arc::new(crate::pdf_ops::save(&mut doc).unwrap());

        renderer.request(1, bytes, vec![0]);
        let thumb = renderer.results.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!((thumb.generation, thumb.index), (1, 0));
        // Страница 1:2 вписывается в рамку 120 × 170 по высоте
        let [w, h] = thumb.image.size;
        assert!(w <= 120 && h <= 171 && h > w, "{w}x{h}");
        let center = thumb.image.pixels[(h / 2) * w + w / 2];
        assert_eq!((center.r(), center.g(), center.b()), (255, 0, 0));
    }
}
