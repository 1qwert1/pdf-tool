//! Операции со страницами PDF: загрузка, удаление, перестановка, вставка, разбиение.
//!
//! Дерево страниц перестраивается вручную (плоский список под корнем /Pages),
//! чтобы закладки, ссылки и метаданные документа не терялись.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use lopdf::{Dictionary, Document, Object, ObjectId, dictionary};

use crate::i18n::tr;
use crate::images::PreparedImage;

/// Атрибуты страницы, которые могут наследоваться от родительских узлов дерева.
const INHERITABLE: [&[u8]; 4] = [b"Resources", b"MediaBox", b"CropBox", b"Rotate"];

/// Размер страницы по умолчанию, если MediaBox не найден (A4).
const DEFAULT_MEDIA_BOX: [f32; 4] = [0.0, 0.0, 595.0, 842.0];

/// Загружает PDF из памяти. Ошибки переводятся в понятные пользователю сообщения.
/// Файлы, защищённые только паролем владельца (запрет редактирования), открываются.
pub fn load(bytes: &[u8], name: &str) -> Result<Document> {
    let mut doc = load_raw(bytes, name)?;
    // lopdf понимает /Encrypt только как ссылку на объект, а некоторые программы
    // (например, MuPDF) пишут словарь прямо в трейлер
    if let Some(patched) = indirect_encrypt(bytes, &doc.trailer) {
        doc = load_raw(&patched, name)?;
    }
    if doc.trailer.has(b"Encrypt") {
        bail!(trf!("Файл «{name}» защищён паролем.", "“{name}” is password protected."));
    }
    pages_root(&doc).with_context(|| trf!("В файле «{name}» нет дерева страниц.", "“{name}” has no page tree."))?;
    Ok(doc)
}

fn load_raw(bytes: &[u8], name: &str) -> Result<Document> {
    Document::load_mem(bytes).map_err(|e| match e {
        lopdf::Error::InvalidPassword | lopdf::Error::Decryption(_) => {
            anyhow!(trf!("Файл «{name}» защищён паролем.", "“{name}” is password protected."))
        }
        e => anyhow!(trf!("Не удалось прочитать «{name}» — файл повреждён или не является PDF.\n\n{e}", "Could not read “{name}”: the file is damaged or not a PDF.\n\n{e}")),
    })
}

/// Если словарь /Encrypt записан прямо в трейлере, дописывает инкрементальное
/// обновление, в котором тот же словарь вынесен в отдельный объект.
fn indirect_encrypt(bytes: &[u8], trailer: &Dictionary) -> Option<Vec<u8>> {
    let encrypt = trailer.get(b"Encrypt").ok()?.as_dict().ok()?;
    let prev_xref = last_startxref(bytes)?;
    let id = u32::try_from(trailer.get(b"Size").and_then(Object::as_i64).ok()?).ok()?;

    let mut out = bytes.to_vec();
    out.push(b'\n');
    let obj_offset = out.len();
    out.extend_from_slice(format!("{id} 0 obj\n").as_bytes());
    write_object(&mut out, &Object::Dictionary(encrypt.clone()));
    out.extend_from_slice(b"\nendobj\n");

    let mut new_trailer = Dictionary::new();
    for key in [b"Root".as_slice(), b"Info", b"ID"] {
        if let Ok(value) = trailer.get(key) {
            new_trailer.set(key, value.clone());
        }
    }
    new_trailer.set("Size", id as i64 + 1);
    new_trailer.set("Prev", prev_xref as i64);
    new_trailer.set("Encrypt", Object::Reference((id, 0)));

    let xref_offset = out.len();
    out.extend_from_slice(format!("xref\n{id} 1\n{obj_offset:010} 00000 n \ntrailer\n").as_bytes());
    write_object(&mut out, &Object::Dictionary(new_trailer));
    out.extend_from_slice(format!("\nstartxref\n{xref_offset}\n%%EOF\n").as_bytes());
    Some(out)
}

