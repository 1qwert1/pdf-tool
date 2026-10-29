//! Редактирование текста на страницах: надписи со встроенным шрифтом
//! и удаление существующего текста в выделенной области.
//!
//! Координаты в интерфейсе — «экранные» пункты страницы: от левого верхнего угла
//! страницы в том виде, как она показана (с учётом /Rotate и CropBox).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use anyhow::{Context, Result, bail};
use lopdf::content::{Content, Operation};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream, StringFormat, dictionary};

use crate::i18n::tr;
use crate::pdf_ops;

// ---------- Шрифты ----------

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub enum Family {
    Sans,
    Serif,
    Mono,
}

impl Family {
    pub const ALL: [Family; 3] = [Family::Sans, Family::Serif, Family::Mono];

    pub fn label(self) -> &'static str {
        match self {
            Family::Sans => tr("Без засечек", "Sans"),
            Family::Serif => tr("С засечками", "Serif"),
            Family::Mono => tr("Моноширинный", "Monospace"),
        }
    }
}

/// Данные встроенного шрифта (DejaVu, лицензия в fonts/LICENSE).
pub fn font_data(family: Family, bold: bool) -> &'static [u8] {
    match (family, bold) {
        (Family::Sans, false) => include_bytes!("../fonts/DejaVuSans.ttf"),
        (Family::Sans, true) => include_bytes!("../fonts/DejaVuSans-Bold.ttf"),
        (Family::Serif, false) => include_bytes!("../fonts/DejaVuSerif.ttf"),
        (Family::Serif, true) => include_bytes!("../fonts/DejaVuSerif-Bold.ttf"),
        (Family::Mono, false) => include_bytes!("../fonts/DejaVuSansMono.ttf"),
        (Family::Mono, true) => include_bytes!("../fonts/DejaVuSansMono-Bold.ttf"),
    }
}

/// Имя шрифта для egui (тот же файл используется для предпросмотра).
pub fn egui_font_name(family: Family, bold: bool) -> String {
    format!("pdf-{family:?}-{bold}")
}

/// Наклон синтетического курсива (tan ≈ 12°).
pub const ITALIC_SHEAR: f32 = 0.21;
/// Межстрочный интервал в долях размера шрифта — как у egui (ascender − descender + lineGap),
/// чтобы строки в поле ввода и в PDF совпадали.
pub fn line_height(family: Family, bold: bool) -> f32 {
    let face = ttf_parser::Face::parse(font_data(family, bold), 0).expect("built-in font");
    (face.ascender() as f32 - face.descender() as f32 + face.line_gap() as f32) / face.units_per_em() as f32
}

/// Направление текста `d` и направление «к следующей строке» `n` на экране
/// (ось y вниз) для наклона `angle` градусов против часовой стрелки.
pub fn axes(angle: f32) -> ((f32, f32), (f32, f32)) {
    let (sin, cos) = angle.to_radians().sin_cos();
    ((cos, -sin), (sin, cos))
}

/// Высота над базовой линией в долях размера шрифта.
pub fn ascent(family: Family, bold: bool) -> f32 {
    let face = ttf_parser::Face::parse(font_data(family, bold), 0).expect("built-in font");
    face.ascender() as f32 / face.units_per_em() as f32
}

// ---------- Правки ----------

#[derive(Clone, Debug, PartialEq)]
pub struct TextItem {
    pub page: usize,
    /// Левый верхний угол блока текста в экранных пунктах.
    pub x: f32,
    pub y: f32,
    pub text: String,
    pub family: Family,
    pub bold: bool,
    pub italic: bool,
    pub size: f32,
    pub color: [u8; 3],
    /// Наклон в градусах против часовой стрелки (вокруг левого верхнего угла).
    pub angle: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Redaction {
    pub page: usize,
    /// Прямоугольник в экранных пунктах: x0, y0, x1, y1.
    pub rect: [f32; 4],
    /// Дополнительно закрасить область белым (скрыть картинки и графику).
    pub fill_white: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum EraseShape {
    /// Мазок кистью: точки в экранных пунктах и толщина.
    Brush { points: Vec<[f32; 2]>, width: f32 },
    /// Прямоугольник в экранных пунктах: x0, y0, x1, y1.
    Rect([f32; 4]),
}

/// Ластик: закрашивает область цветом поверх содержимого страницы.
#[derive(Clone, Debug, PartialEq)]
pub struct Erase {
    pub page: usize,
    pub shape: EraseShape,
    pub color: [u8; 3],
}

// ---------- Геометрия страницы ----------

#[derive(Clone, Copy, Debug)]
pub struct PageGeometry {
    /// CropBox (или MediaBox): x0, y0, x1, y1 в пользовательских координатах.
    pub boxed: [f32; 4],
    /// Поворот при показе: 0, 90, 180, 270 по часовой.
    pub rotate: u16,
}

type Matrix = [f32; 6];
const IDENTITY: Matrix = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

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

fn apply(m: &Matrix, x: f32, y: f32) -> (f32, f32) {
    (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5])
}

fn rect_of(object: &Object) -> Option<[f32; 4]> {
    let values: Vec<f32> = object.as_array().ok()?.iter().filter_map(|v| v.as_float().ok()).collect();
    let [a, b, c, d]: [f32; 4] = values.try_into().ok()?;
    Some([a.min(c), b.min(d), a.max(c), b.max(d)])
}

impl PageGeometry {
    pub fn of(doc: &Document, page_id: ObjectId) -> Self {
        let media = pdf_ops::inherited_attr(doc, page_id, b"MediaBox")
            .as_ref()
            .and_then(rect_of)
            .unwrap_or([0.0, 0.0, 595.0, 842.0]);
        let boxed = pdf_ops::inherited_attr(doc, page_id, b"CropBox")
            .as_ref()
            .and_then(rect_of)
            .map(|c| [c[0].max(media[0]), c[1].max(media[1]), c[2].min(media[2]), c[3].min(media[3])])
            .filter(|c| c[2] > c[0] && c[3] > c[1])
            .unwrap_or(media);
        let rotate = pdf_ops::inherited_attr(doc, page_id, b"Rotate")
            .and_then(|r| r.as_i64().ok())
            .map(|r| r.rem_euclid(360) as u16 / 90 * 90)
            .unwrap_or(0);
        PageGeometry { boxed, rotate }
    }

