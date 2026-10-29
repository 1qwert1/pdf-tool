//! Редактор страницы: надписи (ввод прямо на странице, любой наклон),
//! удаление текста в области и ластик-закраска.
//!
//! Правки хранятся здесь, пока пользователь не нажмёт «Готово», — до этого
//! их можно двигать, менять, удалять и отменять (Ctrl+Z). Затем они записываются в PDF.

use eframe::egui::{
    self, Align2, Color32, FontFamily, FontId, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Vec2,
    epaint::TextShape,
    text::{LayoutJob, TextFormat},
};

use crate::edit::{self, Erase, EraseShape, Family, ITALIC_SHEAR, Redaction, TextItem};
use crate::i18n::tr;

const BLUE: Color32 = Color32::from_rgb(26, 115, 232);
const RED: Color32 = Color32::from_rgb(217, 48, 37);
const GRAY_TEXT: Color32 = Color32::from_rgb(95, 99, 104);
/// Сколько шагов отмены хранить.
const UNDO_LIMIT: usize = 100;
/// Расстояние ручки поворота над рамкой надписи, экранных пикселей.
const HANDLE_OFFSET: f32 = 22.0;

#[derive(PartialEq, Clone, Copy)]
pub enum Tool {
    Select,
    Text,
    Remove,
    Eraser,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Selection {
    Text(usize),
    Area(usize),
    Paint(usize),
}

pub enum EditorAction {
    Apply,
    Cancel,
    ShowPage(usize),
}

/// Настройки новой надписи (и выделенной надписи).
#[derive(Clone, Copy)]
struct Style {
    family: Family,
    bold: bool,
    italic: bool,
    size: f32,
    color: [u8; 3],
    angle: f32,
}

#[derive(Clone, Copy, PartialEq)]
enum EraserShape {
    Brush,
    Rect,
}

/// Что сейчас тянут мышью.
enum Drag {
    /// Рамка удаления текста или прямоугольник ластика от точки.
    Frame { from: Pos2 },
    /// Перемещение выделенного объекта; последняя точка указателя.
    Move { last: Pos2 },
    /// Поворот выделенной надписи ручкой.
    Rotate,
    /// Мазок кистью (последний элемент `erasures`).
    Brush,
}

type Snapshot = (Vec<TextItem>, Vec<Redaction>, Vec<Erase>);

pub struct Editor {
    pub page: usize,
    pub page_count: usize,
    tool: Tool,
    pub texts: Vec<TextItem>,
    pub redactions: Vec<Redaction>,
    pub erasures: Vec<Erase>,
    selection: Option<Selection>,
    /// Надпись, текст которой сейчас вводится прямо на странице.
    editing: Option<usize>,
    focus_text: bool,
    style: Style,
    fill_white: bool,
    eraser_shape: EraserShape,
    eraser_width: f32,
    eraser_color: [u8; 3],
    drag: Option<Drag>,
    undo: Vec<Snapshot>,
    pub texture: Option<egui::TextureHandle>,
    pub texture_page: Option<usize>,
    /// Правки записываются в файл; редактор закроется после успеха.
    pub applying: bool,
}

/// Регистрирует встроенные шрифты в egui для предпросмотра надписей.
pub fn register_fonts(ctx: &egui::Context) {
    for family in Family::ALL {
        for bold in [false, true] {
            let name = edit::egui_font_name(family, bold);
            ctx.add_font(egui::epaint::text::FontInsert::new(
                &name,
                egui::FontData::from_static(edit::font_data(family, bold)),
                vec![egui::epaint::text::InsertFontFamily {
                    family: FontFamily::Name(name.clone().into()),
                    priority: egui::epaint::text::FontPriority::Highest,
                }],
            ));
        }
    }
}

// ---------- Геометрия надписи (в пунктах страницы) ----------

fn v2(t: (f32, f32)) -> Vec2 {
    Vec2::new(t.0, t.1)
}

/// Ширина и высота блока текста без наклона.
fn text_size(t: &TextItem) -> Vec2 {
    let lines: Vec<&str> = if t.text.is_empty() { vec![""] } else { t.text.lines().collect() };
    let width = lines
        .iter()
        .map(|l| edit::text_width(t.family, t.bold, t.size, l))
        .fold(0.0, f32::max)
        .max(t.size * 2.0);
    let extra = if t.italic { t.size * ITALIC_SHEAR } else { 0.0 };
    let lines = lines.len().max(1) + usize::from(t.text.ends_with('\n'));
    Vec2::new(width + extra, lines as f32 * t.size * edit::line_height(t.family, t.bold))
}

/// Направления «вдоль текста» и «к следующей строке» на экране.
fn text_axes(t: &TextItem) -> (Vec2, Vec2) {
    let (d, n) = edit::axes(t.angle);
    (v2(d), v2(n))
}

/// Точка в координатах блока (вдоль текста, поперёк).
fn to_local(t: &TextItem, p: Pos2) -> Vec2 {
    let (d, n) = text_axes(t);
    let r = p - Pos2::new(t.x, t.y);
    Vec2::new(r.dot(d), r.dot(n))
}

fn text_contains(t: &TextItem, p: Pos2, pad: f32) -> bool {
    let local = to_local(t, p);
    let size = text_size(t);
    local.x >= -pad && local.y >= -pad && local.x <= size.x + pad && local.y <= size.y + pad
}

fn text_corners(t: &TextItem) -> [Pos2; 4] {
    let (d, n) = text_axes(t);
    let size = text_size(t);
    let a = Pos2::new(t.x, t.y);
    [a, a + d * size.x, a + d * size.x + n * size.y, a + n * size.y]
}

fn text_center(t: &TextItem) -> Pos2 {
    let (d, n) = text_axes(t);
    let size = text_size(t);
    Pos2::new(t.x, t.y) + d * (size.x / 2.0) + n * (size.y / 2.0)
}

/// Ручка поворота: над серединой верхней стороны.
fn rotate_handle(t: &TextItem, zoom: f32) -> Pos2 {
    let (d, n) = text_axes(t);
    let size = text_size(t);
    Pos2::new(t.x, t.y) + d * (size.x / 2.0) - n * (HANDLE_OFFSET / zoom)
}

/// Наклон с поворотом вокруг центра блока, с шагом 1°.
fn set_angle(t: &mut TextItem, angle: f32) {
    let center = text_center(t);
    let mut angle = angle.round();
    if angle > 180.0 {
        angle -= 360.0;
    }
    if angle <= -180.0 {
        angle += 360.0;
    }
    t.angle = angle;
    let (d, n) = text_axes(t);
    let size = text_size(t);
    let anchor = center - d * (size.x / 2.0) - n * (size.y / 2.0);
    t.x = anchor.x;
    t.y = anchor.y;
}

fn area_rect(r: &[f32; 4]) -> Rect {
    Rect::from_min_max(Pos2::new(r[0], r[1]), Pos2::new(r[2], r[3]))
}

fn erase_bounds(e: &Erase) -> Rect {
    match &e.shape {
        EraseShape::Rect(r) => area_rect(r),
        EraseShape::Brush { points, width } => {
            let mut rect = Rect::NOTHING;
            for p in points {
                rect.extend_with(Pos2::new(p[0], p[1]));
            }
            rect.expand(width / 2.0)
        }
    }
}

fn segment_distance(p: Pos2, a: Pos2, b: Pos2) -> f32 {
    let ab = b - a;
    let t = if ab.length_sq() > 0.0 { ((p - a).dot(ab) / ab.length_sq()).clamp(0.0, 1.0) } else { 0.0 };
    p.distance(a + ab * t)
}

fn erase_contains(e: &Erase, p: Pos2) -> bool {
    match &e.shape {
        EraseShape::Rect(r) => area_rect(r).contains(p),
        EraseShape::Brush { points, width } => {
            let pts: Vec<Pos2> = points.iter().map(|q| Pos2::new(q[0], q[1])).collect();
            let near = |a: Pos2, b: Pos2| segment_distance(p, a, b) <= width / 2.0 + 2.0;
            match pts.as_slice() {
                [single] => near(*single, *single),
                _ => pts.windows(2).any(|w| near(w[0], w[1])),
            }
        }
    }
}

fn rgb(c: [u8; 3]) -> Color32 {
    Color32::from_rgb(c[0], c[1], c[2])
}

fn text_format(t: &TextItem, zoom: f32) -> TextFormat {
    TextFormat {
        font_id: FontId::new(t.size * zoom, FontFamily::Name(edit::egui_font_name(t.family, t.bold).into())),
        color: rgb(t.color),
        italics: t.italic,
        ..Default::default()
    }
}

impl Editor {
    pub fn new(page: usize, page_count: usize) -> Self {
        Editor {
            page,
            page_count,
            tool: Tool::Text,
            texts: Vec::new(),
            redactions: Vec::new(),
            erasures: Vec::new(),
            selection: None,
            editing: None,
            focus_text: false,
            style: Style { family: Family::Sans, bold: false, italic: false, size: 14.0, color: [0, 0, 0], angle: 0.0 },
            fill_white: false,
            eraser_shape: EraserShape::Brush,
            eraser_width: 12.0,
            eraser_color: [255, 255, 255],
            drag: None,
            undo: Vec::new(),
            texture: None,
            texture_page: None,
            applying: false,
        }
    }

