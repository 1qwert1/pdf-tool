# PDF Tool (Rust)

The Rust version of PDF Tool: a single native program for **Linux**, **Windows** and **macOS**, with no Python required. The user interface is available in English and Russian.

## Features

- **Create a PDF from images**: PNG, JPEG, BMP, GIF, TIFF, WebP. EXIF rotation is applied, transparency is flattened onto white, 16-bit and CMYK images are supported. JPEG files are embedded without re-encoding.
- **Delete, reorder and insert pages** from images or another PDF. Bookmarks and internal links are preserved.
- **Split into pages**: each page is saved as a separate PDF.
- **Edit pages** (double-click a page or use the Text tab):
  - add text anywhere: click on the page and type right there. Three fonts with Cyrillic support (DejaVu Sans, Serif, Mono), bold, italic, size, color and any angle (rotation handle or angle field, 1° steps). Until applied, captions can be moved, rotated, edited and removed. Only the glyphs actually used are embedded, and the added text is searchable and copyable;
  - remove text inside a selected area: the characters are cut out of the file and the rest of the line stays in place. Optionally the area is painted white to hide images as well;
  - eraser: paint over any part of the page with a brush or a rectangle in any color (the content underneath stays in the file);
  - undo with Ctrl+Z.
- **Compress** without external tools: photos are downsampled to their real on-page resolution (72–300 dpi) and re-encoded as JPEG; document scans (mostly white pages, including color scans without color and 1–4-bit grayscale scans) are stored in 2, 4 or 16 shades of gray with lossless Flate, never below 150 dpi so text stays readable. Duplicate images are merged and the PDF structure is packed into object streams. If the file cannot be made smaller, the document is left unchanged.
- Drag thumbnails to reorder, click to select (Ctrl/Shift for multiple), right-click for a context menu, Ctrl+A and Delete. Images and PDFs can be dropped onto the window. A file can be passed on the command line: `pdf-tool file.pdf`.
- Interface language: switch between EN and RU in the top-right corner; the choice is remembered. On first launch the system language is used (Russian for `ru` locales, English otherwise). `PDF_TOOL_LANG=en|ru` overrides it.
- The app asks before discarding unsaved changes. Files protected only by an owner password (editing restrictions) can be opened.

## Layout

| File | Purpose |
|---|---|
| `src/pdf_ops.rs` | Page operations on top of [lopdf](https://crates.io/crates/lopdf); the page tree is rebuilt manually so bookmarks are kept |
| `src/images.rs` | Image preparation (`image` crate) |
| `src/compress.rs` | Native compression |
| `src/edit.rs` | Text with embedded fonts and text removal in an area |
| `src/editor.rs` | Page editor |
| `src/render.rs` | Thumbnails via [PDFium](https://pdfium.googlesource.com/pdfium/) on a background thread |
| `src/app.rs` | User interface built with [egui](https://github.com/emilk/egui) |
| `src/i18n.rs` | Interface language (English / Russian) |

PDFium is a dynamic library (`libpdfium.so`, `pdfium.dll`, `libpdfium.dylib`) shipped next to the executable. Without it the app still works, but shows page numbers instead of thumbnails.

## Development build

Requires Rust 1.95+ ([rustup.rs](https://rustup.rs)). Download PDFium for your platform from [pdfium-binaries](https://github.com/bblanchon/pdfium-binaries/releases/tag/chromium%2F7881) (build 7881) and unpack it into `pdfium/`:

```bash
mkdir -p pdfium && curl -sSL https://github.com/bblanchon/pdfium-binaries/releases/download/chromium%2F7881/pdfium-linux-x64.tgz | tar -xz -C pdfium
```

```bash
cargo run
```

```bash
cargo test
```

## Release builds for all three platforms

Builds are produced by GitHub Actions (`.github/workflows/rust-release.yml` in the repository root):

- **Actions → Rust release → Run workflow**: builds are attached to the workflow run as artifacts;
- **a `v*` tag** (for example `git tag v1.0.0 && git push --tags`): a GitHub release with the files is created as well.

| Platform | File | How to run |
|---|---|---|
| Linux x64 | `pdf-tool-linux-x64.tar.gz` | unpack and run `./pdf-tool`, or `./install.sh` to add it to the application menu |
| Windows x64 | `pdf-tool-windows-x64.zip` | unpack and run `pdf-tool.exe` |
| macOS Apple Silicon / Intel | `pdf-tool-macos-arm64.dmg` / `-x64.dmg` | drag PDF Tool to Applications |

The builds are not signed:

- **macOS** will say the developer cannot be verified on first launch. Right-click the app → Open. Avoiding the warning requires a Developer ID signature and notarization (Apple Developer account, $99/year).
- **Windows** may show SmartScreen: More info → Run anyway.

## Licenses

PDFium is distributed under BSD-3-Clause (`PDFIUM-LICENSE` in the builds). The DejaVu fonts are under the Bitstream Vera license (`fonts/LICENSE`), which permits embedding.
