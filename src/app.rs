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
use crate::i18n::{self, Lang, tr};
use crate::edit;
use crate::editor::{self, Editor, EditorAction};
use crate::images::{self, IMAGE_EXTENSIONS};
use crate::pdf_ops;
use crate::render::{Purpose, Renderer};

const THUMB_SIZE: u32 = 120;
/// Ширина отрисовки страницы в редакторе, пикселей.
const EDITOR_PAGE_WIDTH: u32 = 1600;
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
    /// Размеры страниц на экране в пунктах.
    page_sizes: Vec<(f32, f32)>,
}

impl DocState {
    fn display_name(&self) -> String {
        let name = self.path.as_deref().map(pdf_ops::file_name).unwrap_or_else(|| tr("Новый документ", "New document").into());
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
    CloseEditor,
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
    Text,
    Quality,
}

pub struct PdfToolApp {
    renderer: Renderer,
    doc: Option<DocState>,
    selected: BTreeSet<usize>,
    anchor: Option<usize>,
    tab: Tab,
    quality: u8,
    status: String,
    task: Option<Task>,
    dialog: Option<Dialog>,
    allow_close: bool,
    generation: u64,
    /// Открытый редактор страницы.
    editor: Option<Editor>,
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

        editor::register_fonts(ctx);
        let renderer = Renderer::start(ctx.clone());
        let mut app = PdfToolApp {
            renderer,
            doc: None,
            selected: BTreeSet::new(),
            anchor: None,
            tab: Tab::Create,
            quality: 50,
            status: String::new(),
            task: None,
            dialog: None,
            allow_close: false,
            generation: 0,
            editor: None,
            #[cfg(debug_assertions)]
            frames: 0,
        };
        if let Some(path) = open {
            app.run_pending(ctx, Pending::Open(path));
        }
        app
    }