    pub fn has_changes(&self) -> bool {
        self.texts.iter().any(|t| !t.text.trim().is_empty()) || !self.redactions.is_empty() || !self.erasures.is_empty()
    }

    /// Запоминает состояние перед изменением (для Ctrl+Z).
    fn checkpoint(&mut self) {
        self.undo.push((self.texts.clone(), self.redactions.clone(), self.erasures.clone()));
        if self.undo.len() > UNDO_LIMIT {
            self.undo.remove(0);
        }
    }

    fn undo(&mut self) {
        if let Some((texts, redactions, erasures)) = self.undo.pop() {
            self.texts = texts;
            self.redactions = redactions;
            self.erasures = erasures;
            self.selection = None;
            self.editing = None;
            self.drag = None;
        }
    }

    pub fn delete_selected(&mut self) {
        let Some(selection) = self.selection else { return };
        self.checkpoint();
        match selection {
            Selection::Text(i) if i < self.texts.len() => {
                self.texts.remove(i);
            }
            Selection::Area(i) if i < self.redactions.len() => {
                self.redactions.remove(i);
            }
            Selection::Paint(i) if i < self.erasures.len() => {
                self.erasures.remove(i);
            }
            _ => {}
        }
        self.selection = None;
        self.editing = None;
    }

    /// Пустые надписи, с которых сняли выделение, не нужны.
    fn drop_empty_texts(&mut self) {
        let selected = match self.selection {
            Some(Selection::Text(i)) => Some(i),
            _ => None,
        };
        let mut index = 0;
        let mut new_selected = selected;
        self.texts.retain(|t| {
            let keep = Some(index) == selected || !t.text.trim().is_empty();
            if !keep && selected.is_some_and(|s| s > index) {
                new_selected = new_selected.map(|s| s - 1);
            }
            index += 1;
            keep
        });
        if let Some(i) = new_selected {
            self.selection = Some(Selection::Text(i));
        }
    }