/// Смещение последней таблицы xref (число после последнего `startxref`).
fn last_startxref(bytes: &[u8]) -> Option<usize> {
    let tail = &bytes[bytes.len().saturating_sub(1024)..];
    let pos = tail.windows(9).rposition(|w| w == b"startxref")?;
    let digits: String = tail[pos + 9..]
        .iter()
        .map(|&b| b as char)
        .skip_while(|c| c.is_ascii_whitespace())
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// Минимальный сериализатор объектов PDF (для словарей шифрования и трейлера).
fn write_object(out: &mut Vec<u8>, object: &Object) {
    match object {
        Object::Null => out.extend_from_slice(b"null"),
        Object::Boolean(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
        Object::Integer(i) => out.extend_from_slice(i.to_string().as_bytes()),
        Object::Real(r) => out.extend_from_slice(r.to_string().as_bytes()),
        Object::Name(name) => {
            out.push(b'/');
            for &b in name {
                if b.is_ascii_alphanumeric() || b"-_.+".contains(&b) {
                    out.push(b);
                } else {
                    out.extend_from_slice(format!("#{b:02X}").as_bytes());
                }
            }
        }
        Object::String(s, _) => {
            out.push(b'<');
            for b in s {
                out.extend_from_slice(format!("{b:02X}").as_bytes());
            }
            out.push(b'>');
        }
        Object::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b' ');
                }
                write_object(out, item);
            }
            out.push(b']');
        }
        Object::Dictionary(dict) => {
            out.extend_from_slice(b"<<");
            for (key, value) in dict.iter() {
                write_object(out, &Object::Name(key.clone()));
                out.push(b' ');
                write_object(out, value);
                out.push(b' ');
            }
            out.extend_from_slice(b">>");
        }
        Object::Reference((id, gen_)) => out.extend_from_slice(format!("{id} {gen_} R").as_bytes()),
        Object::Stream(_) => out.extend_from_slice(b"null"),
    }
}

/// Загружает PDF из файла.
pub fn load_file(path: &Path) -> Result<(Document, Vec<u8>)> {
    let name = file_name(path);
    let bytes = std::fs::read(path).with_context(|| trf!("Не удалось прочитать «{name}»", "Could not read “{name}”"))?;
    let doc = load(&bytes, &name)?;
    Ok((doc, bytes))
}

pub fn save(doc: &mut Document) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    doc.save_to(&mut out).context(tr("Не удалось записать PDF", "Could not write the PDF"))?;
    Ok(out)
}

pub fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Новый пустой документ без страниц.
pub fn new_document() -> Document {
    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => Vec::<Object>::new(),
            "Count" => 0,
        }),
    );
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    doc.trailer.set("Root", catalog_id);
    doc
}

pub fn page_count(doc: &Document) -> usize {
    doc.get_pages().len()
}

pub(crate) fn page_ids(doc: &Document) -> Vec<ObjectId> {
    doc.get_pages().into_values().collect()
}

fn pages_root(doc: &Document) -> Result<ObjectId> {
    Ok(doc.catalog()?.get(b"Pages")?.as_reference()?)
}

/// Ищет значение наследуемого атрибута у страницы или её предков.
pub(crate) fn inherited_attr(doc: &Document, page_id: ObjectId, key: &[u8]) -> Option<Object> {
    let mut current = Some(page_id);
    let mut visited = HashSet::new();
    while let Some(id) = current {
        if !visited.insert(id) {
            break;
        }
        let dict = doc.get_dictionary(id).ok()?;
        if let Ok(value) = dict.get(key) {
            return Some(value.clone());
        }
        current = dict.get(b"Parent").and_then(Object::as_reference).ok();
    }
    None
}

/// Делает дерево страниц плоским: корень /Pages со страницами `order` в этом порядке.
/// Наследуемые атрибуты переносятся в сами страницы, чтобы ничего не потерять.
fn set_page_order(doc: &mut Document, order: &[ObjectId]) -> Result<()> {
    let root = pages_root(doc)?;

    for &page_id in order {
        let inherited: Vec<(&[u8], Object)> = INHERITABLE
            .iter()
            .filter_map(|key| inherited_attr(doc, page_id, key).map(|v| (*key, v)))
            .collect();
        let page = doc.get_dictionary_mut(page_id)?;
        for (key, value) in inherited {
            if !page.has(key) {
                page.set(key, value);
            }
        }
        if !page.has(b"MediaBox") {
            page.set("MediaBox", DEFAULT_MEDIA_BOX.iter().map(|&v| Object::Real(v)).collect::<Vec<_>>());
        }
        page.set("Parent", root);
    }

    let root_dict = doc.get_dictionary_mut(root)?;
    root_dict.set("Kids", order.iter().map(|&id| Object::Reference(id)).collect::<Vec<_>>());
    root_dict.set("Count", order.len() as i64);
    Ok(())
}

/// Страница, на которую указывает закладка (если это явная ссылка на страницу).
fn outline_target(doc: &Document, item: &Dictionary) -> Option<ObjectId> {
    let dest = match item.get(b"Dest") {
        Ok(dest) => dest.clone(),
        Err(_) => {
            let action = match item.get(b"A").ok()? {
                Object::Reference(id) => doc.get_dictionary(*id).ok()?,
                Object::Dictionary(d) => d,
                _ => return None,
            };
            if action.get(b"S").and_then(Object::as_name).ok()? != b"GoTo" {
                return None;
            }
            action.get(b"D").ok()?.clone()
        }
    };
    let dest = match dest {
        Object::Reference(id) => doc.get_object(id).ok()?.clone(),
        other => other,
    };
    dest.as_array().ok()?.first()?.as_reference().ok()
}