    fn box_size(&self) -> (f32, f32) {
        (self.boxed[2] - self.boxed[0], self.boxed[3] - self.boxed[1])
    }

    /// Размер страницы на экране в пунктах.
    pub fn display_size(&self) -> (f32, f32) {
        let (w, h) = self.box_size();
        if self.rotate.is_multiple_of(180) { (w, h) } else { (h, w) }
    }

    /// Матрица из экранных пунктов (от левого верхнего угла) в пользовательские координаты.
    fn display_to_user(&self) -> Matrix {
        let (w, h) = self.box_size();
        let [x0, y0, ..] = self.boxed;
        // X = a·u + c·v + e, Y = b·u + d·v + f (относительно угла CropBox)
        let m = match self.rotate {
            90 => [0.0, 1.0, 1.0, 0.0, 0.0, 0.0],
            180 => [-1.0, 0.0, 0.0, 1.0, w, 0.0],
            270 => [0.0, -1.0, -1.0, 0.0, w, h],
            _ => [1.0, 0.0, 0.0, -1.0, 0.0, h],
        };
        [m[0], m[1], m[2], m[3], m[4] + x0, m[5] + y0]
    }

    pub fn to_user(self, u: f32, v: f32) -> (f32, f32) {
        apply(&self.display_to_user(), u, v)
    }

    /// Матрица текста для строки, начинающейся в экранной точке (u, v) на базовой линии
    /// и идущей в экранном направлении `d` (единичный вектор).
    fn text_matrix(&self, u: f32, v: f32, d: (f32, f32), italic: bool) -> Matrix {
        let m = self.display_to_user();
        let linear = |x: f32, y: f32| (m[0] * x + m[2] * y, m[1] * x + m[3] * y);
        let dir = linear(d.0, d.1);
        // «Вверх» на экране — перпендикуляр к направлению текста
        let up = linear(d.1, -d.0);
        let shear = if italic { ITALIC_SHEAR } else { 0.0 };
        let (x, y) = apply(&m, u, v);
        [dir.0, dir.1, up.0 + shear * dir.0, up.1 + shear * dir.1, x, y]
    }

