"""Операции с PDF: сборка, сжатие, удаление и добавление страниц."""

from __future__ import annotations

import io
import os
import shutil
import subprocess
import tempfile
from pathlib import Path

import fitz
import pikepdf
from PIL import Image, ImageOps


IMAGE_EXTENSIONS = {".png", ".jpg", ".jpeg", ".bmp", ".gif", ".tiff", ".tif", ".webp"}


def _normalize_image(img: Image.Image) -> Image.Image:
    """Приводит изображение к режиму, который Pillow умеет сохранять в PDF."""
    # Поворот по EXIF (фото с телефонов)
    img = ImageOps.exif_transpose(img)

    if img.mode in ("RGB", "L", "CMYK", "1"):
        return img

    # 16/32-битные изображения: масштабируем в 8 бит
    if img.mode.startswith("I;16") or img.mode == "I":
        scale = 1 / 256 if img.mode.startswith("I;16") or img.getextrema()[1] > 255 else 1
        return img.convert("I").point(lambda v: v * scale).convert("L")
    if img.mode == "F":
        return img.convert("L")

    # Прозрачность: накладываем на белый фон
    if img.mode == "P":
        img = img.convert("RGBA")
    if img.mode in ("RGBA", "LA", "PA", "RGBa", "La"):
        rgba = img.convert("RGBA")
        background = Image.new("RGB", rgba.size, (255, 255, 255))
        background.paste(rgba, mask=rgba.getchannel("A"))
        return background

    return img.convert("RGB")


def _image_to_pdf_bytes(image_path: Path) -> bytes:
    """Конвертирует изображение в одностраничный PDF."""
    with Image.open(image_path) as src:
        img = _normalize_image(src)

    buf = io.BytesIO()
    img.save(buf, format="PDF", resolution=100.0)
    return buf.getvalue()


def create_empty_pdf(output_path: Path) -> None:
    """Создаёт пустой PDF без страниц."""
    pdf = pikepdf.Pdf.new()
    pdf.save(output_path)


def images_to_pdf(image_paths: list[Path], output_path: Path) -> None:
    """Собирает PDF из списка изображений."""
    if not image_paths:
        raise ValueError("Не выбрано ни одного изображения")

    doc = fitz.open()
    try:
        for path in image_paths:
            pdf_bytes = _image_to_pdf_bytes(path)
            src = fitz.open(stream=pdf_bytes, filetype="pdf")
            doc.insert_pdf(src)
            src.close()
        doc.save(str(output_path))
    finally:
        doc.close()


def load_pdf(path: Path) -> pikepdf.Pdf:
    return pikepdf.open(path)


def save_pdf(pdf: pikepdf.Pdf, path: Path) -> None:
    pdf.save(path)


def get_page_count(path: Path) -> int:
    with pikepdf.open(path) as pdf:
        return len(pdf.pages)


def validate_pdf(path: Path) -> int:
    """Проверяет, что PDF можно открыть. Возвращает число страниц или
    бросает ValueError с понятным пользователю сообщением."""
    try:
        return get_page_count(path)
    except pikepdf.PasswordError:
        raise ValueError(f"Файл «{path.name}» защищён паролем.") from None
    except (pikepdf.PdfError, OSError) as e:
        raise ValueError(f"Не удалось прочитать «{path.name}» — файл повреждён "
                         f"или не является PDF.\n\n{e}") from None


def delete_pages(path: Path, pages_to_delete: set[int], output_path: Path) -> None:
    """Удаляет страницы (нумерация с 1). Закладки, ссылки и метаданные сохраняются."""
    doc = fitz.open(str(path))
    try:
        keep = [i for i in range(len(doc)) if (i + 1) not in pages_to_delete]
        if not keep:
            raise ValueError("Нельзя удалить все страницы")

        doc.select(keep)
        doc.save(str(output_path), garbage=3, deflate=True)
    finally:
        doc.close()