    fn select(&mut self, selection: Option<Selection>) {
        if self.editing.is_some() && selection != self.editing.map(Selection::Text) {
            self.editing = None;
        }
        self.selection = selection;
        if let Some(Selection::Text(i)) = selection
            && let Some(t) = self.texts.get(i)
        {
            self.style = Style {
                family: t.family,
                bold: t.bold,
                italic: t.italic,
                size: t.size,
                color: t.color,
                angle: t.angle,
            };
        }
        self.drop_empty_texts();
    }

    fn start_editing(&mut self, i: usize) {
        self.select(Some(Selection::Text(i)));
        if let Some(Selection::Text(i)) = self.selection {
            self.editing = Some(i);
            self.focus_text = true;
        }
    }

    /// Объект под точкой: надписи поверх закраски, закраска поверх областей удаления.
    fn hit(&self, pos: Pos2) -> Option<Selection> {
        let page = self.page;
        let text = self.texts.iter().enumerate().rev().find(|(_, t)| t.page == page && text_contains(t, pos, 3.0));
        if let Some((i, _)) = text {
            return Some(Selection::Text(i));
        }
        let paint = self.erasures.iter().enumerate().rev().find(|(_, e)| e.page == page && erase_contains(e, pos));
        if let Some((i, _)) = paint {
            return Some(Selection::Paint(i));
        }
        self.redactions
            .iter()
            .enumerate()
            .rev()
            .find(|(_, r)| r.page == page && area_rect(&r.rect).contains(pos))
            .map(|(i, _)| Selection::Area(i))
    }

    fn translate_selected(&mut self, delta: Vec2) {
        match self.selection {
            Some(Selection::Text(i)) => {
                self.texts[i].x += delta.x;
                self.texts[i].y += delta.y;
            }
            Some(Selection::Area(i)) => {
                let r = &mut self.redactions[i].rect;
                *r = [r[0] + delta.x, r[1] + delta.y, r[2] + delta.x, r[3] + delta.y];
            }
            Some(Selection::Paint(i)) => match &mut self.erasures[i].shape {
                EraseShape::Rect(r) => *r = [r[0] + delta.x, r[1] + delta.y, r[2] + delta.x, r[3] + delta.y],
                EraseShape::Brush { points, .. } => {
                    for p in points {
                        p[0] += delta.x;
                        p[1] += delta.y;
                    }
                }
            },
            None => {}
        }
    }

    // ---------- Боковая панель ----------