    /// Прямоугольник в пользовательских координатах.
    fn user_rect(&self, rect: [f32; 4]) -> [f32; 4] {
        let (ax, ay) = self.to_user(rect[0], rect[1]);
        let (bx, by) = self.to_user(rect[2], rect[3]);
        [ax.min(bx), ay.min(by), ax.max(bx), ay.max(by)]
    }
}

/// Экранные размеры всех страниц в пунктах.
pub fn page_sizes(doc: &Document) -> Vec<(f32, f32)> {
    doc.get_pages().values().map(|&id| PageGeometry::of(doc, id).display_size()).collect()
}

// ---------- Применение правок ----------

/// Применяет правки к документу. Возвращает описание для строки состояния.
pub fn apply_edits(doc: &mut Document, texts: &[TextItem], redactions: &[Redaction], erasures: &[Erase]) -> Result<String> {
    let pages: Vec<ObjectId> = doc.get_pages().into_values().collect();
    let page_id = |index: usize| pages.get(index).copied().with_context(|| trf!("Нет страницы {}", "No page {}", index + 1));

    // Сначала удаление: новые надписи в выделенной области не должны пропасть
    let mut removed = 0;
    let mut by_page: BTreeMap<usize, Vec<&Redaction>> = BTreeMap::new();
    for r in redactions {
        by_page.entry(r.page).or_default().push(r);
    }
    for (&index, list) in &by_page {
        let id = page_id(index)?;
        let geometry = PageGeometry::of(doc, id);
        let rects: Vec<[f32; 4]> = list.iter().map(|r| geometry.user_rect(r.rect)).collect();
        removed += remove_text(doc, id, &rects).with_context(|| trf!("Страница {}", "Page {}", index + 1))?;
        let fills: Vec<[f32; 4]> =
            list.iter().zip(&rects).filter(|(r, _)| r.fill_white).map(|(_, rect)| *rect).collect();
        if !fills.is_empty() {
            let mut ops = String::from("q 1 1 1 rg\n");
            for [x0, y0, x1, y1] in fills {
                ops += &format!("{x0:.2} {y0:.2} {:.2} {:.2} re f\n", x1 - x0, y1 - y0);
            }
            ops += "Q\n";
            append_content(doc, id, ops.into_bytes(), &[])?;
        }
    }

    // Ластик: закраска поверх содержимого, под новыми надписями
    let mut erase_by_page: BTreeMap<usize, Vec<&Erase>> = BTreeMap::new();
    for e in erasures {
        erase_by_page.entry(e.page).or_default().push(e);
    }
    for (&index, list) in &erase_by_page {
        let id = page_id(index)?;
        let geometry = PageGeometry::of(doc, id);
        let mut ops = String::from("q 1 J 1 j\n");
        for e in list {
            let [r, g, b] = e.color.map(|c| c as f32 / 255.0);
            match &e.shape {
                EraseShape::Brush { points, width } => {
                    let Some(first) = points.first() else { continue };
                    ops += &format!("{r:.3} {g:.3} {b:.3} RG {width:.2} w ");
                    let (x, y) = geometry.to_user(first[0], first[1]);
                    ops += &format!("{x:.2} {y:.2} m ");
                    // Одиночная точка: отрезок нулевой длины с круглыми концами рисует круг
                    let rest = if points.len() > 1 { &points[1..] } else { &points[..] };
                    for p in rest {
                        let (x, y) = geometry.to_user(p[0], p[1]);
                        ops += &format!("{x:.2} {y:.2} l ");
                    }
                    ops += "S\n";
                }
                EraseShape::Rect(rect) => {
                    let [x0, y0, x1, y1] = geometry.user_rect(*rect);
                    ops += &format!("{r:.3} {g:.3} {b:.3} rg {x0:.2} {y0:.2} {:.2} {:.2} re f\n", x1 - x0, y1 - y0);
                }
            }
        }
        ops += "Q\n";
        append_content(doc, id, ops.into_bytes(), &[])?;
    }

    // Надписи: один встроенный шрифт (подмножество) на каждое начертание
    let texts: Vec<&TextItem> = texts.iter().filter(|t| !t.text.trim().is_empty()).collect();
    let mut chars: BTreeMap<(Family, bool), BTreeSet<char>> = BTreeMap::new();
    for t in &texts {
        chars.entry((t.family, t.bold)).or_default().extend(t.text.chars().filter(|c| !c.is_control()));
    }
    let mut fonts = HashMap::new();
    let mut missing = 0;
    for (n, (&(family, bold), set)) in chars.iter().enumerate() {
        let font = embed_font(doc, font_data(family, bold), set)?;
        missing += font.missing;
        fonts.insert((family, bold), (format!("PTF{n}"), font));
    }

    let mut texts_by_page: BTreeMap<usize, Vec<&TextItem>> = BTreeMap::new();
    for t in &texts {
        texts_by_page.entry(t.page).or_default().push(t);
    }
    for (&index, items) in &texts_by_page {
        let id = page_id(index)?;
        let geometry = PageGeometry::of(doc, id);
        let mut ops = String::new();
        let mut used = Vec::new();
        for t in items {
            let (name, font) = &fonts[&(t.family, t.bold)];
            used.push((name.clone(), font.id));
            let asc = ascent(t.family, t.bold) * t.size;
            let leading = line_height(t.family, t.bold) * t.size;
            let (dir, next) = axes(t.angle);
            let [r, g, b] = t.color.map(|c| c as f32 / 255.0);
            for (line_no, line) in t.text.lines().enumerate() {
                if line.is_empty() {
                    continue;
                }
                // Начало базовой линии: от угла блока вниз (с учётом наклона)
                let down = asc + line_no as f32 * leading;
                let (u, v) = (t.x + next.0 * down, t.y + next.1 * down);
                let [a, b2, c, d, e, f] = geometry.text_matrix(u, v, dir, t.italic);
                let hex: String = line
                    .chars()
                    .filter(|c| !c.is_control())
                    .map(|c| format!("{:04X}", font.cids.get(&c).copied().unwrap_or(0)))
                    .collect();
                ops += &format!(
                    "BT /{name} {:.2} Tf {r:.3} {g:.3} {b:.3} rg {a:.4} {b2:.4} {c:.4} {d:.4} {e:.2} {f:.2} Tm <{hex}> Tj ET\n",
                    t.size
                );
            }
        }
        used.sort();
        used.dedup();
        append_content(doc, id, format!("q\n{ops}Q\n").into_bytes(), &used)?;
    }

    let mut parts = Vec::new();
    if !texts.is_empty() {
        parts.push(trf!("добавлено надписей: {}", "captions added: {}", texts.len()));
    }
    if !redactions.is_empty() {
        parts.push(trf!("удалено символов: {removed}", "characters removed: {removed}"));
    }
    if !erasures.is_empty() {
        parts.push(trf!("закрашено областей: {}", "areas painted over: {}", erasures.len()));
    }
    if missing > 0 {
        parts.push(trf!("символов нет в шрифте: {missing}", "characters missing from the font: {missing}"));
    }
    let message = parts.join(", ");
    let mut chars = message.chars();
    Ok(chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default())
}

/// Добавляет содержимое в конец страницы. Старое содержимое оборачивается в q/Q,
/// чтобы его графическое состояние не влияло на новое.
fn append_content(doc: &mut Document, page_id: ObjectId, ops: Vec<u8>, fonts: &[(String, ObjectId)]) -> Result<()> {
    if !fonts.is_empty() {
        // Ресурсы копируем в саму страницу: общий словарь ресурсов менять нельзя
        let resources = pdf_ops::inherited_attr(doc, page_id, b"Resources").unwrap_or(Object::Dictionary(Dictionary::new()));
        let mut resources = match resources {
            Object::Reference(id) => doc.get_dictionary(id)?.clone(),
            Object::Dictionary(d) => d,
            _ => Dictionary::new(),
        };
        let mut font_dict = match resources.get(b"Font") {
            Ok(Object::Reference(id)) => doc.get_dictionary(*id)?.clone(),
            Ok(Object::Dictionary(d)) => d.clone(),
            _ => Dictionary::new(),
        };
        for (name, id) in fonts {
            font_dict.set(name.as_bytes(), Object::Reference(*id));
        }
        resources.set("Font", font_dict);
        doc.get_dictionary_mut(page_id)?.set("Resources", resources);
    }

    let mut contents: Vec<Object> = match doc.get_dictionary(page_id)?.get(b"Contents") {
        Ok(Object::Reference(id)) => vec![Object::Reference(*id)],
        Ok(Object::Array(arr)) => arr.clone(),
        _ => vec![],
    };
    let push = Stream::new(Dictionary::new(), b"q\n".to_vec());
    let mut tail = b"\nQ\n".to_vec();
    tail.extend_from_slice(&ops);
    let mut tail = Stream::new(Dictionary::new(), tail);
    let _ = tail.compress();
    contents.insert(0, Object::Reference(doc.add_object(push)));
    contents.push(Object::Reference(doc.add_object(tail)));
    doc.get_dictionary_mut(page_id)?.set("Contents", contents);
    Ok(())
}

struct EmbeddedFont {
    id: ObjectId,
    cids: HashMap<char, u16>,
    /// Сколько символов нет в шрифте (будут пустыми).
    missing: usize,
}

/// Встраивает подмножество шрифта как Type0/CIDFontType2 с кодировкой Identity-H.
fn embed_font(doc: &mut Document, data: &[u8], chars: &BTreeSet<char>) -> Result<EmbeddedFont> {
    let face = ttf_parser::Face::parse(data, 0).context(tr("Не удалось прочитать шрифт", "Could not read the font"))?;
    let upem = face.units_per_em() as f32;
    let scale = |v: f32| (v * 1000.0 / upem).round() as i64;

    let mut remapper = subsetter::GlyphRemapper::new();
    let mut cids = HashMap::new();
    let mut widths = BTreeMap::new();
    let mut missing = 0;
    for &ch in chars {
        let gid = face.glyph_index(ch);
        if gid.is_none() {
            missing += 1;
        }
        let gid = gid.unwrap_or(ttf_parser::GlyphId(0));
        let cid = remapper.remap(gid.0);
        cids.insert(ch, cid);
        widths.insert(cid, scale(face.glyph_hor_advance(gid).unwrap_or(0) as f32));
    }
    let subset = subsetter::subset(data, 0, &remapper).map_err(|e| anyhow::anyhow!(trf!("Не удалось подготовить шрифт: {e}", "Could not prepare the font: {e}")))?;

    let family_name = face
        .names()
        .into_iter()
        .find(|n| n.name_id == ttf_parser::name_id::POST_SCRIPT_NAME)
        .and_then(|n| n.to_string())
        .unwrap_or_else(|| "DejaVu".into());
    // Метка подмножества — 6 заглавных букв, зависят от набора символов
    let mut hash: u64 = 1469598103934665603;
    for c in chars {
        hash = (hash ^ *c as u64).wrapping_mul(1099511628211);
    }
    let tag: String = (0..6).map(|i| (b'A' + ((hash >> (i * 5)) % 26) as u8) as char).collect();
    let base_font = format!("{tag}+{family_name}");

    let bbox = face.global_bounding_box();
    let mut font_file = Stream::new(dictionary! { "Length1" => subset.len() as i64 }, subset);
    let _ = font_file.compress();
    let font_file_id = doc.add_object(font_file);
    let mono = face.is_monospaced();
    let descriptor_id = doc.add_object(dictionary! {
        "Type" => "FontDescriptor",
        "FontName" => Object::Name(base_font.clone().into_bytes()),
        "Flags" => if mono { 33 } else { 32 },
        "FontBBox" => vec![
            scale(bbox.x_min as f32).into(), scale(bbox.y_min as f32).into(),
            scale(bbox.x_max as f32).into(), scale(bbox.y_max as f32).into(),
        ],
        "ItalicAngle" => 0,
        "Ascent" => scale(face.ascender() as f32),
        "Descent" => scale(face.descender() as f32),
        "CapHeight" => scale(face.capital_height().unwrap_or(face.ascender()) as f32),
        "StemV" => 80,
        "FontFile2" => font_file_id,
    });

    let mut w_array = Vec::new();
    for (cid, w) in &widths {
        w_array.push(Object::Integer(*cid as i64));
        w_array.push(Object::Array(vec![Object::Integer(*w)]));
    }
    let cid_font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "CIDFontType2",
        "BaseFont" => Object::Name(base_font.clone().into_bytes()),
        "CIDSystemInfo" => dictionary! {
            "Registry" => Object::string_literal("Adobe"),
            "Ordering" => Object::string_literal("Identity"),
            "Supplement" => 0,
        },
        "FontDescriptor" => descriptor_id,
        "DW" => 1000,
        "W" => w_array,
        "CIDToGIDMap" => "Identity",
    });

    // ToUnicode — чтобы добавленный текст можно было копировать и искать
    let mut cmap = String::from(
        "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n\
         /CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
         /CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n\
         1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n",
    );
    let entries: Vec<(u16, char)> = cids.iter().map(|(&c, &cid)| (cid, c)).collect::<BTreeMap<_, _>>().into_iter().collect();
    for chunk in entries.chunks(100) {
        cmap += &format!("{} beginbfchar\n", chunk.len());
        for (cid, ch) in chunk {
            let utf16: String = ch.encode_utf16(&mut [0; 2]).iter().map(|u| format!("{u:04X}")).collect();
            cmap += &format!("<{cid:04X}> <{utf16}>\n");
        }
        cmap += "endbfchar\n";
    }
    cmap += "endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n";
    let mut to_unicode = Stream::new(Dictionary::new(), cmap.into_bytes());
    let _ = to_unicode.compress();
    let to_unicode_id = doc.add_object(to_unicode);

    let id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type0",
        "BaseFont" => Object::Name(base_font.into_bytes()),
        "Encoding" => "Identity-H",
        "DescendantFonts" => vec![Object::Reference(cid_font_id)],
        "ToUnicode" => to_unicode_id,
    });
    Ok(EmbeddedFont { id, cids, missing })
}