/// Удаляет закладки, ведущие на удалённые страницы. Если у такой закладки есть
/// дочерние, она остаётся, но без перехода.
fn prune_outlines(doc: &mut Document, live_pages: &HashSet<ObjectId>) {
    let Some(outlines_id) = doc
        .catalog()
        .ok()
        .and_then(|c| c.get(b"Outlines").ok())
        .and_then(|o| o.as_reference().ok())
    else {
        return;
    };
    let mut visited = HashSet::new();
    let total = prune_outline_children(doc, outlines_id, live_pages, &mut visited);
    if let Ok(root) = doc.get_dictionary_mut(outlines_id) {
        root.set("Count", total as i64);
    }
}

/// Обрабатывает дочерние закладки узла; возвращает число оставшихся детей.
fn prune_outline_children(
    doc: &mut Document,
    parent_id: ObjectId,
    live_pages: &HashSet<ObjectId>,
    visited: &mut HashSet<ObjectId>,
) -> usize {
    let mut children = Vec::new();
    let mut next = doc
        .get_dictionary(parent_id)
        .ok()
        .and_then(|d| d.get(b"First").ok())
        .and_then(|o| o.as_reference().ok());
    while let Some(id) = next {
        if !visited.insert(id) {
            break;
        }
        children.push(id);
        next = doc
            .get_dictionary(id)
            .ok()
            .and_then(|d| d.get(b"Next").ok())
            .and_then(|o| o.as_reference().ok());
    }

    let mut kept = Vec::new();
    for id in children {
        let kept_children = prune_outline_children(doc, id, live_pages, visited);
        let Ok(item) = doc.get_dictionary(id) else { continue };
        let dead = outline_target(doc, item).is_some_and(|page| !live_pages.contains(&page));
        if dead && kept_children == 0 {
            continue;
        }
        if let Ok(item) = doc.get_dictionary_mut(id) {
            if dead {
                item.remove(b"Dest");
                item.remove(b"A");
            }
            if kept_children > 0 {
                // Сохраняем признак «свёрнуто» (отрицательный Count)
                let closed = item.get(b"Count").and_then(Object::as_i64).is_ok_and(|c| c < 0);
                let count = kept_children as i64;
                item.set("Count", if closed { -count } else { count });
            }
        }
        kept.push(id);
    }

    for (i, &id) in kept.iter().enumerate() {
        if let Ok(item) = doc.get_dictionary_mut(id) {
            match i.checked_sub(1).map(|p| kept[p]) {
                Some(prev) => item.set("Prev", prev),
                None => {
                    item.remove(b"Prev");
                }
            }
            match kept.get(i + 1) {
                Some(&next) => item.set("Next", next),
                None => {
                    item.remove(b"Next");
                }
            }
        }
    }

    if let Ok(parent) = doc.get_dictionary_mut(parent_id) {
        match (kept.first(), kept.last()) {
            (Some(&first), Some(&last)) => {
                parent.set("First", first);
                parent.set("Last", last);
            }
            _ => {
                parent.remove(b"First");
                parent.remove(b"Last");
                parent.remove(b"Count");
            }
        }
    }
    kept.len()
}

/// Удаляет страницы (индексы с 0).
pub fn delete_pages(doc: &mut Document, indices: &BTreeSet<usize>) -> Result<()> {
    let pages = page_ids(doc);
    let (removed, keep): (Vec<_>, Vec<_>) =
        pages.iter().enumerate().partition(|(i, _)| indices.contains(i));
    let keep: Vec<ObjectId> = keep.into_iter().map(|(_, &id)| id).collect();
    if keep.is_empty() {
        bail!(tr("Нельзя удалить все страницы", "You cannot delete all pages"));
    }

    set_page_order(doc, &keep)?;
    prune_outlines(doc, &keep.iter().copied().collect());
    // Ссылки на удалённые страницы (именованные назначения, ссылки в аннотациях)
    // становятся пустыми — по стандарту PDF это null.
    for (_, id) in removed {
        doc.objects.remove(id);
    }
    doc.prune_objects();
    Ok(())
}

