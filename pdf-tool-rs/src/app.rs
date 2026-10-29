//! Главное окно PDF Tool.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};

use anyhow::Result;
use eframe::egui::{
    self, Align2, Color32, FontId, Key, KeyboardShortcut, Modifiers, RichText, Sense, Stroke,
    StrokeKind, Vec2, ViewportCommand,
};

use crate::compress;
use crate::images::{self, IMAGE_EXTENSIONS};
use crate::pdf_ops;
use crate::render::Renderer;

const THUMB_SIZE: u32 = 120;
const CARD_SIZE: Vec2 = Vec2::new(136.0, 190.0);

const BLUE: Color32 = Color32::from_rgb(26, 115, 232);
const RED: Color32 = Color32::from_rgb(217, 48, 37);
const GRAY_TEXT: Color32 = Color32::from_rgb(95, 99, 104);
const SELECTED_FILL: Color32 = Color32::from_rgb(232, 240, 254);

const SHORTCUT_NEW: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::N);
const SHORTCUT_OPEN: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::O);
const SHORTCUT_SAVE: KeyboardShortcut =
    KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::S);
const SHORTCUT_QUIT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Q);
const SHORTCUT_SELECT_ALL: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::A);

/// Открытый документ: рабочая копия в памяти.
struct DocState {
    bytes: Arc<Vec<u8>>,
    page_count: usize,
    /// Исходный или последний сохранённый файл; None — новый документ.
    path: Option<PathBuf>,
    dirty: bool,
    thumbs: Vec<Option<egui::TextureHandle>>,
}

impl DocState {
    fn display_name(&self) -> String {
        let name = self.path.as_deref().map(pdf_ops::file_name).unwrap_or_else(|| "Новый документ".into());
        if self.dirty { format!("{name} *") } else { name }
    }

    fn stem(&self) -> String {
        self.path
            .as_deref()
            .and_then(Path::file_stem)
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "document".into())
    }
}

/// Результат фоновой операции.
enum Outcome {
    /// Новое содержимое документа.
    Document {
        bytes: Vec<u8>,
        /// Some(path) — сменить файл документа; None — оставить прежний.
        path: Option<Option<PathBuf>>,
        dirty: bool,
        /// Для каждой новой страницы — индекс старой с той же миниатюрой.
        page_map: Option<Vec<Option<usize>>>,
        message: String,
    },
    Saved(PathBuf),
    Info { title: String, text: String },
}

struct Task {
    label: String,
    rx: Receiver<Result<Outcome>>,
}

/// Действия, перед которыми нужно спросить про несохранённые изменения.
enum Pending {
    New,
    Open(PathBuf),
    FromImages(Vec<PathBuf>),
    Close,
}

enum InsertSource {
    Images(Vec<PathBuf>),
    Pdf(PathBuf),
}

enum Dialog {
    Discard { title: &'static str, action: Pending },
    ConfirmDelete(Vec<usize>),
    Insert { source: InsertSource, position: usize, max: usize },
    Message { title: String, text: String },
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Create,
    Pages,
    Quality,
}

pub struct PdfToolApp {
    renderer: Renderer,
    doc: Option<DocState>,
    selected: BTreeSet<usize>,
    anchor: Option<usize>,
    tab: Tab,
    quality: u8,
    ghostscript: bool,
    status: String,
    task: Option<Task>,
    dialog: Option<Dialog>,
    allow_close: bool,
    generation: u64,
    /// Счётчик кадров для отладочного снимка окна (PDF_TOOL_SCREENSHOT).
    #[cfg(debug_assertions)]
    frames: u32,
}

impl PdfToolApp {
    pub fn new(cc: &eframe::CreationContext<'_>, open: Option<PathBuf>) -> Self {
        let ctx = &cc.egui_ctx;
        ctx.set_theme(egui::Theme::Light);
        ctx.global_style_mut(|style| {
            style.spacing.button_padding = Vec2::new(12.0, 6.0);
            style.spacing.item_spacing = Vec2::new(8.0, 8.0);
        });

        let renderer = Renderer::start(ctx.clone(), THUMB_SIZE);
        let mut app = PdfToolApp {
            renderer,
            doc: None,
            selected: BTreeSet::new(),
            anchor: None,
            tab: Tab::Create,
            quality: 50,
            ghostscript: compress::ghostscript_path().is_some(),
            status: String::new(),
            task: None,
            dialog: None,
            allow_close: false,
            generation: 0,
            #[cfg(debug_assertions)]
            frames: 0,
        };
        if let Some(path) = open {
            app.run_pending(ctx, Pending::Open(path));
        }
        app
    }

    fn is_dirty(&self) -> bool {
        self.doc.as_ref().is_some_and(|d| d.dirty)
    }

    fn busy(&self) -> bool {
        self.task.is_some()
    }

    fn error(&mut self, title: &str, text: impl Into<String>) {
        self.dialog = Some(Dialog::Message { title: title.into(), text: text.into() });
    }