def reorder_pages(path: Path, new_order: list[int], output_path: Path) -> None:
    """Переупорядочивает страницы. new_order — номера с 1 в новом порядке.
    Закладки, ссылки и метаданные сохраняются."""
    doc = fitz.open(str(path))
    try:
        total = len(doc)
        if len(new_order) != total or sorted(new_order) != list(range(1, total + 1)):
            raise ValueError("new_order должен быть перестановкой всех номеров страниц")

        if new_order != list(range(1, total + 1)):
            doc.select([n - 1 for n in new_order])
        doc.save(str(output_path), garbage=3, deflate=True)
    finally:
        doc.close()


def split_pdf_to_pages(
    path: Path,
    output_dir: Path,
    pages: list[int] | None = None,
    name_prefix: str | None = None,
) -> list[Path]:
    """
    Разбивает PDF на отдельные одностраничные файлы.
    pages — номера с 1 (None = все). Возвращает пути созданных файлов.
    """
    output_dir.mkdir(parents=True, exist_ok=True)
    prefix = name_prefix or path.stem
    created: list[Path] = []

    with pikepdf.open(path) as pdf:
        total = len(pdf.pages)
        if total == 0:
            raise ValueError("Документ не содержит страниц")

        page_nums = pages if pages is not None else list(range(1, total + 1))
        if not page_nums:
            raise ValueError("Не выбрано ни одной страницы")

        width = max(2, len(str(total)))
        for page_num in page_nums:
            if page_num < 1 or page_num > total:
                raise ValueError(f"Страница {page_num} не существует (всего {total})")

            out_path = output_dir / f"{prefix}_стр_{page_num:0{width}d}.pdf"
            single = pikepdf.Pdf.new()
            single.pages.append(pdf.pages[page_num - 1])
            single.save(out_path)
            created.append(out_path)

    return created


def add_pages_from_images(
    base_path: Path | None,
    image_paths: list[Path],
    insert_after: int,
    output_path: Path,
) -> None:
    """Добавляет страницы из изображений. insert_after=0 — в начало."""
    with tempfile.TemporaryDirectory() as tmp:
        images_pdf = Path(tmp) / "images.pdf"
        images_to_pdf(image_paths, images_pdf)

        if base_path and base_path.exists():
            base_doc = fitz.open(str(base_path))
            add_doc = fitz.open(str(images_pdf))
            try:
                base_doc.insert_pdf(add_doc, start_at=insert_after)
                base_doc.save(str(output_path))
            finally:
                base_doc.close()
                add_doc.close()
        else:
            images_pdf.rename(output_path)


def add_pages_from_pdf(
    base_path: Path,
    source_pdf_path: Path,
    source_pages: list[int] | None,
    insert_after: int,
    output_path: Path,
) -> None:
    """Добавляет страницы из другого PDF. source_pages — номера с 1, None = все."""
    base_doc = fitz.open(str(base_path))
    src_doc = fitz.open(str(source_pdf_path))
    try:
        if source_pages is None:
            base_doc.insert_pdf(src_doc, start_at=insert_after)
        else:
            for page_num in sorted(source_pages):
                if page_num < 1 or page_num > len(src_doc):
                    raise ValueError(f"Страница {page_num} не существует в исходном PDF")
                base_doc.insert_pdf(src_doc, from_page=page_num - 1, to_page=page_num - 1, start_at=insert_after)
                insert_after += 1
        base_doc.save(str(output_path))
    finally:
        base_doc.close()
        src_doc.close()


def _ghostscript_executable() -> str | None:
    """Путь к Ghostscript: gs (Linux/macOS) или gswin64c/gswin32c (Windows)."""
    for name in ("gs", "gswin64c", "gswin32c"):
        path = shutil.which(name)
        if path:
            return path

    # Частые пути Homebrew на macOS (если PATH не подхватился из GUI)
    for candidate in (
        "/opt/homebrew/bin/gs",  # Apple Silicon
        "/usr/local/bin/gs",  # Intel Mac
        "/opt/local/bin/gs",  # MacPorts
    ):
        if Path(candidate).is_file() and os.access(candidate, os.X_OK):
            return candidate

    return None


def ghostscript_available() -> bool:
    """Проверяет, доступен ли Ghostscript в системе."""
    return _ghostscript_executable() is not None


