# PDF Tool

Десктопное приложение для **Linux**, **Windows** и **macOS**: сборка PDF из изображений, изменение качества, удаление и добавление страниц.

## Возможности

- **Создать PDF из изображений** — PNG, JPEG, BMP, GIF, TIFF, WebP
- **Удалить страницы** — выбор нескольких страниц в списке
- **Добавить страницы** — из изображений или другого PDF, с выбором позиции вставки
- **Изменить качество** — сжатие через Ghostscript (`/screen`–`/prepress`), иначе PyMuPDF
- **Разделить на страницы** — каждая страница в отдельный PDF

## Требования

- Python 3.10+
- Зависимости из `requirements.txt` (PyQt6, pikepdf, Pillow, PyMuPDF)
- Ghostscript — рекомендуется для сжатия PDF

---

## macOS

### 1. Homebrew и Python

Если Homebrew ещё нет: https://brew.sh

```bash
brew install python
```

### 2. Зависимости проекта

```bash
cd pdf-tool
python3 -m venv .venv
source .venv/bin/activate
pip install -r requirements.txt
```

### 3. Ghostscript (для сжатия)

```bash
brew install ghostscript
gs --version
```

### 4. Запуск

```bash
source .venv/bin/activate
python3 main.py
```

Или:

```bash
chmod +x run.sh run.command
./run.sh
```

В Finder можно дважды кликнуть **`run.command`** (откроется Terminal).

> Если при запуске из Finder PyQt6 ругается на библиотеки, всегда запускайте из терминала с активированным `.venv`.

---

## Windows

### 1. Python

Скачайте с [python.org](https://www.python.org/downloads/) (3.10+).  
При установке отметьте **Add python.exe to PATH**.

### 2. Зависимости

```bat
cd путь\к\pdf-tool
python -m venv .venv
.venv\Scripts\activate
pip install -r requirements.txt
```

### 3. Ghostscript (для сжатия)

1. Установщик: https://ghostscript.com/releases/gsdnld.html  
2. Добавьте в PATH папку `bin` (например `C:\Program Files\gs\gs10.xx.x\bin`)  
3. В новом терминале: `gswin64c -version`

### 4. Запуск

```bat
.venv\Scripts\activate
python main.py
```

Или `run.bat`.

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

Или `./run.sh`.

Ярлык в меню (опционально): скопируйте `pdf-tool.desktop` в `~/.local/share/applications/` и поправьте путь в `Exec=`.

---

## Использование

1. **Создать новый документ** / **Открыть PDF** — в правой панели  
2. Вкладка **«Создать PDF»** — собрать документ из картинок  
3. Вкладка **«Страницы»** — удалить, добавить, разделить страницы  
4. Вкладка **«Качество»** — настроить сжатие и применить  
5. **Сохранить как…** — сохранить результат  

Изменения применяются к рабочей копии; исходный файл не перезаписывается до сохранения.