/// Переставляет страницы. `order[i]` — старый индекс страницы, которая станет i-й.
pub fn reorder_pages(doc: &mut Document, order: &[usize]) -> Result<()> {
    let pages = page_ids(doc);
    let mut check: Vec<usize> = order.to_vec();
    check.sort_unstable();
    if check != (0..pages.len()).collect::<Vec<_>>() {
        bail!(tr("Новый порядок должен быть перестановкой всех страниц", "The new order must be a permutation of all pages"));
    }
    let new_order: Vec<ObjectId> = order.iter().map(|&i| pages[i]).collect();
    set_page_order(doc, &new_order)
}

/// Вставляет все страницы `other` после страницы `after` (0 — в начало).
/// Возвращает число вставленных страниц.
pub fn insert_document(doc: &mut Document, mut other: Document, after: usize) -> Result<usize> {
    other.renumber_objects_with(doc.max_id + 1);
    let other_pages = page_ids(&other);
    doc.max_id = doc.max_id.max(other.max_id);
    // После перенумерации идентификаторы не пересекаются. Каталог и закладки
    // второго документа ни на что не ссылаются из `doc` и будут удалены при очистке.
    doc.objects.extend(other.objects);

    let mut order = page_ids(doc);
    let at = after.min(order.len());
    order.splice(at..at, other_pages.iter().copied());
    set_page_order(doc, &order)?;
    doc.prune_objects();
    Ok(other_pages.len())
}

/// Добавляет в конец документа по странице на каждое изображение.
pub fn append_images(doc: &mut Document, images: Vec<PreparedImage>) -> Result<()> {
    let root = pages_root(doc)?;
    let mut order = page_ids(doc);
    for image in images {
        let (w_pt, h_pt) = image.page_size_pt();
        let image_id = doc.add_object(image.into_stream());
        let content = format!("q\n{w_pt:.2} 0 0 {h_pt:.2} 0 0 cm\n/Im0 Do\nQ\n");
        let mut content_stream = lopdf::Stream::new(Dictionary::new(), content.into_bytes());
        let _ = content_stream.compress();
        let content_id = doc.add_object(content_stream);
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => root,
            "MediaBox" => vec![0.into(), 0.into(), Object::Real(w_pt), Object::Real(h_pt)],
            "Resources" => dictionary! {
                "XObject" => dictionary! { "Im0" => image_id },
            },
            "Contents" => content_id,
        });
        order.push(page_id);
    }
    set_page_order(doc, &order)
}

/// Документ из изображений.
pub fn images_to_document(images: Vec<PreparedImage>) -> Result<Document> {
    if images.is_empty() {
        bail!(tr("Не выбрано ни одного изображения", "No images selected"));
    }
    let mut doc = new_document();
    append_images(&mut doc, images)?;
    Ok(doc)
}

