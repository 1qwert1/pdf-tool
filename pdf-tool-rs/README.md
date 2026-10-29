# PDF Tool (Rust)

Версия PDF Tool на Rust: одна программа без Python, для **Linux**, **Windows** и **macOS**.

Возможности те же, что у Python-версии:

- **Создать PDF из изображений**: PNG, JPEG, BMP, GIF, TIFF, WebP. Учитывается EXIF-поворот, прозрачность заменяется белым фоном, поддерживаются 16-битные изображения и CMYK. JPEG вставляется без перекодирования.
- **Удалять, переставлять и добавлять страницы** из изображений или другого PDF. Закладки и внутренние ссылки сохраняются.
- **Разделить на страницы**: каждая страница сохраняется в отдельный PDF.
- **Сжать**: через Ghostscript, если он установлен, иначе встроенным пересжатием изображений.
- Перетаскивание миниатюр, выделение с Ctrl и Shift, Ctrl+A и Delete. Изображения и PDF можно перетащить прямо в окно. Файл можно передать аргументом: `pdf-tool файл.pdf`.
- Перед закрытием и сменой документа программа спрашивает о несохранённых изменениях. Файлы, защищённые только от редактирования (паролем владельца), открываются.

## Устройство

| Файл | Что делает |
|---|---|
| `src/pdf_ops.rs` | Операции со страницами на [lopdf](https://crates.io/crates/lopdf): дерево страниц перестраивается вручную, чтобы не терять закладки |
| `src/images.rs` | Подготовка изображений (крейт `image`) |
| `src/compress.rs` | Ghostscript или встроенное сжатие |
| `src/render.rs` | Миниатюры через [PDFium](https://pdfium.googlesource.com/pdfium/) в отдельном потоке |
| `src/app.rs` | Интерфейс на [egui](https://github.com/emilk/egui) |

PDFium — динамическая библиотека (`libpdfium.so`, `pdfium.dll`, `libpdfium.dylib`), она лежит рядом с программой. Если её нет, программа работает, но вместо миниатюр показывает номера страниц.

## Сборка для разработки

Нужен Rust 1.95+ ([rustup.rs](https://rustup.rs)). Скачайте PDFium для своей системы с [pdfium-binaries](https://github.com/bblanchon/pdfium-binaries/releases/tag/chromium%2F7881) (версия 7881) и распакуйте в папку `pdfium/`:

```bash
mkdir -p pdfium && curl -sSL https://github.com/bblanchon/pdfium-binaries/releases/download/chromium%2F7881/pdfium-linux-x64.tgz | tar -xz -C pdfium
```

```bash
cargo run
```

```bash
cargo test
```

## Готовые сборки для трёх систем

Сборки делает GitHub Actions (`.github/workflows/rust-release.yml` в корне репозитория):

- **Actions → Rust release → Run workflow**: сборки появятся в артефактах запуска;
- **тег `v*`** (например, `git tag v1.0.0 && git push --tags`): дополнительно создаётся релиз с файлами.

| Система | Файл | Как запустить |
|---|---|---|
| Linux x64 | `pdf-tool-linux-x64.tar.gz` | распаковать и запустить `./pdf-tool` или `./install.sh` (добавит в меню) |
| Windows x64 | `pdf-tool-windows-x64.zip` | распаковать и запустить `pdf-tool.exe` |
| macOS Apple Silicon / Intel | `pdf-tool-macos-arm64.dmg` / `-x64.dmg` | перетащить «PDF Tool» в «Программы» |

Сборки не подписаны сертификатами:

- **macOS** при первом запуске скажет, что разработчик не проверен. Щёлкните по программе правой кнопкой → «Открыть». Чтобы предупреждения не было, нужна подпись Developer ID и нотаризация (аккаунт Apple Developer, $99 в год).
- **Windows** может показать SmartScreen: «Подробнее» → «Выполнить в любом случае».

## Лицензии

PDFium распространяется под BSD-3-Clause (файл `PDFIUM-LICENSE` в сборках). Ghostscript не входит в сборку, программа только вызывает установленный в системе `gs`.
