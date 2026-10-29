//! Миниатюры страниц через PDFium в отдельном потоке.
//!
//! Библиотека PDFium загружается динамически. Если её нет, приложение работает,
//! но вместо миниатюр показывает номера страниц.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};

use eframe::egui;
use pdfium_render::prelude::*;

/// Что рисуем: миниатюры списка или крупную страницу для редактора.
/// У каждого вида своя очередь: новое задание заменяет только задание того же вида.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Purpose {
    Thumb,
    Page,
}

/// Задание: отрисовать указанные страницы документа.
struct Job {
    purpose: Purpose,
    generation: u64,
    bytes: Arc<Vec<u8>>,
    pages: Vec<usize>,
    /// Ширина в пикселях (для миниатюр высота ограничена 1.42 × ширина).
    width: u32,
}

pub struct Thumb {
    pub purpose: Purpose,
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
        .map_err(|_| crate::i18n::tr("библиотека PDFium не найдена — миниатюры отключены", "PDFium library not found — thumbnails are disabled").to_owned())
}

impl Renderer {
    pub fn start(ctx: egui::Context) -> Self {
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
                worker(&pdfium, &jobs_rx, &results_tx, &ctx);
            })
            .expect("failed to start the render thread");

        let unavailable = status_rx.recv().unwrap_or_else(|_| Some(crate::i18n::tr("поток отрисовки завершился", "render thread stopped").into()));
        Renderer { jobs: jobs_tx, results: results_rx, unavailable }
    }

    pub fn request(&self, purpose: Purpose, generation: u64, bytes: Arc<Vec<u8>>, pages: Vec<usize>, width: u32) {
        if self.unavailable.is_none() && !pages.is_empty() {
            let _ = self.jobs.send(Job { purpose, generation, bytes, pages, width });
        }
    }
}

/// Последние задания каждого вида; страница редактора рисуется первой.
#[derive(Default)]
struct Queue {
    page: Option<Job>,
    thumbs: Option<Job>,
}

impl Queue {
    fn put(&mut self, job: Job) {
        match job.purpose {
            Purpose::Page => self.page = Some(job),
            Purpose::Thumb => self.thumbs = Some(job),
        }
    }

    /// Забирает все пришедшие задания. false — приложение закрыто.
    fn drain(&mut self, jobs: &Receiver<Job>) -> bool {
        loop {
            match jobs.try_recv() {
                Ok(job) => self.put(job),
                Err(TryRecvError::Empty) => return true,
                Err(TryRecvError::Disconnected) => return false,
            }
        }
    }
}

fn worker(pdfium: &Pdfium, jobs: &Receiver<Job>, results: &Sender<Thumb>, ctx: &egui::Context) {
    let mut queue = Queue::default();
    loop {
        if queue.page.is_none() && queue.thumbs.is_none() {
            match jobs.recv() {
                Ok(job) => queue.put(job),
                Err(_) => return,
            }
        }
        if !queue.drain(jobs) {
            return;
        }
        let Some(mut job) = queue.page.take().or_else(|| queue.thumbs.take()) else { continue };
        let bytes = job.bytes.clone();
        let Ok(doc) = pdfium.load_pdf_from_byte_slice(&bytes, None) else { continue };
        let mut config = PdfRenderConfig::new().set_target_width(job.width as Pixels);
        if job.purpose == Purpose::Thumb {
            config = config.set_maximum_height((job.width as f32 * 1.42) as Pixels);
        }

        while !job.pages.is_empty() {
            // Миниатюры рисуем по одной, проверяя, не пришло ли что-то важнее
            if job.purpose == Purpose::Thumb {
                if !queue.drain(jobs) {
                    return;
                }
                if queue.page.is_some() || queue.thumbs.is_some() {
                    if queue.thumbs.is_none() {
                        queue.thumbs = Some(job);
                    }
                    break;
                }
            }
            let index = job.pages.remove(0);
            let Ok(page) = doc.pages().get(index as PdfPageIndex) else { continue };
            let Ok(bitmap) = page.render_with_config(&config) else { continue };
            let (w, h) = (bitmap.width() as usize, bitmap.height() as usize);
            let image = egui::ColorImage::from_rgba_unmultiplied([w, h], &bitmap.as_rgba_bytes());
            let thumb = Thumb { purpose: job.purpose, generation: job.generation, index, image };
            if results.send(thumb).is_err() {
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
        let renderer = Renderer::start(egui::Context::default());
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

        renderer.request(Purpose::Thumb, 1, bytes.clone(), vec![0], 120);
        let thumb = renderer.results.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!((thumb.generation, thumb.index), (1, 0));
        // Страница 1:2 вписывается в рамку 120 × 170 по высоте
        let [w, h] = thumb.image.size;
        assert!(w <= 120 && h <= 171 && h > w, "{w}x{h}");
        let center = thumb.image.pixels[(h / 2) * w + w / 2];
        assert_eq!((center.r(), center.g(), center.b()), (255, 0, 0));

        // Крупная страница для редактора рисуется без ограничения высоты
        renderer.request(Purpose::Page, 2, bytes, vec![0], 400);
        let page = renderer.results.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!((page.purpose, page.generation), (Purpose::Page, 2));
        assert_eq!(page.image.size, [400, 800]);
    }
}