    /// Запускает операцию в фоновом потоке.
    fn spawn(
        &mut self,
        ctx: &egui::Context,
        label: &str,
        job: impl FnOnce() -> Result<Outcome> + Send + 'static,
    ) {
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(job());
            ctx.request_repaint();
        });
        self.task = Some(Task { label: label.into(), rx });
    }

    fn poll_task(&mut self, ctx: &egui::Context) {
        let Some(task) = &self.task else { return };
        let Ok(result) = task.rx.try_recv() else { return };
        self.task = None;
        match result {
            Ok(outcome) => self.apply(ctx, outcome),
            Err(e) => self.error("Ошибка", format!("{e:#}")),
        }
    }

    fn apply(&mut self, ctx: &egui::Context, outcome: Outcome) {
        match outcome {
            Outcome::Document { bytes, path, dirty, page_map, message } => {
                let page_count = match pdf_ops::load(&bytes, "документ") {
                    Ok(doc) => pdf_ops::page_count(&doc),
                    Err(e) => return self.error("Ошибка", format!("{e:#}")),
                };
                let old = self.doc.take();
                let mut thumbs = vec![None; page_count];
                let mut selected = BTreeSet::new();
                if let (Some(old), Some(map)) = (&old, &page_map) {
                    for (new_i, old_i) in map.iter().enumerate() {
                        if let Some(old_i) = *old_i {
                            thumbs[new_i] = old.thumbs.get(old_i).cloned().flatten();
                            if self.selected.contains(&old_i) {
                                selected.insert(new_i);
                            }
                        }
                    }
                }
                let path = match path {
                    Some(p) => p,
                    None => old.and_then(|o| o.path),
                };
                let doc = DocState { bytes: Arc::new(bytes), page_count, path, dirty, thumbs };
                self.selected = selected;
                self.anchor = None;
                self.generation += 1;
                let missing: Vec<usize> = (0..page_count).filter(|&i| doc.thumbs[i].is_none()).collect();
                self.renderer.request(self.generation, doc.bytes.clone(), missing);
                self.doc = Some(doc);
                self.status = message;
                ctx.request_repaint();
            }
            Outcome::Saved(path) => {
                if let Some(doc) = &mut self.doc {
                    doc.dirty = false;
                    self.status = format!("Сохранено: {}", path.display());
                    doc.path = Some(path);
                }
            }
            Outcome::Info { title, text } => {
                self.status = text.lines().next().unwrap_or_default().to_owned();
                self.dialog = Some(Dialog::Message { title, text });
            }
        }
    }

    fn receive_thumbs(&mut self, ctx: &egui::Context) {
        while let Ok(thumb) = self.renderer.results.try_recv() {
            if thumb.generation != self.generation {
                continue;
            }
            if let Some(slot) = self.doc.as_mut().and_then(|d| d.thumbs.get_mut(thumb.index)) {
                let name = format!("page-{}-{}", thumb.generation, thumb.index);
                *slot = Some(ctx.load_texture(name, thumb.image, egui::TextureOptions::LINEAR));
            }
        }
    }

    // ---------- Действия ----------

    /// Выполняет действие, при необходимости спросив про несохранённые изменения.
    fn request(&mut self, ctx: &egui::Context, title: &'static str, action: Pending) {
        if self.is_dirty() {
            self.dialog = Some(Dialog::Discard { title, action });
        } else {
            self.run_pending(ctx, action);
        }
    }

    fn run_pending(&mut self, ctx: &egui::Context, action: Pending) {
        match action {
            Pending::New => {
                let mut doc = pdf_ops::new_document();
                match pdf_ops::save(&mut doc) {
                    Ok(bytes) => self.apply(
                        ctx,
                        Outcome::Document {
                            bytes,
                            path: Some(None),
                            dirty: false,
                            page_map: None,
                            message: "Создан новый пустой документ".into(),
                        },
                    ),
                    Err(e) => self.error("Ошибка", format!("{e:#}")),
                }
            }
            Pending::Open(path) => self.spawn(ctx, "Открытие…", move || {
                let (doc, bytes) = pdf_ops::load_file(&path)?;
                let message = format!("Открыт: {} — {} стр.", pdf_ops::file_name(&path), pdf_ops::page_count(&doc));
                Ok(Outcome::Document { bytes, path: Some(Some(path)), dirty: false, page_map: None, message })
            }),
            Pending::FromImages(paths) => self.spawn(ctx, "Создание PDF…", move || {
                let images = paths.iter().map(|p| images::prepare(p)).collect::<Result<Vec<_>>>()?;
                let mut doc = pdf_ops::images_to_document(images)?;
                Ok(Outcome::Document {
                    bytes: pdf_ops::save(&mut doc)?,
                    path: Some(None),
                    dirty: true,
                    page_map: None,
                    message: format!("Создан PDF из {} изображений", paths.len()),
                })
            }),
            Pending::Close => {
                self.allow_close = true;
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
        }
    }

    fn pick_pdf(title: &str) -> Option<PathBuf> {
        rfd::FileDialog::new()
            .set_title(title)
            .add_filter("PDF файлы", &["pdf", "PDF"])
            .add_filter("Все файлы", &["*"])
            .pick_file()
    }

    fn pick_images(title: &str) -> Option<Vec<PathBuf>> {
        rfd::FileDialog::new()
            .set_title(title)
            .add_filter("Изображения", &IMAGE_EXTENSIONS)
            .add_filter("Все файлы", &["*"])
            .pick_files()
    }

    fn open_dialog(&mut self, ctx: &egui::Context) {
        if let Some(path) = Self::pick_pdf("Открыть PDF") {
            self.request(ctx, "Открыть PDF", Pending::Open(path));
        }
    }

    fn save_dialog(&mut self, ctx: &egui::Context) {
        let Some(doc) = &self.doc else {
            return self.error("Нет документа", "Сначала откройте или создайте PDF.");
        };
        let mut dialog = rfd::FileDialog::new()
            .set_title("Сохранить PDF")
            .set_file_name(format!("{}.pdf", doc.stem()))
            .add_filter("PDF файлы", &["pdf"]);
        if let Some(dir) = doc.path.as_deref().and_then(Path::parent) {
            dialog = dialog.set_directory(dir);
        }
        let Some(mut path) = dialog.save_file() else { return };
        if path.extension().is_none() {
            path.set_extension("pdf");
        }
        let bytes = doc.bytes.clone();
        self.spawn(ctx, "Сохранение…", move || {
            std::fs::write(&path, bytes.as_slice())
                .map_err(|e| anyhow::anyhow!("Не удалось сохранить {}:\n{e}", path.display()))?;
            Ok(Outcome::Saved(path))
        });
    }

    /// Применяет к документу операцию над lopdf::Document в фоне.
    fn modify(
        &mut self,
        ctx: &egui::Context,
        label: &str,
        page_map: Option<Vec<Option<usize>>>,
        op: impl FnOnce(&mut lopdf::Document) -> Result<String> + Send + 'static,
    ) {
        let Some(doc) = &self.doc else {
            return self.error("Нет документа", "Сначала откройте или создайте PDF.");
        };
        let bytes = doc.bytes.clone();
        self.spawn(ctx, label, move || {
            let mut pdf = pdf_ops::load(&bytes, "документ")?;
            let message = op(&mut pdf)?;
            Ok(Outcome::Document { bytes: pdf_ops::save(&mut pdf)?, path: None, dirty: true, page_map, message })
        });
    }

    fn reorder(&mut self, ctx: &egui::Context, order: Vec<usize>) {
        if order.iter().enumerate().all(|(i, &o)| i == o) {
            return;
        }
        let map = order.iter().map(|&o| Some(o)).collect();
        self.modify(ctx, "Перестановка…", Some(map), move |pdf| {
            pdf_ops::reorder_pages(pdf, &order)?;
            Ok("Порядок страниц изменён".into())
        });
    }

    fn delete(&mut self, ctx: &egui::Context, pages: Vec<usize>) {
        let Some(doc) = &self.doc else { return };
        let set: BTreeSet<usize> = pages.iter().copied().collect();
        let map = (0..doc.page_count).filter(|i| !set.contains(i)).map(Some).collect();
        let count = set.len();
        self.modify(ctx, "Удаление…", Some(map), move |pdf| {
            pdf_ops::delete_pages(pdf, &set)?;
            Ok(format!("Удалено страниц: {count}"))
        });
    }

    fn ask_delete(&mut self) {
        let Some(doc) = &self.doc else {
            return self.error("Нет документа", "Сначала откройте PDF.");
        };
        if self.selected.is_empty() {
            return self.error("Не выбраны страницы", "Выберите страницы для удаления.");
        }
        if self.selected.len() >= doc.page_count {
            return self.error("Ошибка", "Нельзя удалить все страницы.");
        }
        self.dialog = Some(Dialog::ConfirmDelete(self.selected.iter().copied().collect()));
    }

    fn ask_insert(&mut self, ctx: &egui::Context, source: InsertSource) {
        match &self.doc {
            Some(doc) => {
                let max = doc.page_count;
                self.dialog = Some(Dialog::Insert { source, position: max, max });
            }
            None => match source {
                // Без документа изображения просто становятся новым документом
                InsertSource::Images(paths) => self.run_pending(ctx, Pending::FromImages(paths)),
                InsertSource::Pdf(path) => self.run_pending(ctx, Pending::Open(path)),
            },
        }
    }

    fn insert(&mut self, ctx: &egui::Context, source: InsertSource, after: usize) {
        let Some(doc) = &self.doc else { return };
        let total = doc.page_count;
        // Количество вставленных страниц станет известно только после загрузки,
        // поэтому миниатюры после вставки сопоставляются в фоне.
        let bytes = doc.bytes.clone();
        self.spawn(ctx, "Добавление страниц…", move || {
            let mut pdf = pdf_ops::load(&bytes, "документ")?;
            let (other, what) = match &source {
                InsertSource::Images(paths) => {
                    let images = paths.iter().map(|p| images::prepare(p)).collect::<Result<Vec<_>>>()?;
                    (pdf_ops::images_to_document(images)?, "изображений".to_owned())
                }
                InsertSource::Pdf(path) => {
                    let (other, _) = pdf_ops::load_file(path)?;
                    (other, format!("страниц из {}", pdf_ops::file_name(path)))
                }
            };
            let added = pdf_ops::insert_document(&mut pdf, other, after)?;
            let page_map = (0..total + added)
                .map(|i| match i {
                    i if i < after => Some(i),
                    i if i < after + added => None,
                    i => Some(i - added),
                })
                .collect();
            Ok(Outcome::Document {
                bytes: pdf_ops::save(&mut pdf)?,
                path: None,
                dirty: true,
                page_map: Some(page_map),
                message: format!("Добавлено {added} {what}"),
            })
        });
    }

    fn split(&mut self, ctx: &egui::Context, selected_only: bool) {
        let Some(doc) = &self.doc else {
            return self.error("Нет документа", "Сначала откройте или создайте PDF.");
        };
        if doc.page_count == 0 {
            return self.error("Пустой документ", "В документе нет страниц для разделения.");
        }
        let pages: Vec<usize> = if selected_only {
            if self.selected.is_empty() {
                return self.error(
                    "Не выбраны страницы",
                    "Выберите страницы в списке слева или используйте «Разделить все страницы».",
                );
            }
            self.selected.iter().copied().collect()
        } else {
            (0..doc.page_count).collect()
        };
        let mut dialog = rfd::FileDialog::new().set_title("Папка для сохранения страниц");
        if let Some(dir) = doc.path.as_deref().and_then(Path::parent) {
            dialog = dialog.set_directory(dir);
        }
        let Some(dir) = dialog.pick_folder() else { return };
        let (bytes, prefix) = (doc.bytes.clone(), doc.stem());
        self.spawn(ctx, "Разделение…", move || {
            let pdf = pdf_ops::load(&bytes, "документ")?;
            let created = pdf_ops::split_pages(&pdf, &pages, &dir, &prefix)?;
            Ok(Outcome::Info {
                title: "Готово".into(),
                text: format!("Создано файлов: {}\nПапка: {}", created.len(), dir.display()),
            })
        });
    }

    fn compress(&mut self, ctx: &egui::Context) {
        let Some(doc) = &self.doc else {
            return self.error("Нет документа", "Сначала откройте PDF.");
        };
        let (bytes, quality) = (doc.bytes.clone(), self.quality);
        self.spawn(ctx, "Сжатие…", move || {
            let (out, engine) = compress::compress(&bytes, quality)?;
            let (old, new) = (bytes.len() as f64, out.len() as f64);
            let change = (1.0 - new / old) * 100.0;
            let sign = if change > 0.0 { "−" } else { "+" };
            let message = format!(
                "Сжатие ({}): {:.0} КБ → {:.0} КБ ({sign}{:.0}%)",
                engine.name(),
                old / 1024.0,
                new / 1024.0,
                change.abs()
            );
            Ok(Outcome::Document { bytes: out, path: None, dirty: true, page_map: None, message })
        });
    }

    // ---------- Порядок страниц ----------

    fn move_selection(&mut self, ctx: &egui::Context, how: Move) {
        let Some(doc) = &self.doc else {
            return self.error("Нет документа", "Сначала откройте PDF.");
        };
        if self.selected.is_empty() {
            return self.error("Не выбраны страницы", "Выберите страницы для перемещения.");
        }
        if let Some(order) = move_order(doc.page_count, &self.selected, how) {
            self.reorder(ctx, order);
        }
    }

    fn click_page(&mut self, index: usize, modifiers: Modifiers) {
        if modifiers.shift
            && let Some(anchor) = self.anchor
        {
            let range = anchor.min(index)..=anchor.max(index);
            if !modifiers.command {
                self.selected.clear();
            }
            self.selected.extend(range);
            return;
        }
        if modifiers.command {
            if !self.selected.remove(&index) {
                self.selected.insert(index);
            }
        } else {
            self.selected = BTreeSet::from([index]);
        }
        self.anchor = Some(index);
    }

    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> =
            ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        if dropped.is_empty() || self.busy() || self.dialog.is_some() {
            return;
        }
        let (images, others): (Vec<PathBuf>, Vec<PathBuf>) =
            dropped.into_iter().partition(|p| images::is_image_path(p));
        if !images.is_empty() {
            self.ask_insert(ctx, InsertSource::Images(images));
        } else if let Some(pdf) = others.into_iter().next() {
            if self.doc.is_some() {
                self.ask_insert(ctx, InsertSource::Pdf(pdf));
            } else {
                self.run_pending(ctx, Pending::Open(pdf));
            }
        }
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        if self.busy() || self.dialog.is_some() {
            return;
        }
        if ctx.input_mut(|i| i.consume_shortcut(&SHORTCUT_NEW)) {
            self.request(ctx, "Новый документ", Pending::New);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&SHORTCUT_OPEN)) {
            self.open_dialog(ctx);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&SHORTCUT_SAVE)) {
            self.save_dialog(ctx);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&SHORTCUT_QUIT)) {
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
        if let Some(doc) = &self.doc {
            if ctx.input_mut(|i| i.consume_shortcut(&SHORTCUT_SELECT_ALL)) {
                self.selected = (0..doc.page_count).collect();
            }
            if !self.selected.is_empty() && ctx.input(|i| i.key_pressed(Key::Delete)) {
                self.ask_delete();
            }
        }
    }

    // ---------- Интерфейс ----------

    fn menu_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("Файл", |ui| {
                let item = |ui: &mut egui::Ui, text: &str, shortcut: &KeyboardShortcut| {
                    ui.add(egui::Button::new(text).shortcut_text(ctx.format_shortcut(shortcut)))
                        .clicked()
                };
                if item(ui, "Новый документ", &SHORTCUT_NEW) {
                    self.request(&ctx, "Новый документ", Pending::New);
                }
                if item(ui, "Открыть PDF…", &SHORTCUT_OPEN) {
                    self.open_dialog(&ctx);
                }
                if item(ui, "Сохранить как…", &SHORTCUT_SAVE) {
                    self.save_dialog(&ctx);
                }
                ui.separator();
                if item(ui, "Выход", &SHORTCUT_QUIT) {
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                }
            });
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if let Some(task) = &self.task {
                ui.spinner();
                ui.label(&task.label);
            } else if !self.selected.is_empty() {
                let pages: Vec<String> = self.selected.iter().map(|i| (i + 1).to_string()).collect();
                ui.label(format!("Выбрано страниц: {} — {}", pages.len(), pages.join(", ")));
                if !self.status.is_empty() {
                    ui.separator();
                }
                ui.label(RichText::new(&self.status).color(GRAY_TEXT));
            } else {
                ui.label(RichText::new(&self.status).color(GRAY_TEXT));
            }
        });
    }

    fn side_panel(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let full = |ui: &egui::Ui| [ui.available_width(), 32.0];

        ui.add_space(4.0);
        ui.label(RichText::new("Документ").strong().size(15.0));
        if ui.add_sized(full(ui), primary_button("Создать новый документ")).clicked() {
            self.request(&ctx, "Новый документ", Pending::New);
        }
        if ui.add_sized(full(ui), egui::Button::new("Открыть PDF…")).clicked() {
            self.open_dialog(&ctx);
        }
        if ui.add_sized(full(ui), primary_button("Сохранить как…")).clicked() {
            self.save_dialog(&ctx);
        }

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.tab, Tab::Create, "Создать PDF");
            ui.selectable_value(&mut self.tab, Tab::Pages, "Страницы");
            ui.selectable_value(&mut self.tab, Tab::Quality, "Качество");
        });
        ui.separator();

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match self.tab {
            Tab::Create => {
                ui.label("Выберите изображения и соберите из них PDF-документ.");
                ui.label(RichText::new("Поддерживаются: PNG, JPEG, BMP, GIF, TIFF, WebP.").color(GRAY_TEXT));
                if ui.add_sized(full(ui), egui::Button::new("Выбрать изображения…")).clicked()
                    && let Some(paths) = Self::pick_images("Выбрать изображения")
                {
                    self.request(&ctx, "Создать PDF", Pending::FromImages(paths));
                }
                ui.label(RichText::new("Изображения и PDF можно перетащить прямо в окно.").color(GRAY_TEXT));
            }
            Tab::Pages => {
                group(ui, "Порядок страниц", |ui| {
                    ui.label("Перетащите миниатюры слева или выберите страницы и нажмите кнопки под списком.");
                });
                group(ui, "Удаление страниц", |ui| {
                    ui.label("Выберите страницы в списке слева и нажмите «Удалить».");
                    let button = egui::Button::new(RichText::new("Удалить выбранные страницы").color(RED));
                    if ui.add_sized(full(ui), button).clicked() {
                        self.ask_delete();
                    }
                });
                group(ui, "Добавление страниц", |ui| {
                    if ui.add_sized(full(ui), egui::Button::new("Добавить изображения…")).clicked()
                        && let Some(paths) = Self::pick_images("Добавить изображения")
                    {
                        self.ask_insert(&ctx, InsertSource::Images(paths));
                    }
                    if ui.add_sized(full(ui), egui::Button::new("Добавить страницы из PDF…")).clicked() {
                        if self.doc.is_none() {
                            self.error("Нет документа", "Сначала откройте PDF.");
                        } else if let Some(path) = Self::pick_pdf("Выбрать PDF") {
                            self.ask_insert(&ctx, InsertSource::Pdf(path));
                        }
                    }
                });
                group(ui, "Разделение на страницы", |ui| {
                    ui.label("Каждая страница сохраняется как отдельный PDF-файл в выбранную папку.");
                    if ui.add_sized(full(ui), egui::Button::new("Разделить все страницы…")).clicked() {
                        self.split(&ctx, false);
                    }
                    if ui.add_sized(full(ui), egui::Button::new("Разделить выбранные страницы…")).clicked() {
                        self.split(&ctx, true);
                    }
                });
            }
            Tab::Quality => {
                ui.label(
                    "Измените качество PDF через Ghostscript (если установлен) или встроенный движок. \
                     Меньшее значение — меньший размер файла.",
                );
                let engine = if self.ghostscript {
                    "Движок: Ghostscript"
                } else {
                    "Движок: встроенный (установите Ghostscript для лучшего сжатия)"
                };
                ui.label(RichText::new(engine).color(GRAY_TEXT).small());
                ui.horizontal(|ui| {
                    ui.label("Качество:");
                    ui.add(egui::Slider::new(&mut self.quality, 1..=100));
                });
                ui.label(RichText::new(compress::preset_hint(self.quality, self.ghostscript)).color(GRAY_TEXT));
                if ui.add_sized(full(ui), egui::Button::new("Применить сжатие")).clicked() {
                    self.compress(&ctx);
                }
            }
        });
    }

    fn pages_panel(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.label(RichText::new("Страницы документа").strong().size(15.0));
        ui.label(RichText::new("Перетащите миниатюры, чтобы изменить порядок").color(GRAY_TEXT).small());
        if let Some(reason) = &self.renderer.unavailable {
            ui.label(RichText::new(format!("Внимание: {reason}")).color(RED).small());
        }

        let bottom_height = 80.0;
        let list_height = (ui.available_height() - bottom_height).max(100.0);
        let mut clicked: Option<(usize, Modifiers)> = None;
        let mut dropped: Option<(usize, usize)> = None;
        let mut clicked_empty = false;

        egui::Frame::group(ui.style()).fill(Color32::WHITE).show(ui, |ui| {
            ui.set_min_height(list_height);
            egui::ScrollArea::vertical().max_height(list_height).auto_shrink([false, false]).show(ui, |ui| {
                let Some(doc) = &self.doc else {
                    ui.centered_and_justified(|ui| ui.label(RichText::new("Документ не открыт").color(GRAY_TEXT)));
                    return;
                };
                if doc.page_count == 0 {
                    ui.centered_and_justified(|ui| {
                        ui.label(RichText::new("В документе нет страниц — добавьте изображения или страницы из PDF").color(GRAY_TEXT))
                    });
                    return;
                }
                let response = ui.horizontal_wrapped(|ui| {
                    for index in 0..doc.page_count {
                        let card = page_card(ui, index, doc.thumbs[index].as_ref(), self.selected.contains(&index));
                        if card.clicked {
                            clicked = Some((index, ui.input(|i| i.modifiers)));
                        }
                        if let Some(drop) = card.dropped {
                            dropped = Some(drop);
                        }
                    }
                });
                // Клик по пустому месту снимает выделение
                let empty = ui.interact(response.response.rect, ui.scope_id().with("empty"), Sense::click());
                clicked_empty = empty.clicked();
            });
        });

        if let Some((index, modifiers)) = clicked {
            self.click_page(index, modifiers);
        } else if clicked_empty {
            self.selected.clear();
        }
        if let Some((source, target)) = dropped
            && let Some(doc) = &self.doc
        {
            if !self.selected.contains(&source) {
                self.selected = BTreeSet::from([source]);
            }
            if let Some(order) = move_order(doc.page_count, &self.selected, Move::To(target)) {
                self.reorder(&ctx, order);
            }
        }

        ui.horizontal(|ui| {
            let buttons = [
                ("« В начало", Move::To(0)),
                ("↑ Выше", Move::Up),
                ("↓ Ниже", Move::Down),
                ("В конец »", Move::To(usize::MAX)),
            ];
            for (label, how) in buttons {
                if ui.button(label).clicked() {
                    self.move_selection(&ctx, how);
                }
            }
        });
        let info = match &self.doc {
            Some(doc) => format!(
                "{} — {} стр., {:.0} КБ",
                doc.display_name(),
                doc.page_count,
                doc.bytes.len() as f64 / 1024.0
            ),
            None => "Документ не открыт".into(),
        };
        ui.label(RichText::new(info).color(GRAY_TEXT));
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.dialog.take() else { return };
        // None — диалог остаётся открытым; Some(true) — подтверждено; Some(false) — отмена
        let mut answer: Option<bool> = None;

        let modal = egui::Modal::new(egui::Id::unique("pdf-tool-dialog")).show(ctx, |ui| {
            ui.set_max_width(420.0);
            match &mut dialog {
                Dialog::Discard { title, .. } => {
                    ui.heading(*title);
                    ui.label("Есть несохранённые изменения. Они будут потеряны.\nПродолжить?");
                    ui.horizontal(|ui| {
                        if ui.button("Да").clicked() {
                            answer = Some(true);
                        }
                        if ui.button("Нет").clicked() {
                            answer = Some(false);
                        }
                    });
                }
                Dialog::ConfirmDelete(pages) => {
                    ui.heading("Подтверждение");
                    let list: Vec<String> = pages.iter().map(|p| (p + 1).to_string()).collect();
                    ui.label(format!("Удалить страниц: {} ({})?", pages.len(), list.join(", ")));
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new("Удалить").color(RED)).clicked() {
                            answer = Some(true);
                        }
                        if ui.button("Отмена").clicked() {
                            answer = Some(false);
                        }
                    });
                }
                Dialog::Insert { position, max, .. } => {
                    ui.heading("Позиция вставки");
                    ui.horizontal(|ui| {
                        ui.label("Вставить после страницы:");
                        ui.add(egui::DragValue::new(position).range(0..=*max));
                    });
                    ui.label(RichText::new("0 = в начало, N = после страницы N").color(GRAY_TEXT).small());
                    ui.horizontal(|ui| {
                        if ui.add(primary_button("OK")).clicked() {
                            answer = Some(true);
                        }
                        if ui.button("Отмена").clicked() {
                            answer = Some(false);
                        }
                    });
                }
                Dialog::Message { title, text } => {
                    ui.heading(title.as_str());
                    ui.label(text.as_str());
                    if ui.button("OK").clicked() {
                        answer = Some(false);
                    }
                }
            }
        });
        if answer.is_none() && modal.should_close() {
            answer = Some(false);
        }

        match answer {
            None => self.dialog = Some(dialog),
            Some(false) => {}
            Some(true) => match dialog {
                Dialog::Discard { action, .. } => self.run_pending(ctx, action),
                Dialog::ConfirmDelete(pages) => self.delete(ctx, pages),
                Dialog::Insert { source, position, .. } => self.insert(ctx, source, position),
                Dialog::Message { .. } => {}
            },
        }
    }
}