    pub fn side_ui(&mut self, ui: &mut egui::Ui) -> Option<EditorAction> {
        let mut action = None;
        let full = |ui: &egui::Ui| [ui.available_width(), 32.0];

        ui.add_space(4.0);
        ui.label(RichText::new(tr("Редактирование страницы", "Page editing")).strong().size(15.0));
        ui.horizontal(|ui| {
            if ui.add_enabled(self.page > 0, egui::Button::new(tr("‹ Пред.", "‹ Prev"))).clicked() {
                action = Some(EditorAction::ShowPage(self.page - 1));
            }
            ui.label(trf!("Страница {} из {}", "Page {} of {}", self.page + 1, self.page_count));
            if ui.add_enabled(self.page + 1 < self.page_count, egui::Button::new(tr("След. ›", "Next ›"))).clicked() {
                action = Some(EditorAction::ShowPage(self.page + 1));
            }
        });
        ui.separator();

        ui.horizontal_wrapped(|ui| {
            let tools = [
                (Tool::Select, tr("Выбор", "Select")),
                (Tool::Text, tr("Текст", "Text")),
                (Tool::Remove, tr("Удалить текст", "Remove text")),
                (Tool::Eraser, tr("Ластик", "Eraser")),
            ];
            for (tool, label) in tools {
                if ui.selectable_label(self.tool == tool, label).clicked() {
                    self.tool = tool;
                    self.editing = None;
                }
            }
        });
        let hint = match self.tool {
            Tool::Select => tr(
                "Щелчок — выделить, перетаскивание — сдвинуть, двойной щелчок по надписи — изменить текст. Круглая ручка над надписью поворачивает её.",
                "Click to select, drag to move, double-click a caption to edit its text. The round handle above a caption rotates it.",
            ),
            Tool::Text => tr(
                "Щёлкните на странице и печатайте прямо там. Щелчок по надписи — продолжить ввод.",
                "Click on the page and type right there. Click a caption to keep typing.",
            ),
            Tool::Remove => tr(
                "Обведите рамкой область: текст в ней будет удалён из файла.",
                "Drag a box around an area: the text inside will be removed from the file.",
            ),
            Tool::Eraser => tr(
                "Проведите кистью или растяните прямоугольник: область закрасится выбранным цветом.",
                "Paint with the brush or drag a rectangle: the area is painted over with the chosen color.",
            ),
        };
        ui.label(RichText::new(hint).color(GRAY_TEXT));

        egui::ScrollArea::vertical().auto_shrink([false, true]).max_height(ui.available_height() - 90.0).show(ui, |ui| {
            let selected_text = match self.selection {
                Some(Selection::Text(i)) if i < self.texts.len() => Some(i),
                _ => None,
            };
            if self.tool == Tool::Text || selected_text.is_some() {
                self.text_group(ui, selected_text);
            }
            if self.tool == Tool::Remove {
                group(ui, tr("Удаление текста", "Text removal"), |ui| {
                    ui.checkbox(&mut self.fill_white, tr("Закрасить область белым", "Paint the area white"));
                    ui.label(
                        RichText::new(tr(
                            "Скроет и картинки с графикой. Текст удаляется из файла в любом случае.",
                            "Also hides images and graphics. Text is removed from the file either way.",
                        ))
                        .color(GRAY_TEXT)
                        .small(),
                    );
                });
            }
            if self.tool == Tool::Eraser {
                self.eraser_group(ui);
            }
            if let Some(Selection::Area(i)) = self.selection
                && let Some(area) = self.redactions.get_mut(i)
            {
                let mut remove = false;
                group(ui, tr("Выделенная область", "Selected area"), |ui| {
                    ui.checkbox(&mut area.fill_white, tr("Закрасить белым", "Paint white"));
                    remove = ui.button(RichText::new(tr("Убрать область", "Remove area")).color(RED)).clicked();
                });
                if remove {
                    self.delete_selected();
                }
            }
            if let Some(Selection::Paint(i)) = self.selection
                && let Some(paint) = self.erasures.get_mut(i)
            {
                let mut remove = false;
                group(ui, tr("Выделенная закраска", "Selected paint"), |ui| {
                    ui.horizontal(|ui| {
                        ui.label(tr("Цвет:", "Color:"));
                        ui.color_edit_button_srgb(&mut paint.color);
                    });
                    remove = ui.button(RichText::new(tr("Убрать закраску", "Remove paint")).color(RED)).clicked();
                });
                if remove {
                    self.delete_selected();
                }
            }

            ui.add_space(4.0);
            if ui.add_enabled(!self.undo.is_empty(), egui::Button::new(tr("↶ Отменить (Ctrl+Z)", "↶ Undo (Ctrl+Z)"))).clicked() {
                self.undo();
            }
            let texts = self.texts.iter().filter(|t| !t.text.trim().is_empty()).count();
            if self.has_changes() {
                ui.label(
                    RichText::new(trf!(
                        "Несохранённые правки: надписей {texts}, областей удаления {}, закрасок {}",
                        "Unsaved edits: {texts} caption(s), {} removal area(s), {} paint(s)",
                        self.redactions.len(),
                        self.erasures.len()
                    ))
                    .color(GRAY_TEXT),
                );
            }
        });

        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            if ui.add_sized(full(ui), egui::Button::new(tr("Отмена", "Cancel"))).clicked() {
                action = Some(EditorAction::Cancel);
            }
            let label = if self.has_changes() {
                tr("Готово — применить правки", "Done — apply edits")
            } else {
                tr("Закрыть редактор", "Close editor")
            };
            let button = egui::Button::new(RichText::new(label).color(Color32::WHITE)).fill(BLUE);
            if ui.add_sized(full(ui), button).clicked() {
                action = Some(EditorAction::Apply);
            }
        });
        action
    }

    fn text_group(&mut self, ui: &mut egui::Ui, selected: Option<usize>) {
        let title = if selected.is_some() { tr("Выделенная надпись", "Selected caption") } else { tr("Новая надпись", "New caption") };
        let mut start_edit = false;
        let mut remove = false;
        group(ui, title, |ui| {
            let mut style = self.style;
            egui::ComboBox::from_label(tr("Шрифт", "Font")).selected_text(style.family.label()).show_ui(ui, |ui| {
                for family in Family::ALL {
                    ui.selectable_value(&mut style.family, family, family.label());
                }
            });
            ui.horizontal(|ui| {
                ui.checkbox(&mut style.bold, tr("Жирный", "Bold"));
                ui.checkbox(&mut style.italic, tr("Курсив", "Italic"));
            });
            ui.horizontal(|ui| {
                ui.label(tr("Размер:", "Size:"));
                ui.add(egui::DragValue::new(&mut style.size).range(4.0..=200.0).speed(0.5).suffix(" pt"));
                ui.label(tr("Цвет:", "Color:"));
                ui.color_edit_button_srgb(&mut style.color);
            });
            ui.horizontal(|ui| {
                ui.label(tr("Наклон:", "Angle:"));
                ui.add(egui::DragValue::new(&mut style.angle).range(-180.0..=180.0).speed(1.0).fixed_decimals(0).suffix("°"));
                if ui.small_button("0°").clicked() {
                    style.angle = 0.0;
                }
                if ui.small_button("90°").clicked() {
                    style.angle = 90.0;
                }
                if ui.small_button("−90°").clicked() {
                    style.angle = -90.0;
                }
            });
            style.angle = style.angle.round();
            self.style = style;

            if let Some(i) = selected {
                ui.horizontal(|ui| {
                    start_edit = ui.button(tr("Изменить текст", "Edit text")).clicked();
                    remove = ui.button(RichText::new(tr("Удалить надпись", "Delete caption")).color(RED)).clicked();
                });
                let item = &mut self.texts[i];
                item.family = style.family;
                item.bold = style.bold;
                item.italic = style.italic;
                item.size = style.size;
                item.color = style.color;
                if item.angle != style.angle {
                    set_angle(item, style.angle);
                }
            }
        });
        if let Some(i) = selected {
            if start_edit {
                self.start_editing(i);
            }
            if remove {
                self.delete_selected();
            }
        }
    }

    fn eraser_group(&mut self, ui: &mut egui::Ui) {
        group(ui, tr("Ластик", "Eraser"), |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.eraser_shape, EraserShape::Brush, tr("Кисть", "Brush"));
                ui.selectable_value(&mut self.eraser_shape, EraserShape::Rect, tr("Прямоугольник", "Rectangle"));
            });
            if self.eraser_shape == EraserShape::Brush {
                ui.horizontal(|ui| {
                    ui.label(tr("Толщина:", "Width:"));
                    ui.add(egui::DragValue::new(&mut self.eraser_width).range(1.0..=150.0).speed(0.5).suffix(" pt"));
                });
            }
            ui.horizontal(|ui| {
                ui.label(tr("Цвет:", "Color:"));
                ui.color_edit_button_srgb(&mut self.eraser_color);
                if ui.small_button(tr("Белый", "White")).clicked() {
                    self.eraser_color = [255, 255, 255];
                }
                if ui.small_button(tr("Чёрный", "Black")).clicked() {
                    self.eraser_color = [0, 0, 0];
                }
            });
            ui.label(
                RichText::new(tr(
                    "Закраска ложится поверх страницы; то, что под ней, остаётся в файле. Чтобы вырезать текст, используйте «Удалить текст».",
                    "Paint covers the page; whatever is underneath stays in the file. To cut text out, use “Remove text”.",
                ))
                .color(GRAY_TEXT)
                .small(),
            );
        });
    }

    // ---------- Страница ----------

    pub fn page_ui(&mut self, ui: &mut egui::Ui, page_size: (f32, f32), preview_unavailable: Option<&str>) {
        let (pw, ph) = page_size;
        let zoom = ((ui.available_width() - 24.0) / pw).clamp(0.3, 4.0);

        egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
            let (rect, response) = ui.allocate_exact_size(Vec2::new(pw * zoom, ph * zoom), Sense::click_and_drag());
            let origin = rect.min;
            let to_page =
                move |p: Pos2| Pos2::new(((p.x - origin.x) / zoom).clamp(0.0, pw), ((p.y - origin.y) / zoom).clamp(0.0, ph));
            let to_screen = move |p: Pos2| origin + p.to_vec2() * zoom;
            let painter = ui.painter_at(rect.expand(2.0));

            // Страница
            painter.rect(rect, 0.0, Color32::WHITE, Stroke::new(1.0, Color32::from_gray(200)), StrokeKind::Outside);
            match (&self.texture, self.texture_page) {
                (Some(tex), Some(p)) if p == self.page => {
                    painter.image(tex.id(), rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                }
                _ => {
                    let text = preview_unavailable.unwrap_or(tr("Загрузка страницы…", "Loading page…"));
                    painter.text(rect.center_top() + Vec2::new(0.0, 40.0), Align2::CENTER_CENTER, text, FontId::proportional(14.0), GRAY_TEXT);
                }
            }

            // Области удаления текста
            for (i, r) in self.redactions.iter().enumerate().filter(|(_, r)| r.page == self.page) {
                let selected = self.selection == Some(Selection::Area(i));
                let fill = if r.fill_white { Color32::from_white_alpha(230) } else { Color32::from_rgba_unmultiplied(217, 48, 37, 40) };
                let screen = Rect::from_min_max(to_screen(area_rect(&r.rect).min), to_screen(area_rect(&r.rect).max));
                painter.rect(screen, 0.0, fill, Stroke::new(if selected { 2.5 } else { 1.5 }, RED), StrokeKind::Inside);
            }

            // Закраска ластиком
            for (i, e) in self.erasures.iter().enumerate().filter(|(_, e)| e.page == self.page) {
                let color = rgb(e.color);
                match &e.shape {
                    EraseShape::Rect(r) => {
                        painter.rect_filled(Rect::from_min_max(to_screen(area_rect(r).min), to_screen(area_rect(r).max)), 0.0, color);
                    }
                    EraseShape::Brush { points, width } => {
                        let pts: Vec<Pos2> = points.iter().map(|p| to_screen(Pos2::new(p[0], p[1]))).collect();
                        let w = width * zoom;
                        for seg in pts.windows(2) {
                            painter.line_segment([seg[0], seg[1]], Stroke::new(w, color));
                        }
                        for p in &pts {
                            painter.circle_filled(*p, w / 2.0, color);
                        }
                    }
                }
                if self.selection == Some(Selection::Paint(i)) {
                    let b = erase_bounds(e);
                    let screen = Rect::from_min_max(to_screen(b.min), to_screen(b.max)).expand(2.0);
                    painter.rect_stroke(screen, 2.0, Stroke::new(1.5, BLUE), StrokeKind::Outside);
                }
            }

            // Надписи (кроме той, что сейчас вводится — её показывает поле ввода)
            for (i, t) in self.texts.iter().enumerate().filter(|(_, t)| t.page == self.page) {
                let selected = self.selection == Some(Selection::Text(i));
                let (d, n) = text_axes(t);
                let lh = edit::line_height(t.family, t.bold) * t.size;
                if self.editing != Some(i) {
                    for (line_no, line) in t.text.lines().enumerate() {
                        let galley = painter.layout_job(LayoutJob::simple_format(line.to_owned(), text_format(t, zoom)));
                        let top = Pos2::new(t.x, t.y) + n * (line_no as f32 * lh);
                        let angle = -t.angle.to_radians();
                        painter.add(TextShape::new(to_screen(top), galley, rgb(t.color)).with_angle(angle));
                    }
                }
                let corners = text_corners(t).map(to_screen);
                if selected || t.text.is_empty() {
                    let mut outline = corners.to_vec();
                    outline.push(corners[0]);
                    painter.line(outline, Stroke::new(1.5, BLUE));
                }
                if selected && self.editing != Some(i) {
                    // Ручка поворота
                    let top_mid = to_screen(Pos2::new(t.x, t.y) + d * (text_size(t).x / 2.0));
                    let handle = to_screen(rotate_handle(t, zoom));
                    painter.line_segment([top_mid, handle], Stroke::new(1.5, BLUE));
                    painter.circle(handle, 6.0, Color32::WHITE, Stroke::new(2.0, BLUE));
                    if t.angle != 0.0 {
                        painter.text(handle + Vec2::new(10.0, -10.0), Align2::LEFT_BOTTOM, format!("{:.0}°", t.angle), FontId::proportional(12.0), BLUE);
                    }
                }
            }

            // Рамка, которую сейчас тянут
            if let (Some(Drag::Frame { from }), Some(pointer)) = (&self.drag, response.interact_pointer_pos()) {
                let r = Rect::from_two_pos(to_screen(*from), to_screen(to_page(pointer)));
                if self.tool == Tool::Eraser {
                    painter.rect_filled(r, 0.0, rgb(self.eraser_color).gamma_multiply(0.85));
                    painter.rect_stroke(r, 0.0, Stroke::new(1.0, BLUE), StrokeKind::Inside);
                } else {
                    painter.rect(r, 0.0, Color32::from_rgba_unmultiplied(217, 48, 37, 30), Stroke::new(1.5, RED), StrokeKind::Inside);
                }
            }
            // Размер кисти под курсором
            if self.tool == Tool::Eraser
                && self.eraser_shape == EraserShape::Brush
                && let Some(hover) = response.hover_pos()
            {
                painter.circle_stroke(hover, self.eraser_width * zoom / 2.0, Stroke::new(1.0, Color32::from_gray(90)));
            }

            self.handle_pointer(ui, &response, &to_page, zoom);
            self.inline_editor(ui, &to_screen, zoom);
        });

        // Клавиши, когда текст не вводится
        if !ui.ctx().egui_wants_keyboard_input() {
            if ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Z)) {
                self.undo();
            }
            if self.selection.is_some() && ui.input(|i| i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace)) {
                self.delete_selected();
            }
        }
    }

    /// Поле ввода прямо на странице для надписи, которую сейчас печатают.
    fn inline_editor(&mut self, ui: &mut egui::Ui, to_screen: &dyn Fn(Pos2) -> Pos2, zoom: f32) {
        let Some(i) = self.editing else { return };
        let Some(t) = self.texts.get(i) else {
            self.editing = None;
            return;
        };
        let format = text_format(t, zoom);
        let size = text_size(t) * zoom + Vec2::new(t.size * zoom * 2.0, 0.0);
        let pos = to_screen(Pos2::new(t.x, t.y));
        let mut layouter = move |ui: &egui::Ui, text: &dyn egui::TextBuffer, _wrap: f32| {
            ui.fonts_mut(|f| f.layout_job(LayoutJob::simple_format(text.as_str().to_owned(), format.clone())))
        };
        let item = &mut self.texts[i];
        let output = ui.put(
            Rect::from_min_size(pos, size),
            egui::TextEdit::multiline(&mut item.text)
                .frame(egui::Frame::NONE)
                .margin(egui::Margin::ZERO)
                .desired_rows(1)
                .desired_width(size.x)
                .layouter(&mut layouter),
        );
        if self.focus_text {
            output.request_focus();
            self.focus_text = false;
        } else if output.lost_focus() {
            self.editing = None;
            self.drop_empty_texts();
        }
    }

    fn handle_pointer(&mut self, ui: &egui::Ui, response: &egui::Response, to_page: &dyn Fn(Pos2) -> Pos2, zoom: f32) {
        if response.drag_started() {
            let Some(origin) = ui.input(|i| i.pointer.press_origin()) else { return };
            let pos = to_page(origin);
            // Ручка поворота выделенной надписи — в любом инструменте
            if let Some(Selection::Text(i)) = self.selection
                && self.editing != Some(i)
                && let Some(t) = self.texts.get(i)
                && rotate_handle(t, zoom).distance(pos) * zoom <= 10.0
            {
                self.checkpoint();
                self.drag = Some(Drag::Rotate);
                return;
            }
            match self.tool {
                Tool::Remove => self.drag = Some(Drag::Frame { from: pos }),
                Tool::Eraser if self.eraser_shape == EraserShape::Rect => self.drag = Some(Drag::Frame { from: pos }),
                Tool::Eraser => {
                    self.checkpoint();
                    self.erasures.push(Erase {
                        page: self.page,
                        shape: EraseShape::Brush { points: vec![[pos.x, pos.y]], width: self.eraser_width },
                        color: self.eraser_color,
                    });
                    self.drag = Some(Drag::Brush);
                }
                Tool::Select | Tool::Text => match self.hit(pos) {
                    Some(sel) => {
                        self.select(Some(sel));
                        self.checkpoint();
                        self.drag = Some(Drag::Move { last: pos });
                    }
                    None => self.select(None),
                },
            }
        }

        if response.dragged()
            && let Some(pointer) = response.interact_pointer_pos()
        {
            let pos = to_page(pointer);
            match &mut self.drag {
                Some(Drag::Move { last }) => {
                    let delta = pos - *last;
                    *last = pos;
                    self.translate_selected(delta);
                }
                Some(Drag::Rotate) => {
                    if let Some(Selection::Text(i)) = self.selection
                        && let Some(t) = self.texts.get_mut(i)
                    {
                        // Ручка смотрит «вверх» от текста: угол, при котором она указывает на курсор
                        let v = pos - text_center(t);
                        if v.length() > 1.0 {
                            set_angle(t, (-v.x).atan2(-v.y).to_degrees());
                            self.style.angle = t.angle;
                        }
                    }
                }
                Some(Drag::Brush) => {
                    if let Some(Erase { shape: EraseShape::Brush { points, .. }, .. }) = self.erasures.last_mut()
                        && points.last().is_none_or(|l| Pos2::new(l[0], l[1]).distance(pos) * zoom >= 2.0)
                    {
                        points.push([pos.x, pos.y]);
                    }
                }
                _ => {}
            }
        }

        if response.drag_stopped() {
            if let (Some(Drag::Frame { from }), Some(pointer)) = (&self.drag, response.interact_pointer_pos()) {
                let r = Rect::from_two_pos(*from, to_page(pointer));
                if r.width() > 3.0 && r.height() > 3.0 {
                    self.checkpoint();
                    let rect = [r.min.x, r.min.y, r.max.x, r.max.y];
                    if self.tool == Tool::Eraser {
                        self.erasures.push(Erase { page: self.page, shape: EraseShape::Rect(rect), color: self.eraser_color });
                    } else {
                        self.redactions.push(Redaction { page: self.page, rect, fill_white: self.fill_white });
                        self.select(Some(Selection::Area(self.redactions.len() - 1)));
                    }
                }
            }
            self.drag = None;
        }

        if response.clicked()
            && let Some(pointer) = response.interact_pointer_pos()
        {
            let pos = to_page(pointer);
            match (self.tool, self.hit(pos)) {
                (Tool::Text, Some(Selection::Text(i))) => self.start_editing(i),
                (Tool::Text, _) => {
                    self.checkpoint();
                    let s = self.style;
                    self.texts.push(TextItem {
                        page: self.page,
                        x: pos.x,
                        y: pos.y,
                        text: String::new(),
                        family: s.family,
                        bold: s.bold,
                        italic: s.italic,
                        size: s.size,
                        color: s.color,
                        angle: s.angle,
                    });
                    self.start_editing(self.texts.len() - 1);
                }
                (Tool::Eraser, _) if self.eraser_shape == EraserShape::Brush => {
                    // Щелчок кистью — круглое пятно
                    self.checkpoint();
                    self.erasures.push(Erase {
                        page: self.page,
                        shape: EraseShape::Brush { points: vec![[pos.x, pos.y]], width: self.eraser_width },
                        color: self.eraser_color,
                    });
                }
                (_, hit) => self.select(hit),
            }
        }

        if response.double_clicked()
            && let Some(pointer) = response.interact_pointer_pos()
            && let Some(Selection::Text(i)) = self.hit(to_page(pointer))
        {
            self.start_editing(i);
        }
    }

    /// Переход на другую страницу.
    pub fn show_page(&mut self, page: usize) {
        self.select(None);
        self.editing = None;
        self.page = page.min(self.page_count.saturating_sub(1));
        self.texture_page = None;
    }
}