/// Сохраняет каждую страницу из `indices` (с 0) в отдельный файл. Возвращает пути.
pub fn split_pages(
    doc: &Document,
    indices: &[usize],
    output_dir: &Path,
    prefix: &str,
) -> Result<Vec<PathBuf>> {
    let total = page_count(doc);
    if total == 0 {
        bail!(tr("Документ не содержит страниц", "The document has no pages"));
    }
    if indices.is_empty() {
        bail!(tr("Не выбрано ни одной страницы", "No pages selected"));
    }
    std::fs::create_dir_all(output_dir)
        .with_context(|| trf!("Не удалось создать папку {}", "Could not create folder {}", output_dir.display()))?;

    let width = total.to_string().len().max(2);
    let mut created = Vec::new();
    for &index in indices {
        if index >= total {
            bail!(trf!("Страница {} не существует (всего {total})", "Page {} does not exist (total {total})", index + 1));
        }
        let mut single = doc.clone();
        let others: BTreeSet<usize> = (0..total).filter(|&i| i != index).collect();
        if !others.is_empty() {
            delete_pages(&mut single, &others)?;
        }
        let path = output_dir.join(trf!("{prefix}_стр_{:0width$}.pdf", "{prefix}_page_{:0width$}.pdf", index + 1));
        single
            .save(&path)
            .with_context(|| trf!("Не удалось сохранить {}", "Could not save {}", path.display()))?;
        created.push(path);
    }
    Ok(created)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Документ из `n` пустых страниц с закладкой на каждую.
    fn sample(n: usize) -> Document {
        let mut doc = new_document();
        let root = pages_root(&doc).unwrap();
        let mut pages = Vec::new();
        for i in 0..n {
            let page = doc.add_object(dictionary! {
                "Type" => "Page",
                "Parent" => root,
                "MediaBox" => vec![0.into(), 0.into(), (100 + i as i64).into(), 100.into()],
            });
            pages.push(page);
        }
        set_page_order(&mut doc, &pages).unwrap();

        let outlines = doc.new_object_id();
        let items: Vec<ObjectId> = (0..n).map(|_| doc.new_object_id()).collect();
        for (i, &id) in items.iter().enumerate() {
            let mut item = dictionary! {
                "Title" => Object::string_literal(format!("B{}", i + 1)),
                "Parent" => outlines,
                "Dest" => vec![pages[i].into(), "Fit".into()],
            };
            if i > 0 {
                item.set("Prev", items[i - 1]);
            }
            if i + 1 < n {
                item.set("Next", items[i + 1]);
            }
            doc.objects.insert(id, Object::Dictionary(item));
        }
        doc.objects.insert(
            outlines,
            Object::Dictionary(dictionary! {
                "Type" => "Outlines",
                "First" => items[0],
                "Last" => items[n - 1],
                "Count" => n as i64,
            }),
        );
        doc.catalog_mut().unwrap().set("Outlines", outlines);
        doc
    }

    fn roundtrip(doc: &mut Document) -> Document {
        load(&save(doc).unwrap(), "test").unwrap()
    }

    /// Ширины MediaBox страниц — по ним видно, какая страница где.
    fn widths(doc: &Document) -> Vec<i64> {
        page_ids(doc)
            .iter()
            .map(|&id| {
                let mb = inherited_attr(doc, id, b"MediaBox").unwrap();
                mb.as_array().unwrap()[2].as_i64().unwrap()
            })
            .collect()
    }

    /// Заголовки закладок и номера страниц, на которые они ведут (с 1).
    fn toc(doc: &Document) -> Vec<(String, usize)> {
        let pages = page_ids(doc);
        let outlines = doc.catalog().unwrap().get(b"Outlines").unwrap().as_reference().unwrap();
        let mut result = Vec::new();
        let mut next = doc.get_dictionary(outlines).unwrap().get(b"First").ok().map(|o| o.as_reference().unwrap());
        while let Some(id) = next {
            let item = doc.get_dictionary(id).unwrap();
            let title = String::from_utf8_lossy(item.get(b"Title").unwrap().as_str().unwrap()).into_owned();
            let page = outline_target(doc, item).unwrap();
            result.push((title, pages.iter().position(|&p| p == page).unwrap() + 1));
            next = item.get(b"Next").ok().map(|o| o.as_reference().unwrap());
        }
        result
    }

    #[test]
    fn delete_keeps_bookmarks() {
        let mut doc = sample(3);
        delete_pages(&mut doc, &BTreeSet::from([1])).unwrap();
        let doc = roundtrip(&mut doc);
        assert_eq!(widths(&doc), [100, 102]);
        assert_eq!(toc(&doc), [("B1".into(), 1), ("B3".into(), 2)]);
    }

    #[test]
    fn delete_all_fails() {
        let mut doc = sample(2);
        assert!(delete_pages(&mut doc, &BTreeSet::from([0, 1])).is_err());
    }

    #[test]
    fn reorder_keeps_bookmarks() {
        let mut doc = sample(3);
        reorder_pages(&mut doc, &[2, 0, 1]).unwrap();
        let doc = roundtrip(&mut doc);
        assert_eq!(widths(&doc), [102, 100, 101]);
        assert_eq!(toc(&doc), [("B1".into(), 2), ("B2".into(), 3), ("B3".into(), 1)]);
    }

    #[test]
    fn insert_other_document() {
        let mut doc = sample(2);
        let other = sample(3);
        assert_eq!(insert_document(&mut doc, other, 1).unwrap(), 3);
        let doc = roundtrip(&mut doc);
        assert_eq!(widths(&doc), [100, 100, 101, 102, 101]);
        // Закладки исходного документа указывают на те же страницы
        assert_eq!(toc(&doc), [("B1".into(), 1), ("B2".into(), 5)]);
    }

    #[test]
    fn insert_into_empty_document() {
        let mut doc = new_document();
        insert_document(&mut doc, sample(2), 0).unwrap();
        assert_eq!(page_count(&roundtrip(&mut doc)), 2);
    }

    #[test]
    fn split_writes_single_pages() {
        let doc = sample(3);
        let dir = tempfile::tempdir().unwrap();
        let files = split_pages(&doc, &[0, 2], dir.path(), "doc").unwrap();
        assert_eq!(files.len(), 2);
        assert!(files[1].ends_with("doc_стр_03.pdf"));
        let (single, _) = load_file(&files[1]).unwrap();
        assert_eq!(widths(&single), [102]);
    }

    #[test]
    fn rejects_garbage() {
        let err = load(b"not a pdf", "bad.pdf").unwrap_err().to_string();
        assert!(err.contains("повреждён"), "{err}");
    }
}