impl eframe::App for PdfToolApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        self.poll_task(&ctx);
        self.receive_thumbs(&ctx);
        self.handle_dropped_files(&ctx);
        self.handle_shortcuts(&ctx);

        if ctx.input(|i| i.viewport().close_requested()) && !self.allow_close && self.is_dirty() {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            if self.dialog.is_none() {
                self.dialog = Some(Dialog::Discard { title: "Выход", action: Pending::Close });
            }
        }

        let busy = self.busy();
        egui::Panel::top("menu").show(ui, |ui| {
            ui.add_enabled_ui(!busy, |ui| self.menu_bar(ui));
        });
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        egui::Panel::right("tools")
            .resizable(true)
            .default_size(340.0)
            .min_size(300.0)
            .show(ui, |ui| {
                ui.add_enabled_ui(!busy, |ui| self.side_panel(ui));
            });
        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_enabled_ui(!busy, |ui| self.pages_panel(ui));
        });

        self.dialogs(&ctx);
        #[cfg(debug_assertions)]
        self.debug_screenshot(&ctx);
    }
}

#[cfg(debug_assertions)]
impl PdfToolApp {
    /// Отладка: PDF_TOOL_SCREENSHOT=файл.png — сохранить снимок окна и выйти.
    /// PDF_TOOL_SELECT=1,3 — выделить страницы, PDF_TOOL_TAB=pages|quality — открыть вкладку.
    fn debug_screenshot(&mut self, ctx: &egui::Context) {
        let Some(path) = std::env::var_os("PDF_TOOL_SCREENSHOT") else { return };
        self.frames += 1;
        if self.frames == 2
            && let Ok(list) = std::env::var("PDF_TOOL_SELECT")
        {
            self.selected = list.split(',').filter_map(|n| n.trim().parse::<usize>().ok()).map(|n| n - 1).collect();
        }
        if self.frames == 2 {
            match std::env::var("PDF_TOOL_TAB").as_deref() {
                Ok("pages") => self.tab = Tab::Pages,
                Ok("quality") => self.tab = Tab::Quality,
                _ => {}
            }
        }
        if self.frames == 90 {
            let close_ctx = ctx.clone();
            ctx.send_viewport_cmd(ViewportCommand::Screenshot(egui::ScreenshotCallback::new(move |img| {
                let [w, h] = img.size;
                let bytes = img.pixels.iter().flat_map(|c| c.to_array()).collect();
                if let Some(png) = image::RgbaImage::from_raw(w as u32, h as u32, bytes) {
                    let _ = png.save(&path);
                }
                close_ctx.send_viewport_cmd(ViewportCommand::Close);
            })));
        }
        ctx.request_repaint();
    }
}