    fn is_dirty(&self) -> bool {
        self.doc.as_ref().is_some_and(|d| d.dirty) || self.editor.as_ref().is_some_and(Editor::has_changes)
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
            Err(e) => {
                // Правки остаются в редакторе, их можно исправить и применить снова
                if let Some(editor) = &mut self.editor {
                    editor.applying = false;
                }
                self.error(tr("Ошибка", "Error"), format!("{e:#}"))
            }
        }
    }

    fn apply(&mut self, ctx: &egui::Context, outcome: Outcome) {
        match outcome {
            Outcome::Document { bytes, path, dirty, page_map, message } => {
                let (page_count, page_sizes) = match pdf_ops::load(&bytes, tr("документ", "document")) {
                    Ok(doc) => (pdf_ops::page_count(&doc), edit::page_sizes(&doc)),
                    Err(e) => return self.error(tr("Ошибка", "Error"), format!("{e:#}")),
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
                let doc = DocState { bytes: Arc::new(bytes), page_count, path, dirty, thumbs, page_sizes };
                self.selected = selected;
                self.anchor = None;
                self.generation += 1;
                let missing: Vec<usize> = (0..page_count).filter(|&i| doc.thumbs[i].is_none()).collect();
                self.renderer.request(Purpose::Thumb, self.generation, doc.bytes.clone(), missing, THUMB_SIZE);
                self.doc = Some(doc);
                // Документ сменился: редактор закрывается (после применения правок — тоже)
                self.editor = None;
                self.status = message;
                ctx.request_repaint();
            }
            Outcome::Saved(path) => {
                if let Some(doc) = &mut self.doc {
                    doc.dirty = false;
                    self.status = trf!("Сохранено: {}", "Saved: {}", path.display());
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
            if thumb.purpose == Purpose::Page {
                if let Some(editor) = &mut self.editor
                    && editor.page == thumb.index
                {
                    let name = format!("editor-{}-{}", thumb.generation, thumb.index);
                    editor.texture = Some(ctx.load_texture(name, thumb.image, egui::TextureOptions::LINEAR));
                    editor.texture_page = Some(thumb.index);
                }
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
                            message: tr("Создан новый пустой документ", "New empty document created").into(),
                        },
                    ),
                    Err(e) => self.error(tr("Ошибка", "Error"), format!("{e:#}")),
                }
            }
            Pending::Open(path) => self.spawn(ctx, tr("Открытие…", "Opening…"), move || {
                let (doc, bytes) = pdf_ops::load_file(&path)?;
                let message = trf!("Открыт: {} — {} стр.", "Opened: {} — {} pages", pdf_ops::file_name(&path), pdf_ops::page_count(&doc));
                Ok(Outcome::Document { bytes, path: Some(Some(path)), dirty: false, page_map: None, message })
            }),
            Pending::FromImages(paths) => self.spawn(ctx, tr("Создание PDF…", "Creating PDF…"), move || {
                let images = paths.iter().map(|p| images::prepare(p)).collect::<Result<Vec<_>>>()?;
                let mut doc = pdf_ops::images_to_document(images)?;
                Ok(Outcome::Document {
                    bytes: pdf_ops::save(&mut doc)?,
                    path: Some(None),
                    dirty: true,
                    page_map: None,
                    message: trf!("Создан PDF из {} изображений", "Created a PDF from {} images", paths.len()),
                })
            }),
            Pending::Close => {
                self.allow_close = true;
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
            Pending::CloseEditor => self.editor = None,
        }
    }

    // ---------- Редактор ----------

    fn open_editor(&mut self, page: usize) {
        let Some(doc) = &self.doc else {
            return self.error(tr("Нет документа", "No document"), tr("Сначала откройте PDF.", "Open a PDF first."));
        };
        if doc.page_count == 0 {
            return self.error(tr("Пустой документ", "Empty document"), tr("В документе нет страниц для редактирования.", "The document has no pages to edit."));
        }
        self.editor = Some(Editor::new(page.min(doc.page_count - 1), doc.page_count));
        self.request_editor_page();
    }

    fn request_editor_page(&mut self) {
        if let (Some(doc), Some(editor)) = (&self.doc, &self.editor) {
            self.renderer.request(Purpose::Page, self.generation, doc.bytes.clone(), vec![editor.page], EDITOR_PAGE_WIDTH);
        }
    }

    fn editor_action(&mut self, ctx: &egui::Context, action: EditorAction) {
        let Some(editor) = &mut self.editor else { return };
        match action {
            EditorAction::ShowPage(page) => {
                editor.show_page(page);
                self.request_editor_page();
            }
            EditorAction::Cancel => {
                if editor.has_changes() {
                    self.dialog = Some(Dialog::Discard { title: tr("Отменить правки", "Discard edits"), action: Pending::CloseEditor });
                } else {
                    self.editor = None;
                }
            }
            EditorAction::Apply => {
                if !editor.has_changes() {
                    self.editor = None;
                    return;
                }
                editor.applying = true;
                let texts = editor.texts.clone();
                let redactions = editor.redactions.clone();
                let erasures = editor.erasures.clone();
                let touched = edit::touched_pages(&texts, &redactions, &erasures);
                let count = self.doc.as_ref().map_or(0, |d| d.page_count);
                let map = (0..count).map(|i| (!touched.contains(&i)).then_some(i)).collect();
                self.modify(ctx, tr("Применение правок…", "Applying edits…"), Some(map), move |pdf| edit::apply_edits(pdf, &texts, &redactions, &erasures));
            }
        }
    }

    fn pick_pdf(title: &str) -> Option<PathBuf> {
        rfd::FileDialog::new()
            .set_title(title)
            .add_filter(tr("PDF файлы", "PDF files"), &["pdf", "PDF"])
            .add_filter(tr("Все файлы", "All files"), &["*"])
            .pick_file()
    }

    fn pick_images(title: &str) -> Option<Vec<PathBuf>> {
        rfd::FileDialog::new()
            .set_title(title)
            .add_filter(tr("Изображения", "Images"), &IMAGE_EXTENSIONS)
            .add_filter(tr("Все файлы", "All files"), &["*"])
            .pick_files()
    }

    fn open_dialog(&mut self, ctx: &egui::Context) {
        if let Some(path) = Self::pick_pdf(tr("Открыть PDF", "Open PDF")) {
            self.request(ctx, tr("Открыть PDF", "Open PDF"), Pending::Open(path));
        }
    }

    fn save_dialog(&mut self, ctx: &egui::Context) {
        let Some(doc) = &self.doc else {
            return self.error(tr("Нет документа", "No document"), tr("Сначала откройте или создайте PDF.", "Open or create a PDF first."));
        };
        let mut dialog = rfd::FileDialog::new()
            .set_title(tr("Сохранить PDF", "Save PDF"))
            .set_file_name(format!("{}.pdf", doc.stem()))
            .add_filter(tr("PDF файлы", "PDF files"), &["pdf"]);
        if let Some(dir) = doc.path.as_deref().and_then(Path::parent) {
            dialog = dialog.set_directory(dir);
        }
        let Some(mut path) = dialog.save_file() else { return };
        if path.extension().is_none() {
            path.set_extension("pdf");
        }
        let bytes = doc.bytes.clone();
        self.spawn(ctx, tr("Сохранение…", "Saving…"), move || {
            std::fs::write(&path, bytes.as_slice())
                .map_err(|e| anyhow::anyhow!(trf!("Не удалось сохранить {}:\n{e}", "Could not save {}:\n{e}", path.display())))?;
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
            return self.error(tr("Нет документа", "No document"), tr("Сначала откройте или создайте PDF.", "Open or create a PDF first."));
        };
        let bytes = doc.bytes.clone();
        self.spawn(ctx, label, move || {
            let mut pdf = pdf_ops::load(&bytes, tr("документ", "document"))?;
            let message = op(&mut pdf)?;
            Ok(Outcome::Document { bytes: pdf_ops::save(&mut pdf)?, path: None, dirty: true, page_map, message })
        });
    }

    fn reorder(&mut self, ctx: &egui::Context, order: Vec<usize>) {
        if order.iter().enumerate().all(|(i, &o)| i == o) {
            return;
        }
        let map = order.iter().map(|&o| Some(o)).collect();
        self.modify(ctx, tr("Перестановка…", "Reordering…"), Some(map), move |pdf| {
            pdf_ops::reorder_pages(pdf, &order)?;
            Ok(tr("Порядок страниц изменён", "Page order changed").into())
        });
    }

    fn delete(&mut self, ctx: &egui::Context, pages: Vec<usize>) {
        let Some(doc) = &self.doc else { return };
        let set: BTreeSet<usize> = pages.iter().copied().collect();
        let map = (0..doc.page_count).filter(|i| !set.contains(i)).map(Some).collect();
        let count = set.len();
        self.modify(ctx, tr("Удаление…", "Deleting…"), Some(map), move |pdf| {
            pdf_ops::delete_pages(pdf, &set)?;
            Ok(trf!("Удалено страниц: {count}", "Pages deleted: {count}"))
        });
    }

    fn ask_delete(&mut self) {
        let Some(doc) = &self.doc else {
            return self.error(tr("Нет документа", "No document"), tr("Сначала откройте PDF.", "Open a PDF first."));
        };
        if self.selected.is_empty() {
            return self.error(tr("Не выбраны страницы", "No pages selected"), tr("Выберите страницы для удаления.", "Select pages to delete."));
        }
        if self.selected.len() >= doc.page_count {
            return self.error(tr("Ошибка", "Error"), tr("Нельзя удалить все страницы.", "You cannot delete all pages."));
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
        self.spawn(ctx, tr("Добавление страниц…", "Adding pages…"), move || {
            let mut pdf = pdf_ops::load(&bytes, tr("документ", "document"))?;
            let (other, what) = match &source {
                InsertSource::Images(paths) => {
                    let images = paths.iter().map(|p| images::prepare(p)).collect::<Result<Vec<_>>>()?;
                    (pdf_ops::images_to_document(images)?, tr("изображений", "images").to_owned())
                }
                InsertSource::Pdf(path) => {
                    let (other, _) = pdf_ops::load_file(path)?;
                    (other, trf!("страниц из {}", "pages from {}", pdf_ops::file_name(path)))
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
                message: trf!("Добавлено {added} {what}", "Added {added} {what}"),
            })
        });
    }

    fn split(&mut self, ctx: &egui::Context, selected_only: bool) {
        let Some(doc) = &self.doc else {
            return self.error(tr("Нет документа", "No document"), tr("Сначала откройте или создайте PDF.", "Open or create a PDF first."));
        };
        if doc.page_count == 0 {
            return self.error(tr("Пустой документ", "Empty document"), tr("В документе нет страниц для разделения.", "The document has no pages to split."));
        }
        let pages: Vec<usize> = if selected_only {
            if self.selected.is_empty() {
                return self.error(
                    tr("Не выбраны страницы", "No pages selected"),
                    tr("Выберите страницы в списке слева или используйте «Разделить все страницы».", "Select pages in the list on the left or use “Split all pages”."),
                );
            }
            self.selected.iter().copied().collect()
        } else {
            (0..doc.page_count).collect()
        };
        let mut dialog = rfd::FileDialog::new().set_title(tr("Папка для сохранения страниц", "Folder for the pages"));
        if let Some(dir) = doc.path.as_deref().and_then(Path::parent) {
            dialog = dialog.set_directory(dir);
        }
        let Some(dir) = dialog.pick_folder() else { return };
        let (bytes, prefix) = (doc.bytes.clone(), doc.stem());
        self.spawn(ctx, tr("Разделение…", "Splitting…"), move || {
            let pdf = pdf_ops::load(&bytes, tr("документ", "document"))?;
            let created = pdf_ops::split_pages(&pdf, &pages, &dir, &prefix)?;
            Ok(Outcome::Info {
                title: tr("Готово", "Done").into(),
                text: trf!("Создано файлов: {}\nПапка: {}", "Files created: {}\nFolder: {}", created.len(), dir.display()),
            })
        });
    }

    fn compress(&mut self, ctx: &egui::Context) {
        let Some(doc) = &self.doc else {
            return self.error(tr("Нет документа", "No document"), tr("Сначала откройте PDF.", "Open a PDF first."));
        };
        let (bytes, quality) = (doc.bytes.clone(), self.quality);
        self.spawn(ctx, tr("Сжатие…", "Compressing…"), move || {
            let old = bytes.len() as f64 / 1024.0;
            let Some(out) = compress::compress(&bytes, quality)? else {
                return Ok(Outcome::Info {
                    title: tr("Размер не уменьшился", "Size not reduced").into(),
                    text: trf!(
                        "Сжатие не уменьшило файл ({old:.0} КБ), документ оставлен без изменений.\n\n\
                         Обычно так бывает, если в PDF почти нет изображений: текст и шрифты уже сжаты. \
                         Для документов с фото и сканами попробуйте качество ниже.",
                        "Compression did not make the file smaller ({old:.0} KB), so the document was left unchanged.\n\n\
                         This usually happens when the PDF has almost no images: text and fonts are already compressed. \
                         For documents with photos and scans, try a lower quality."
                    ),
                });
            };
            let new = out.len() as f64 / 1024.0;
            let message = trf!(
                "Сжатие: {old:.0} КБ → {new:.0} КБ (−{:.0}%)",
                "Compressed: {old:.0} KB → {new:.0} KB (−{:.0}%)",
                (1.0 - new / old) * 100.0
            );
            Ok(Outcome::Document { bytes: out, path: None, dirty: true, page_map: None, message })
        });
    }

    // ---------- Порядок страниц ----------

    fn move_selection(&mut self, ctx: &egui::Context, how: Move) {
        let Some(doc) = &self.doc else {
            return self.error(tr("Нет документа", "No document"), tr("Сначала откройте PDF.", "Open a PDF first."));
        };
        if self.selected.is_empty() {
            return self.error(tr("Не выбраны страницы", "No pages selected"), tr("Выберите страницы для перемещения.", "Select pages to move."));
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
        if dropped.is_empty() || self.busy() || self.dialog.is_some() || self.editor.is_some() {
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
        if self.editor.is_some() {
            if ctx.input_mut(|i| i.consume_shortcut(&SHORTCUT_QUIT)) {
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
            return;
        }
        if ctx.input_mut(|i| i.consume_shortcut(&SHORTCUT_NEW)) {
            self.request(ctx, tr("Новый документ", "New document"), Pending::New);
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
            ui.menu_button(tr("Файл", "File"), |ui| {
                let item = |ui: &mut egui::Ui, text: &str, shortcut: &KeyboardShortcut| {
                    ui.add(egui::Button::new(text).shortcut_text(ctx.format_shortcut(shortcut)))
                        .clicked()
                };
                if item(ui, tr("Новый документ", "New document"), &SHORTCUT_NEW) {
                    self.request(&ctx, tr("Новый документ", "New document"), Pending::New);
                }
                if item(ui, tr("Открыть PDF…", "Open PDF…"), &SHORTCUT_OPEN) {
                    self.open_dialog(&ctx);
                }
                if item(ui, tr("Сохранить как…", "Save as…"), &SHORTCUT_SAVE) {
                    self.save_dialog(&ctx);
                }
                ui.separator();
                if item(ui, tr("Выход", "Quit"), &SHORTCUT_QUIT) {
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
                ui.label(trf!("Выбрано страниц: {} — {}", "Selected pages: {} — {}", pages.len(), pages.join(", ")));
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
        ui.label(RichText::new(tr("Документ", "Document")).strong().size(15.0));
        if ui.add_sized(full(ui), primary_button(tr("Создать новый документ", "Create new document"))).clicked() {
            self.request(&ctx, tr("Новый документ", "New document"), Pending::New);
        }
        if ui.add_sized(full(ui), egui::Button::new(tr("Открыть PDF…", "Open PDF…"))).clicked() {
            self.open_dialog(&ctx);
        }
        if ui.add_sized(full(ui), primary_button(tr("Сохранить как…", "Save as…"))).clicked() {
            self.save_dialog(&ctx);
        }

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.tab, Tab::Create, tr("Создать PDF", "Create PDF"));
            ui.selectable_value(&mut self.tab, Tab::Pages, tr("Страницы", "Pages"));
            ui.selectable_value(&mut self.tab, Tab::Text, tr("Текст", "Text"));
            ui.selectable_value(&mut self.tab, Tab::Quality, tr("Качество", "Quality"));
        });
        ui.separator();

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match self.tab {
            Tab::Create => {
                ui.label(tr("Выберите изображения и соберите из них PDF-документ.", "Choose images and build a PDF document from them."));
                ui.label(RichText::new(tr("Поддерживаются: PNG, JPEG, BMP, GIF, TIFF, WebP.", "Supported: PNG, JPEG, BMP, GIF, TIFF, WebP.")).color(GRAY_TEXT));
                if ui.add_sized(full(ui), egui::Button::new(tr("Выбрать изображения…", "Choose images…"))).clicked()
                    && let Some(paths) = Self::pick_images(tr("Выбрать изображения", "Choose images"))
                {
                    self.request(&ctx, tr("Создать PDF", "Create PDF"), Pending::FromImages(paths));
                }
                ui.label(RichText::new(tr("Изображения и PDF можно перетащить прямо в окно.", "You can also drop images and PDFs onto the window.")).color(GRAY_TEXT));
            }
            Tab::Pages => {
                group(ui, tr("Порядок страниц", "Page order"), |ui| {
                    ui.label(tr("Перетащите миниатюры слева или выберите страницы и нажмите кнопки под списком.", "Drag thumbnails on the left, or select pages and use the buttons below the list."));
                });
                group(ui, tr("Удаление страниц", "Delete pages"), |ui| {
                    ui.label(tr("Выберите страницы в списке слева и нажмите «Удалить».", "Select pages in the list on the left and press “Delete”."));
                    let button = egui::Button::new(RichText::new(tr("Удалить выбранные страницы", "Delete selected pages")).color(RED));
                    if ui.add_sized(full(ui), button).clicked() {
                        self.ask_delete();
                    }
                });
                group(ui, tr("Добавление страниц", "Add pages"), |ui| {
                    if ui.add_sized(full(ui), egui::Button::new(tr("Добавить изображения…", "Add images…"))).clicked()
                        && let Some(paths) = Self::pick_images(tr("Добавить изображения", "Add images"))
                    {
                        self.ask_insert(&ctx, InsertSource::Images(paths));
                    }
                    if ui.add_sized(full(ui), egui::Button::new(tr("Добавить страницы из PDF…", "Add pages from PDF…"))).clicked() {
                        if self.doc.is_none() {
                            self.error(tr("Нет документа", "No document"), tr("Сначала откройте PDF.", "Open a PDF first."));
                        } else if let Some(path) = Self::pick_pdf(tr("Выбрать PDF", "Choose PDF")) {
                            self.ask_insert(&ctx, InsertSource::Pdf(path));
                        }
                    }
                });
                group(ui, tr("Разделение на страницы", "Split into pages"), |ui| {
                    ui.label(tr("Каждая страница сохраняется как отдельный PDF-файл в выбранную папку.", "Each page is saved as a separate PDF file in the chosen folder."));
                    if ui.add_sized(full(ui), egui::Button::new(tr("Разделить все страницы…", "Split all pages…"))).clicked() {
                        self.split(&ctx, false);
                    }
                    if ui.add_sized(full(ui), egui::Button::new(tr("Разделить выбранные страницы…", "Split selected pages…"))).clicked() {
                        self.split(&ctx, true);
                    }
                });
            }
            Tab::Text => {
                ui.label(tr("Добавляйте надписи в любом месте страницы и удаляйте существующий текст в выделенной области.", "Add text anywhere on a page and remove existing text inside a selected area."));
                ui.label(
                    RichText::new(tr("Удалённый текст вырезается из файла: его нельзя найти поиском или скопировать.", "Removed text is cut out of the file: it can no longer be found or copied."))
                        .color(GRAY_TEXT),
                );
                if ui.add_sized(full(ui), primary_button(tr("Открыть редактор страницы", "Open page editor"))).clicked() {
                    let page = self.selected.first().copied().unwrap_or(0);
                    self.open_editor(page);
                }
                ui.label(
                    RichText::new(tr("Также: двойной щелчок по странице или «Редактировать текст» в меню правого щелчка.", "Also: double-click a page, or choose “Edit text” in the right-click menu."))
                        .color(GRAY_TEXT)
                        .small(),
                );
            }
            Tab::Quality => {
                ui.label(tr(
                    "Сжатие уменьшает изображения до нужного разрешения и пересжимает их. \
                     Меньшее значение — меньший размер файла. Текст не меняется.",
                    "Compression downsamples images to the needed resolution and re-encodes them. \
                     A lower value gives a smaller file. Text is not changed.",
                ));
                ui.horizontal(|ui| {
                    ui.label(tr("Качество:", "Quality:"));
                    ui.add(egui::Slider::new(&mut self.quality, 1..=100));
                });
                ui.label(RichText::new(compress::preset_hint(self.quality)).color(GRAY_TEXT));
                if ui.add_sized(full(ui), egui::Button::new(tr("Применить сжатие", "Apply compression"))).clicked() {
                    self.compress(&ctx);
                }
            }
        });
    }

    fn pages_panel(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.label(RichText::new(tr("Страницы документа", "Document pages")).strong().size(15.0));
        ui.label(
            RichText::new(tr("Щелчок — выделить (Ctrl/Shift — несколько), перетаскивание — порядок, правый щелчок — меню", "Click to select (Ctrl/Shift for several), drag to reorder, right-click for menu"))
                .color(GRAY_TEXT)
                .small(),
        );
        if let Some(reason) = &self.renderer.unavailable {
            ui.label(RichText::new(trf!("Внимание: {reason}", "Warning: {reason}")).color(RED).small());
        }

        let bottom_height = 80.0;
        let list_height = (ui.available_height() - bottom_height).max(100.0);
        let mut clicked: Option<(usize, Modifiers)> = None;
        let mut dropped: Option<(usize, usize)> = None;
        let mut menu: Option<(usize, CardAction)> = None;
        let mut pointer_on_card = false;

        let frame = egui::Frame::group(ui.style()).fill(Color32::WHITE).show(ui, |ui| {
            ui.set_min_height(list_height);
            egui::ScrollArea::vertical().max_height(list_height).auto_shrink([false, false]).show(ui, |ui| {
                let Some(doc) = &self.doc else {
                    ui.centered_and_justified(|ui| ui.label(RichText::new(tr("Документ не открыт", "No document open")).color(GRAY_TEXT)));
                    return;
                };
                if doc.page_count == 0 {
                    ui.centered_and_justified(|ui| {
                        ui.label(RichText::new(tr("В документе нет страниц — добавьте изображения или страницы из PDF", "The document has no pages — add images or pages from a PDF")).color(GRAY_TEXT))
                    });
                    return;
                }
                ui.horizontal_wrapped(|ui| {
                    for index in 0..doc.page_count {
                        let card = page_card(ui, index, doc.thumbs[index].as_ref(), self.selected.contains(&index));
                        pointer_on_card |= card.pointer_on;
                        if card.clicked {
                            clicked = Some((index, ui.input(|i| i.modifiers)));
                        }
                        if let Some(drop) = card.dropped {
                            dropped = Some(drop);
                        }
                        if let Some(action) = card.action {
                            menu = Some((index, action));
                        }
                    }
                });
            });
        });

        if let Some((index, modifiers)) = clicked {
            self.click_page(index, modifiers);
        } else if !pointer_on_card
            && ui.input(|i| i.pointer.primary_clicked())
            && frame.response.contains_pointer()
        {
            // Клик по пустому месту списка снимает выделение
            self.selected.clear();
        }
        if let Some((index, action)) = menu {
            // Действие из меню относится к выделению, если страница в нём, иначе к ней одной
            if !self.selected.contains(&index) {
                self.selected = BTreeSet::from([index]);
                self.anchor = Some(index);
            }
            match action {
                CardAction::Delete => self.ask_delete(),
                CardAction::ToStart => self.move_selection(&ctx, Move::To(0)),
                CardAction::ToEnd => self.move_selection(&ctx, Move::To(usize::MAX)),
                CardAction::Edit => self.open_editor(index),
            }
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
                (tr("« В начало", "« To start"), Move::To(0)),
                (tr("↑ Выше", "↑ Up"), Move::Up),
                (tr("↓ Ниже", "↓ Down"), Move::Down),
                (tr("В конец »", "To end »"), Move::To(usize::MAX)),
            ];
            for (label, how) in buttons {
                if ui.button(label).clicked() {
                    self.move_selection(&ctx, how);
                }
            }
        });
        let info = match &self.doc {
            Some(doc) => trf!(
                "{} — {} стр., {:.0} КБ",
                "{} — {} pages, {:.0} KB",
                doc.display_name(),
                doc.page_count,
                doc.bytes.len() as f64 / 1024.0
            ),
            None => tr("Документ не открыт", "No document open").into(),
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
                    ui.label(tr("Есть несохранённые изменения. Они будут потеряны.\nПродолжить?", "There are unsaved changes. They will be lost.\nContinue?"));
                    ui.horizontal(|ui| {
                        if ui.button(tr("Да", "Yes")).clicked() {
                            answer = Some(true);
                        }
                        if ui.button(tr("Нет", "No")).clicked() {
                            answer = Some(false);
                        }
                    });
                }
                Dialog::ConfirmDelete(pages) => {
                    ui.heading(tr("Подтверждение", "Confirm"));
                    let list: Vec<String> = pages.iter().map(|p| (p + 1).to_string()).collect();
                    ui.label(trf!("Удалить страниц: {} ({})?", "Delete {} page(s) ({})?", pages.len(), list.join(", ")));
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new(tr("Удалить", "Delete")).color(RED)).clicked() {
                            answer = Some(true);
                        }
                        if ui.button(tr("Отмена", "Cancel")).clicked() {
                            answer = Some(false);
                        }
                    });
                }
                Dialog::Insert { position, max, .. } => {
                    ui.heading(tr("Позиция вставки", "Insert position"));
                    ui.horizontal(|ui| {
                        ui.label(tr("Вставить после страницы:", "Insert after page:"));
                        ui.add(egui::DragValue::new(position).range(0..=*max));
                    });
                    ui.label(RichText::new(tr("0 = в начало, N = после страницы N", "0 = at the beginning, N = after page N")).color(GRAY_TEXT).small());
                    ui.horizontal(|ui| {
                        if ui.add(primary_button("OK")).clicked() {
                            answer = Some(true);
                        }
                        if ui.button(tr("Отмена", "Cancel")).clicked() {
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
    /// Отладка: PDF_TOOL_CLICKS="x,y;x,y,ctrl;x,y,right;x,y,double;type:текст;drag:x1,y1,x2,y2" —
    /// синтетический ввод, по шагу каждые 8 кадров.
    #[cfg(debug_assertions)]
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        use egui::{Event, PointerButton};
        let Ok(script) = std::env::var("PDF_TOOL_CLICKS") else { return };
        for (k, item) in script.split(';').enumerate() {
            let step = self.frames.wrapping_sub(20 + k as u32 * 8);
            if step > 7 {
                continue;
            }
            if let Some(text) = item.strip_prefix("type:") {
                if step == 0 {
                    raw.events.push(Event::Text(text.replace("\\n", "\n")));
                }
                continue;
            }
            let (is_drag, rest) = match item.strip_prefix("drag:") {
                Some(rest) => (true, rest),
                None => (false, item),
            };
            let parts: Vec<&str> = rest.split(',').map(str::trim).collect();
            let nums: Vec<f32> = parts.iter().filter_map(|v| v.parse().ok()).collect();
            if nums.len() < 2 {
                continue;
            }
            let pos = egui::pos2(nums[0], nums[1]);
            let button = |pos, pressed, modifiers| Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers };
            if is_drag && nums.len() == 4 {
                let end = egui::pos2(nums[2], nums[3]);
                match step {
                    0 => raw.events.push(Event::PointerMoved(pos)),
                    1 => raw.events.push(button(pos, true, Modifiers::NONE)),
                    2 => raw.events.push(Event::PointerMoved(pos.lerp(end, 0.5))),
                    3 => raw.events.push(Event::PointerMoved(end)),
                    4 => raw.events.push(button(end, false, Modifiers::NONE)),
                    _ => {}
                }
                continue;
            }
            let extra = parts.get(2).copied().unwrap_or("");
            let modifiers = match extra {
                "shift" => Modifiers::SHIFT,
                "ctrl" => Modifiers::COMMAND,
                _ => Modifiers::NONE,
            };
            let pointer = if extra == "right" { PointerButton::Secondary } else { PointerButton::Primary };
            let click = |pressed| Event::PointerButton { pos, button: pointer, pressed, modifiers };
            match step {
                0 => {
                    raw.events.push(Event::ModifiersChanged(modifiers));
                    raw.events.push(Event::PointerMoved(pos));
                }
                1 => raw.events.push(click(true)),
                2 => raw.events.push(click(false)),
                3 if extra == "double" => raw.events.push(click(true)),
                4 if extra == "double" => raw.events.push(click(false)),
                _ => {}
            }
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        self.poll_task(&ctx);
        self.receive_thumbs(&ctx);
        self.handle_dropped_files(&ctx);
        self.handle_shortcuts(&ctx);

        if ctx.input(|i| i.viewport().close_requested()) && !self.allow_close && self.is_dirty() {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            if self.dialog.is_none() {
                self.dialog = Some(Dialog::Discard { title: tr("Выход", "Quit"), action: Pending::Close });
            }
        }

        let busy = self.busy();
        let editing = self.editor.is_some();
        egui::Panel::top("menu").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add_enabled_ui(!busy && !editing, |ui| self.menu_bar(ui));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), language_switch);
            });
        });
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        let mut editor_action = None;
        egui::Panel::right("tools")
            .resizable(true)
            .default_size(340.0)
            .min_size(300.0)
            .show(ui, |ui| {
                ui.add_enabled_ui(!busy, |ui| match &mut self.editor {
                    Some(editor) => editor_action = editor.side_ui(ui),
                    None => self.side_panel(ui),
                });
            });
        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_enabled_ui(!busy, |ui| {
                let page_size = |page: usize| self.doc.as_ref().and_then(|d| d.page_sizes.get(page).copied());
                match self.editor.as_ref().map(|e| page_size(e.page)) {
                    Some(size) => {
                        let unavailable = self.renderer.unavailable.as_deref().map(|_| tr("Предпросмотр недоступен: не найдена библиотека PDFium", "Preview unavailable: PDFium library not found"));
                        let size = size.unwrap_or((595.0, 842.0));
                        if let Some(editor) = &mut self.editor {
                            editor.page_ui(ui, size, unavailable);
                        }
                    }
                    None => self.pages_panel(ui),
                }
            });
        });
        if let Some(action) = editor_action {
            self.editor_action(&ctx, action);
        }

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
        // Запрос повторяется: при активном поле ввода он иногда теряется
        static SAVED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if self.frames >= 90 && self.frames.is_multiple_of(30) && !SAVED.load(std::sync::atomic::Ordering::SeqCst) {
            let close_ctx = ctx.clone();
            ctx.send_viewport_cmd(ViewportCommand::Screenshot(egui::ScreenshotCallback::new(move |img| {
                if SAVED.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
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

/// Переключатель языка интерфейса (выбор запоминается).
fn language_switch(ui: &mut egui::Ui) {
    // Порядок справа налево
    for (lang, label, hint) in [(Lang::En, "EN", "English"), (Lang::Ru, "RU", "Русский")] {
        if ui.selectable_label(i18n::lang() == lang, label).on_hover_text(hint).clicked() {
            i18n::set_lang(lang);
        }
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

#[derive(Clone, Copy)]
enum CardAction {
    Edit,
    Delete,
    ToStart,
    ToEnd,
}

struct CardResult {
    clicked: bool,
    /// Указатель над карточкой (чтобы отличать клик по пустому месту)
    pointer_on: bool,
    /// Пункт контекстного меню
    action: Option<CardAction>,
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
        trf!("Стр. {}", "Page {}", index + 1),
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

    let mut action = None;
    response.context_menu(|ui| {
        if ui.button(tr("Редактировать текст…", "Edit text…")).clicked() {
            action = Some(CardAction::Edit);
        }
        if ui.button(tr("Удалить", "Delete")).clicked() {
            action = Some(CardAction::Delete);
        }
        ui.separator();
        if ui.button(tr("Переместить в начало", "Move to start")).clicked() {
            action = Some(CardAction::ToStart);
        }
        if ui.button(tr("Переместить в конец", "Move to end")).clicked() {
            action = Some(CardAction::ToEnd);
        }
    });

    if response.double_clicked() {
        action = Some(CardAction::Edit);
    }

    CardResult { clicked: response.clicked(), pointer_on: response.contains_pointer(), action, dropped }
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