// ---------- Удаление текста ----------

/// Ширины символов шрифта (в тысячных долях размера).
struct FontInfo {
    two_byte: bool,
    widths: HashMap<u32, f32>,
    default_width: f32,
}

fn deref<'a>(doc: &'a Document, object: &'a Object) -> &'a Object {
    match object {
        Object::Reference(id) => doc.get_object(*id).unwrap_or(object),
        other => other,
    }
}

fn font_info(doc: &Document, font: &Dictionary) -> FontInfo {
    let subtype = font.get(b"Subtype").and_then(Object::as_name).unwrap_or(b"");
    let mut widths = HashMap::new();

    if subtype == b"Type0" {
        let descendant = font
            .get(b"DescendantFonts")
            .ok()
            .map(|d| deref(doc, d))
            .and_then(|d| d.as_array().ok())
            .and_then(|a| a.first())
            .map(|d| deref(doc, d))
            .and_then(|d| d.as_dict().ok());
        let mut default_width = 1000.0;
        if let Some(cid_font) = descendant {
            default_width = cid_font.get(b"DW").and_then(Object::as_float).unwrap_or(1000.0);
            if let Some(w) = cid_font.get(b"W").ok().map(|w| deref(doc, w)).and_then(|w| w.as_array().ok()) {
                let mut i = 0;
                while i < w.len() {
                    let Ok(first) = deref(doc, &w[i]).as_i64() else { break };
                    match w.get(i + 1).map(|o| deref(doc, o)) {
                        Some(Object::Array(list)) => {
                            for (k, v) in list.iter().enumerate() {
                                if let Ok(v) = deref(doc, v).as_float() {
                                    widths.insert(first as u32 + k as u32, v);
                                }
                            }
                            i += 2;
                        }
                        Some(last) => {
                            let (Ok(last), Some(Ok(v))) = (last.as_i64(), w.get(i + 2).map(|o| deref(doc, o).as_float()))
                            else {
                                break;
                            };
                            for code in first..=last {
                                widths.insert(code as u32, v);
                            }
                            i += 3;
                        }
                        None => break,
                    }
                }
            }
        }
        return FontInfo { two_byte: true, widths, default_width };
    }

    // Простые шрифты: однобайтовые коды, ширины в /Widths начиная с /FirstChar
    let first = font.get(b"FirstChar").and_then(Object::as_i64).unwrap_or(0);
    if let Some(list) = font.get(b"Widths").ok().map(|w| deref(doc, w)).and_then(|w| w.as_array().ok()) {
        for (k, v) in list.iter().enumerate() {
            if let Ok(v) = deref(doc, v).as_float() {
                widths.insert((first + k as i64) as u32, v);
            }
        }
    }
    // Type3: ширины в единицах глифа, переводим через FontMatrix
    if subtype == b"Type3"
        && let Some(scale) = font
            .get(b"FontMatrix")
            .ok()
            .and_then(|m| m.as_array().ok())
            .and_then(|m| m.first())
            .and_then(|v| v.as_float().ok())
    {
        for v in widths.values_mut() {
            *v *= scale * 1000.0;
        }
    }
    let missing = font
        .get(b"FontDescriptor")
        .ok()
        .map(|d| deref(doc, d))
        .and_then(|d| d.as_dict().ok())
        .and_then(|d| d.get(b"MissingWidth").and_then(Object::as_float).ok());
    // Для стандартных шрифтов без /Widths берём среднюю ширину
    FontInfo { two_byte: false, widths, default_width: missing.unwrap_or(500.0) }
}