fn group(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style()).fill(Color32::WHITE).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(title).strong());
        add(ui);
    });
    ui.add_space(4.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(page: usize, text: &str) -> TextItem {
        TextItem {
            page,
            x: 10.0,
            y: 10.0,
            text: text.into(),
            family: Family::Sans,
            bold: false,
            italic: false,
            size: 20.0,
            color: [0, 0, 0],
            angle: 0.0,
        }
    }

    #[test]
    fn empty_texts_are_dropped_on_deselect() {
        let mut e = Editor::new(0, 1);
        e.texts = vec![item(0, ""), item(0, "Привет")];
        e.select(Some(Selection::Text(0)));
        assert_eq!(e.texts.len(), 2);
        e.select(Some(Selection::Text(1)));
        assert_eq!(e.texts.len(), 1);
        assert_eq!(e.selection, Some(Selection::Text(0)));
        assert_eq!(e.texts[0].text, "Привет");
    }

    #[test]
    fn hit_order_and_pages() {
        let mut e = Editor::new(0, 2);
        e.texts = vec![item(1, "Другая страница"), item(0, "Здесь")];
        e.redactions = vec![Redaction { page: 0, rect: [0.0, 0.0, 100.0, 100.0], fill_white: false }];
        e.erasures = vec![Erase {
            page: 0,
            shape: EraseShape::Brush { points: vec![[60.0, 80.0], [90.0, 80.0]], width: 6.0 },
            color: [255, 255, 255],
        }];
        assert_eq!(e.hit(Pos2::new(15.0, 15.0)), Some(Selection::Text(1)));
        assert_eq!(e.hit(Pos2::new(75.0, 82.0)), Some(Selection::Paint(0)));
        assert_eq!(e.hit(Pos2::new(90.0, 95.0)), Some(Selection::Area(0)));
        assert!(e.hit(Pos2::new(300.0, 300.0)).is_none());
    }

    #[test]
    fn rotated_text_hit_and_center_rotation() {
        let mut t = item(0, "Длинная надпись");
        let center = text_center(&t);
        set_angle(&mut t, 90.4);
        // Шаг 1°, поворот вокруг центра
        assert_eq!(t.angle, 90.0);
        assert!(text_center(&t).distance(center) < 0.01);
        // Повёрнутый на 90° текст идёт вверх: точка над центром внутри, справа от центра — нет
        assert!(text_contains(&t, center + Vec2::new(0.0, -60.0), 0.0));
        assert!(!text_contains(&t, center + Vec2::new(60.0, 0.0), 0.0));
    }

    #[test]
    fn undo_restores_previous_state() {
        let mut e = Editor::new(0, 1);
        e.checkpoint();
        e.texts.push(item(0, "Текст"));
        e.checkpoint();
        e.erasures.push(Erase { page: 0, shape: EraseShape::Rect([0.0, 0.0, 5.0, 5.0]), color: [0, 0, 0] });
        e.undo();
        assert!(e.erasures.is_empty() && e.texts.len() == 1);
        e.undo();
        assert!(e.texts.is_empty());
        e.undo();
        assert!(e.texts.is_empty());
    }
}