fn primary_button(text: &str) -> egui::Button<'_> {
    egui::Button::new(RichText::new(text).color(Color32::WHITE)).fill(BLUE)
}

fn group(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style()).fill(Color32::WHITE).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(title).strong());
        add(ui);
    });
    ui.add_space(4.0);
}

struct CardResult {
    clicked: bool,
    /// (перетаскиваемая страница, позиция вставки перед страницей с этим индексом)
    dropped: Option<(usize, usize)>,
}

/// Карточка страницы: миниатюра, номер, выделение, перетаскивание.
fn page_card(ui: &mut egui::Ui, index: usize, thumb: Option<&egui::TextureHandle>, selected: bool) -> CardResult {
    let (rect, response) = ui.allocate_exact_size(CARD_SIZE, Sense::click_and_drag());
    response.dnd_set_drag_payload(index);
    let painter = ui.painter_at(rect.expand(4.0));
    let dragging_this = response.dragged();

    let (fill, stroke) = if selected {
        (SELECTED_FILL, Stroke::new(2.0, BLUE))
    } else if response.hovered() {
        (Color32::from_gray(245), Stroke::new(1.0, Color32::from_gray(200)))
    } else {
        (Color32::WHITE, Stroke::NONE)
    };
    painter.rect(rect, 4.0, fill, stroke, StrokeKind::Inside);

    let image_area = egui::Rect::from_min_size(rect.min + Vec2::new(8.0, 8.0), Vec2::new(rect.width() - 16.0, 150.0));
    let tint = if dragging_this { Color32::from_white_alpha(110) } else { Color32::WHITE };
    match thumb {
        Some(tex) => {
            let size = tex.size_vec2();
            let scale = (image_area.width() / size.x).min(image_area.height() / size.y);
            let img_rect = egui::Rect::from_center_size(image_area.center(), size * scale);
            painter.image(tex.id(), img_rect, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), tint);
            painter.rect_stroke(img_rect, 0.0, Stroke::new(1.0, Color32::from_gray(210)), StrokeKind::Outside);
        }
        None => {
            let img_rect = egui::Rect::from_center_size(image_area.center(), Vec2::new(100.0, 140.0));
            painter.rect(img_rect, 0.0, Color32::from_gray(240), Stroke::new(1.0, Color32::from_gray(210)), StrokeKind::Inside);
            painter.text(img_rect.center(), Align2::CENTER_CENTER, (index + 1).to_string(), FontId::proportional(28.0), Color32::from_gray(170));
        }
    }
    painter.text(
        egui::pos2(rect.center().x, rect.bottom() - 14.0),
        Align2::CENTER_CENTER,
        format!("Стр. {}", index + 1),
        FontId::proportional(13.0),
        if selected { BLUE } else { Color32::from_gray(60) },
    );

    // Индикатор места вставки при перетаскивании
    let pointer_before = |r: &egui::Response| {
        r.ctx.pointer_interact_pos().is_some_and(|p| p.x < rect.center().x)
    };
    if response.dnd_hover_payload::<usize>().is_some() {
        let x = if pointer_before(&response) { rect.left() - 3.0 } else { rect.right() + 3.0 };
        ui.painter().line_segment([egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())], Stroke::new(3.0, BLUE));
    }
    let dropped = response.dnd_release_payload::<usize>().map(|source| {
        let target = if pointer_before(&response) { index } else { index + 1 };
        (*source, target)
    });

    CardResult { clicked: response.clicked(), dropped }
}