/// Состояние графики и текста, сохраняемое q/Q.
#[derive(Clone)]
struct GState {
    ctm: Matrix,
    char_spacing: f32,
    word_spacing: f32,
    h_scale: f32,
    leading: f32,
    rise: f32,
    font: Option<Vec<u8>>,
    size: f32,
}

impl Default for GState {
    fn default() -> Self {
        GState {
            ctm: IDENTITY,
            char_spacing: 0.0,
            word_spacing: 0.0,
            h_scale: 1.0,
            leading: 0.0,
            rise: 0.0,
            font: None,
            size: 0.0,
        }
    }
}

enum Piece {
    Text(Vec<u8>),
    Shift(f32),
}

fn in_rects(rects: &[[f32; 4]], (x, y): (f32, f32)) -> bool {
    rects.iter().any(|r| x >= r[0] && x <= r[2] && y >= r[1] && y <= r[3])
}

fn num(object: &Object) -> f32 {
    object.as_float().unwrap_or(0.0)
}

/// Удаляет со страницы символы, центр которых попадает в прямоугольники
/// (пользовательские координаты). Возвращает число удалённых символов.
fn remove_text(doc: &mut Document, page_id: ObjectId, rects: &[[f32; 4]]) -> Result<usize> {
    let content = doc.get_page_content(page_id);
    let Ok(content) = Content::decode(&content) else {
        bail!(tr("Не удалось разобрать содержимое страницы", "Could not parse the page content"));
    };

    let resources = pdf_ops::inherited_attr(doc, page_id, b"Resources");
    let font_dicts: HashMap<Vec<u8>, FontInfo> = resources
        .as_ref()
        .map(|r| deref(doc, r))
        .and_then(|r| r.as_dict().ok())
        .and_then(|r| r.get(b"Font").ok())
        .map(|f| deref(doc, f))
        .and_then(|f| f.as_dict().ok())
        .map(|fonts| {
            fonts
                .iter()
                .filter_map(|(name, f)| deref(doc, f).as_dict().ok().map(|d| (name.clone(), font_info(doc, d))))
                .collect()
        })
        .unwrap_or_default();
    let fallback = FontInfo { two_byte: false, widths: HashMap::new(), default_width: 500.0 };

    let mut gs = GState::default();
    let mut stack: Vec<GState> = Vec::new();
    let mut tm = IDENTITY;
    let mut tlm = IDENTITY;
    let mut removed = 0;
    let mut out: Vec<Operation> = Vec::with_capacity(content.operations.len());

    for op in content.operations {
        let operands = &op.operands;
        match op.operator.as_str() {
            "q" => stack.push(gs.clone()),
            "Q" => gs = stack.pop().unwrap_or_default(),
            "cm" => {
                let values: Vec<f32> = operands.iter().map(num).collect();
                if let Ok(m) = <[f32; 6]>::try_from(values) {
                    gs.ctm = multiply(&m, &gs.ctm);
                }
            }
            "BT" => {
                tm = IDENTITY;
                tlm = IDENTITY;
            }
            "Tc" => gs.char_spacing = operands.first().map(num).unwrap_or(0.0),
            "Tw" => gs.word_spacing = operands.first().map(num).unwrap_or(0.0),
            "Tz" => gs.h_scale = operands.first().map(num).unwrap_or(100.0) / 100.0,
            "TL" => gs.leading = operands.first().map(num).unwrap_or(0.0),
            "Ts" => gs.rise = operands.first().map(num).unwrap_or(0.0),
            "Tf" => {
                gs.font = operands.first().and_then(|o| o.as_name().ok()).map(<[u8]>::to_vec);
                gs.size = operands.get(1).map(num).unwrap_or(0.0);
            }
            "Td" | "TD" => {
                let (tx, ty) = (operands.first().map(num).unwrap_or(0.0), operands.get(1).map(num).unwrap_or(0.0));
                if op.operator == "TD" {
                    gs.leading = -ty;
                }
                tlm = multiply(&[1.0, 0.0, 0.0, 1.0, tx, ty], &tlm);
                tm = tlm;
            }
            "Tm" => {
                let values: Vec<f32> = operands.iter().map(num).collect();
                if let Ok(m) = <[f32; 6]>::try_from(values) {
                    tlm = m;
                    tm = m;
                }
            }
            "T*" => {
                tlm = multiply(&[1.0, 0.0, 0.0, 1.0, 0.0, -gs.leading], &tlm);
                tm = tlm;
            }
            "Tj" | "TJ" | "'" | "\"" => {
                let mut prefix = Vec::new();
                if op.operator == "\"" {
                    gs.word_spacing = operands.first().map(num).unwrap_or(0.0);
                    gs.char_spacing = operands.get(1).map(num).unwrap_or(0.0);
                    prefix.push(Operation::new("Tw", vec![operands[0].clone()]));
                    prefix.push(Operation::new("Tc", vec![operands[1].clone()]));
                }
                if op.operator == "'" || op.operator == "\"" {
                    tlm = multiply(&[1.0, 0.0, 0.0, 1.0, 0.0, -gs.leading], &tlm);
                    tm = tlm;
                    prefix.push(Operation::new("T*", vec![]));
                }
                let elements: Vec<Object> = match op.operator.as_str() {
                    "TJ" => operands.first().and_then(|o| o.as_array().ok()).cloned().unwrap_or_default(),
                    _ => operands.last().cloned().into_iter().collect(),
                };
                let font = gs.font.as_ref().and_then(|f| font_dicts.get(f)).unwrap_or(&fallback);

                let mut pieces: Vec<Piece> = Vec::new();
                let mut any_removed = false;
                let mut format = StringFormat::Literal;
                for element in &elements {
                    match element {
                        Object::String(bytes, fmt) => {
                            format = *fmt;
                            let step = if font.two_byte { 2 } else { 1 };
                            for chunk in bytes.chunks(step) {
                                let code = chunk.iter().fold(0u32, |acc, b| (acc << 8) | *b as u32);
                                let w0 = font.widths.get(&code).copied().unwrap_or(font.default_width) / 1000.0;
                                let trm = multiply(
                                    &multiply(&[gs.size * gs.h_scale, 0.0, 0.0, gs.size, 0.0, gs.rise], &tm),
                                    &gs.ctm,
                                );
                                let center = apply(&trm, w0 / 2.0, 0.35);
                                let space = if !font.two_byte && code == 32 { gs.word_spacing } else { 0.0 };
                                let tx = (w0 * gs.size + gs.char_spacing + space) * gs.h_scale;
                                tm = multiply(&[1.0, 0.0, 0.0, 1.0, tx, 0.0], &tm);
                                if in_rects(rects, center) {
                                    any_removed = true;
                                    removed += 1;
                                    match pieces.last_mut() {
                                        Some(Piece::Shift(s)) => *s += tx,
                                        _ => pieces.push(Piece::Shift(tx)),
                                    }
                                } else {
                                    match pieces.last_mut() {
                                        Some(Piece::Text(t)) => t.extend_from_slice(chunk),
                                        _ => pieces.push(Piece::Text(chunk.to_vec())),
                                    }
                                }
                            }
                        }
                        other => {
                            let n = num(other);
                            let tx = -n / 1000.0 * gs.size * gs.h_scale;
                            tm = multiply(&[1.0, 0.0, 0.0, 1.0, tx, 0.0], &tm);
                            match pieces.last_mut() {
                                Some(Piece::Shift(s)) => *s += tx,
                                _ => pieces.push(Piece::Shift(tx)),
                            }
                        }
                    }
                }

                if !any_removed {
                    out.push(op);
                    continue;
                }
                out.extend(prefix);
                let scale = gs.size * gs.h_scale;
                let array: Vec<Object> = pieces
                    .into_iter()
                    .filter_map(|p| match p {
                        Piece::Text(t) => Some(Object::String(t, format)),
                        Piece::Shift(s) if scale.abs() > f32::EPSILON && s.abs() > 1e-4 => {
                            Some(Object::Real(-s * 1000.0 / scale))
                        }
                        Piece::Shift(_) => None,
                    })
                    .collect();
                out.push(Operation::new("TJ", vec![Object::Array(array)]));
                continue;
            }
            _ => {}
        }
        out.push(op);
    }

    if removed > 0 {
        let encoded = Content { operations: out }.encode().context(tr("Не удалось записать содержимое страницы", "Could not write the page content"))?;
        let mut stream = Stream::new(Dictionary::new(), encoded);
        let _ = stream.compress();
        let id = doc.add_object(stream);
        doc.get_dictionary_mut(page_id)?.set("Contents", Object::Reference(id));
    }
    Ok(removed)
}