def _quality_to_gs_settings(quality: int) -> str:
    """Сопоставляет ползунок качества с -dPDFSETTINGS Ghostscript."""
    if quality <= 25:
        return "/screen"  # ~72 dpi, макс. сжатие
    if quality <= 50:
        return "/ebook"  # ~150 dpi
    if quality <= 75:
        return "/printer"  # ~300 dpi
    return "/prepress"  # высокое качество


def quality_preset_name(quality: int) -> str:
    """Человекочитаемое имя пресета для UI."""
    settings = _quality_to_gs_settings(quality)
    names = {
        "/screen": "screen — макс. сжатие (~72 dpi)",
        "/ebook": "ebook — для чтения (~150 dpi)",
        "/printer": "printer — для печати (~300 dpi)",
        "/prepress": "prepress — высокое качество",
    }
    return names[settings]


def _compress_with_ghostscript(
    input_path: Path,
    output_path: Path,
    quality: int,
) -> None:
    gs = _ghostscript_executable()
    if not gs:
        raise RuntimeError("Ghostscript не найден")

    pdf_settings = _quality_to_gs_settings(quality)
    tmp_out = output_path.with_suffix(".gs.tmp.pdf")
    cmd = [
        gs,
        "-sDEVICE=pdfwrite",
        "-dCompatibilityLevel=1.4",
        f"-dPDFSETTINGS={pdf_settings}",
        "-dNOPAUSE",
        "-dQUIET",
        "-dBATCH",
        "-dDetectDuplicateImages=true",
        "-dCompressFonts=true",
        "-dSubsetFonts=true",
        f"-sOutputFile={tmp_out}",
        str(input_path),
    ]
    try:
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            check=False,
        )
        if result.returncode != 0 or not tmp_out.exists() or tmp_out.stat().st_size == 0:
            err = (result.stderr or result.stdout or "неизвестная ошибка").strip()
            raise RuntimeError(f"Ghostscript завершился с ошибкой: {err}")
        tmp_out.replace(output_path)
    finally:
        if tmp_out.exists():
            tmp_out.unlink(missing_ok=True)


def _compress_with_pymupdf(
    input_path: Path,
    output_path: Path,
    quality: int,
) -> None:
    """Запасной вариант без Ghostscript."""
    jpeg_quality = int(25 + (quality / 100) * 67)

    if quality <= 30:
        dpi_threshold, dpi_target = 72, 72
    elif quality <= 60:
        dpi_threshold, dpi_target = 120, 96
    elif quality <= 85:
        dpi_threshold, dpi_target = 200, 150
    else:
        dpi_threshold, dpi_target = None, 0

    doc = fitz.open(str(input_path))
    try:
        doc.rewrite_images(
            dpi_threshold=dpi_threshold,
            dpi_target=dpi_target,
            quality=jpeg_quality,
            lossy=True,
            lossless=True,
            bitonal=True,
            color=True,
            gray=True,
            set_to_gray=False,
        )
        doc.save(
            str(output_path),
            garbage=4,
            deflate=True,
            deflate_images=True,
            deflate_fonts=True,
            use_objstms=1,
        )
    finally:
        doc.close()


def compress_pdf(
    input_path: Path,
    output_path: Path,
    quality: int,
) -> str:
    """
    Сжимает PDF. quality: 1 (макс. сжатие) — 100 (макс. качество).
    Предпочитает Ghostscript; при отсутствии — PyMuPDF.
    Возвращает имя использованного движка: "ghostscript" или "pymupdf".
    """
    quality = max(1, min(100, quality))

    if ghostscript_available():
        _compress_with_ghostscript(input_path, output_path, quality)
        return "ghostscript"

    _compress_with_pymupdf(input_path, output_path, quality)
    return "pymupdf"


def render_page_thumbnail(pdf_path: Path, page_index: int, max_size: int = 120) -> bytes:
    """Рендерит миниатюру страницы (page_index с 0)."""
    doc = fitz.open(str(pdf_path))
    try:
        page = doc[page_index]
        rect = page.rect
        scale = min(max_size / rect.width, max_size / rect.height)
        mat = fitz.Matrix(scale, scale)
        pix = page.get_pixmap(matrix=mat, alpha=False)
        return pix.tobytes("png")
    finally:
        doc.close()
