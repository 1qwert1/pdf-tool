# PDF Tool

A desktop app for **Linux**, **Windows** and **macOS**: build PDFs from images, change quality, delete and add pages. The Python version's interface is in Russian; the Rust version can switch between English and Russian.

This repository contains two versions:

- **Python** (this folder): PyQt6 + pikepdf + PyMuPDF, described below.
- **Rust** ([`pdf-tool-rs/`](pdf-tool-rs/README.md)): a native app with the same features plus text editing, built for all three platforms by GitHub Actions.

## Features (Python version)

- **Create a PDF from images**: PNG, JPEG, BMP, GIF, TIFF, WebP
- **Delete pages**: select several pages in the list
- **Add pages** from images or another PDF, at a chosen position
- **Change quality**: compression via Ghostscript (`/screen`–`/prepress`), otherwise PyMuPDF
- **Split into pages**: each page as a separate PDF

## Requirements

- Python 3.10+
- Dependencies from `requirements.txt` (PyQt6, pikepdf, Pillow, PyMuPDF)
- Ghostscript: recommended for compression

---

## macOS

### 1. Homebrew and Python

If you don't have Homebrew yet: https://brew.sh

```bash
brew install python
```

### 2. Project dependencies

```bash
cd pdf-tool
python3 -m venv .venv
source .venv/bin/activate
pip install -r requirements.txt
```

### 3. Ghostscript (for compression)

```bash
brew install ghostscript
gs --version
```

### 4. Run

```bash
source .venv/bin/activate
python3 main.py
```

Or:

```bash
chmod +x run.sh run.command
./run.sh
```

In Finder you can double-click **`run.command`** (it opens Terminal).

> If PyQt6 complains about libraries when started from Finder, run it from a terminal with `.venv` activated.

---

## Windows

### 1. Python

Download it from [python.org](https://www.python.org/downloads/) (3.10+).
During installation, check **Add python.exe to PATH**.

### 2. Dependencies

```bat
cd path\to\pdf-tool
python -m venv .venv
.venv\Scripts\activate
pip install -r requirements.txt
```

### 3. Ghostscript (for compression)

1. Installer: https://ghostscript.com/releases/gsdnld.html
2. Add its `bin` folder to PATH (for example `C:\Program Files\gs\gs10.xx.x\bin`)
3. In a new terminal: `gswin64c -version`

### 4. Run

```bat
.venv\Scripts\activate
python main.py
```

Or `run.bat`.

---

## Ubuntu / Linux

```bash
cd pdf-tool
sudo apt install python3-venv ghostscript
python3 -m venv .venv
source .venv/bin/activate
pip install -r requirements.txt
python3 main.py
```

Or `./run.sh`.

Menu shortcut (optional): copy `pdf-tool.desktop` to `~/.local/share/applications/` and fix the path in `Exec=`.

---

## Usage

1. **New document** / **Open PDF** in the right panel
2. **Create PDF** tab: build a document from images
3. **Pages** tab: delete, add and split pages
4. **Quality** tab: set the compression level and apply it
5. **Save as…**: save the result

Changes are applied to a working copy; the original file is not overwritten until you save.