/// Ширина строки текста в пунктах (для рамки выделения надписи).
pub fn text_width(family: Family, bold: bool, size: f32, line: &str) -> f32 {
    let face = ttf_parser::Face::parse(font_data(family, bold), 0).expect("built-in font");
    let upem = face.units_per_em() as f32;
    line.chars()
        .map(|c| face.glyph_index(c).and_then(|g| face.glyph_hor_advance(g)).unwrap_or(0) as f32)
        .sum::<f32>()
        * size
        / upem
}

/// Страницы, затронутые правками.
pub fn touched_pages(texts: &[TextItem], redactions: &[Redaction], erasures: &[Erase]) -> HashSet<usize> {
    texts.iter().map(|t| t.page).chain(redactions.iter().map(|r| r.page)).chain(erasures.iter().map(|e| e.page)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Документ из одной страницы 600×800 с текстом Helvetica.
    fn text_page(content: &str, rotate: i64) -> Document {
        let mut doc = pdf_ops::new_document();
        let font = doc.add_object(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
            "FirstChar" => 32, "Widths" => vec![Object::Integer(500); 95],
        });
        let contents = doc.add_object(Stream::new(Dictionary::new(), content.as_bytes().to_vec()));
        let root = doc.catalog().unwrap().get(b"Pages").unwrap().as_reference().unwrap();
        let page = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => root,
            "MediaBox" => vec![0.into(), 0.into(), 600.into(), 800.into()],
            "Rotate" => rotate,
            "Resources" => dictionary! { "Font" => dictionary! { "F1" => font } },
            "Contents" => contents,
        });
        let pages = doc.get_dictionary_mut(root).unwrap();
        pages.set("Kids", vec![Object::Reference(page)]);
        pages.set("Count", 1);
        doc
    }

    fn shown_strings(doc: &Document) -> Vec<String> {
        let page = *doc.get_pages().values().next().unwrap();
        let content = Content::decode(&doc.get_page_content(page)).unwrap();
        let mut result = Vec::new();
        for op in content.operations {
            for operand in op.operands {
                let items = match operand {
                    Object::Array(a) => a,
                    other => vec![other],
                };
                for item in items {
                    if let Object::String(s, _) = item {
                        result.push(String::from_utf8_lossy(&s).into_owned());
                    }
                }
            }
        }
        result
    }

    #[test]
    fn geometry_maps_rotations() {
        // Точка в левом верхнем углу экрана для каждого поворота
        let corners = [(0, (0.0, 800.0)), (90, (0.0, 0.0)), (180, (600.0, 0.0)), (270, (600.0, 800.0))];
        for (rotate, expected) in corners {
            let g = PageGeometry { boxed: [0.0, 0.0, 600.0, 800.0], rotate };
            assert_eq!(g.to_user(0.0, 0.0), expected, "поворот {rotate}");
        }
        let g = PageGeometry { boxed: [0.0, 0.0, 600.0, 800.0], rotate: 90 };
        assert_eq!(g.display_size(), (800.0, 600.0));
    }

    #[test]
    fn removes_only_text_in_area() {
        // Каждая буква шириной 500/1000 × 20 = 10 pt, строка начинается в x=100
        let mut doc = text_page("BT /F1 20 Tf 100 700 Td (Hello World) Tj ET", 0);
        // Экранная область над словом World: x 160..210, y (800-700)=100 → 85..105
        let r = Redaction { page: 0, rect: [158.0, 85.0, 212.0, 105.0], fill_white: false };
        let message = apply_edits(&mut doc, &[], &[r], &[]).unwrap();
        assert_eq!(message, "Удалено символов: 5");
        assert_eq!(shown_strings(&doc), ["Hello "]);
    }

    #[test]
    fn keeps_text_outside_area() {
        let mut doc = text_page("BT /F1 20 Tf 100 700 Td (Hello) Tj ET", 0);
        let r = Redaction { page: 0, rect: [0.0, 0.0, 50.0, 50.0], fill_white: false };
        apply_edits(&mut doc, &[], &[r], &[]).unwrap();
        assert_eq!(shown_strings(&doc), ["Hello"]);
    }

    #[test]
    fn handles_tj_arrays_and_matrices() {
        // Tm со сдвигом и масштабом 2: шрифт 10 → 20 pt на странице
        let mut doc = text_page("q 1 0 0 1 0 0 cm BT /F1 10 Tf 2 0 0 2 100 700 Tm [(AB) -1000 (CD)] TJ ET Q", 0);
        // A: 100..110, B: 110..120, сдвиг 20, C: 140..150, D: 150..160 — удаляем B и C
        let r = Redaction { page: 0, rect: [112.0, 85.0, 148.0, 105.0], fill_white: false };
        apply_edits(&mut doc, &[], &[r], &[]).unwrap();
        assert_eq!(shown_strings(&doc), ["A", "D"]);
    }

    #[test]
    fn rotated_text_matrix() {
        // Наклон 90° против часовой: текст идёт вверх по экрану (в PDF — вверх по y)
        let g = PageGeometry { boxed: [0.0, 0.0, 600.0, 800.0], rotate: 0 };
        let (dir, next) = axes(90.0);
        let m = g.text_matrix(100.0, 700.0, dir, false);
        assert!((m[0]).abs() < 1e-6 && (m[1] - 1.0).abs() < 1e-6, "{m:?}");
        assert!((m[2] + 1.0).abs() < 1e-6 && m[3].abs() < 1e-6, "{m:?}");
        assert_eq!((m[4], m[5]), (100.0, 100.0));
        // Следующая строка — вправо по экрану
        assert!((next.0 - 1.0).abs() < 1e-6 && next.1.abs() < 1e-6);
    }

    #[test]
    fn eraser_paints_over() {
        let mut doc = text_page("BT /F1 20 Tf 100 700 Td (Hello) Tj ET", 0);
        let erasures = [
            Erase { page: 0, shape: EraseShape::Rect([90.0, 80.0, 200.0, 110.0]), color: [255, 255, 255] },
            Erase { page: 0, shape: EraseShape::Brush { points: vec![[10.0, 10.0], [50.0, 10.0]], width: 8.0 }, color: [255, 0, 0] },
        ];
        let message = apply_edits(&mut doc, &[], &[], &erasures).unwrap();
        assert_eq!(message, "Закрашено областей: 2");
        // Текст остаётся в файле, поверх — закраска
        assert_eq!(shown_strings(&doc), ["Hello"]);
        let page = *doc.get_pages().values().next().unwrap();
        let content = String::from_utf8_lossy(&doc.get_page_content(page)).into_owned();
        assert!(content.contains("90.00 690.00 110.00 30.00 re f"), "{content}");
        assert!(content.contains("8.00 w 10.00 790.00 m 50.00 790.00 l S"), "{content}");
    }

    #[test]
    fn adds_text_with_embedded_font() {
        let mut doc = text_page("", 90);
        let item = TextItem {
            page: 0, x: 50.0, y: 40.0, text: "Привет, мир!\nВторая строка".into(),
            family: Family::Serif, bold: true, italic: true, size: 18.0, color: [200, 0, 0], angle: 0.0,
        };
        let message = apply_edits(&mut doc, &[item], &[], &[]).unwrap();
        assert_eq!(message, "Добавлено надписей: 1");
        let bytes = pdf_ops::save(&mut doc).unwrap();
        let doc = pdf_ops::load(&bytes, "t").unwrap();
        let page = *doc.get_pages().values().next().unwrap();
        let fonts = doc.get_page_fonts(page).unwrap();
        let font = fonts[b"PTF0".as_slice()];
        assert_eq!(font.get(b"Subtype").unwrap().as_name().unwrap(), b"Type0");
        // Встроено подмножество, а не весь шрифт (≈360 КБ)
        assert!(bytes.len() < 60_000, "{}", bytes.len());
    }
}