#[derive(Clone, Copy)]
enum Move {
    Up,
    Down,
    /// Переместить выделенные перед страницей с этим индексом (usize::MAX — в конец).
    To(usize),
}

/// Новый порядок страниц (`order[new] = old`) или None, если ничего не меняется.
fn move_order(n: usize, selected: &BTreeSet<usize>, how: Move) -> Option<Vec<usize>> {
    let mut order: Vec<usize> = (0..n).collect();
    match how {
        Move::Up => {
            if selected.first().is_none_or(|&f| f == 0) {
                return None;
            }
            for &r in selected {
                order.swap(r - 1, r);
            }
        }
        Move::Down => {
            if selected.last().is_none_or(|&l| l + 1 >= n) {
                return None;
            }
            for &r in selected.iter().rev() {
                order.swap(r, r + 1);
            }
        }
        Move::To(target) => {
            let moving: Vec<usize> = selected.iter().copied().filter(|&i| i < n).collect();
            let rest: Vec<usize> = (0..n).filter(|i| !selected.contains(i)).collect();
            let at = rest.iter().filter(|&&i| i < target).count();
            order = rest[..at].iter().chain(&moving).chain(&rest[at..]).copied().collect();
        }
    }
    (order.iter().enumerate().any(|(i, &o)| i != o)).then_some(order)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn move_to_start_and_end() {
        let sel = BTreeSet::from([2, 3]);
        assert_eq!(move_order(5, &sel, Move::To(0)), Some(vec![2, 3, 0, 1, 4]));
        assert_eq!(move_order(5, &sel, Move::To(usize::MAX)), Some(vec![0, 1, 4, 2, 3]));
        assert_eq!(move_order(5, &sel, Move::To(2)), None);
    }

    #[test]
    fn drop_between_pages() {
        // Перетащить страницу 1 (индекс 0) за страницу 3 (перед индексом 3)
        assert_eq!(move_order(4, &BTreeSet::from([0]), Move::To(3)), Some(vec![1, 2, 0, 3]));
    }

    #[test]
    fn move_up_down() {
        let sel = BTreeSet::from([1, 3]);
        assert_eq!(move_order(4, &sel, Move::Up), Some(vec![1, 0, 3, 2]));
        assert_eq!(move_order(4, &sel, Move::Down), None);
        assert_eq!(move_order(4, &BTreeSet::from([0]), Move::Up), None);
    }
}
