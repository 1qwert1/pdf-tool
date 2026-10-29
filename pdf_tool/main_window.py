"""Главное окно приложения PDF Tool."""

from __future__ import annotations

import shutil
import sys
import tempfile
import traceback
import uuid
from pathlib import Path

from PyQt6.QtCore import Qt
from PyQt6.QtGui import QAction, QColor, QKeySequence
from PyQt6.QtWidgets import (
    QApplication,
    QDialog,
    QDialogButtonBox,
    QFileDialog,
    QFormLayout,
    QFrame,
    QGraphicsDropShadowEffect,
    QGroupBox,
    QHBoxLayout,
    QLabel,
    QMainWindow,
    QMessageBox,
    QPushButton,
    QScrollArea,
    QSizePolicy,
    QSlider,
    QSpinBox,
    QStatusBar,
    QTabWidget,
    QVBoxLayout,
    QWidget,
)

from pdf_tool.pdf_operations import (
    add_pages_from_images,
    add_pages_from_pdf,
    compress_pdf,
    create_empty_pdf,
    delete_pages,
    get_page_count,
    ghostscript_available,
    images_to_pdf,
    quality_preset_name,
    reorder_pages,
    split_pdf_to_pages,
    validate_pdf,
)
from pdf_tool.widgets.page_list import PageListWidget

# Material / Google-style button styles
_BTN_BASE = """
    border-radius: 2px;
    padding: 10px 18px;
    font-size: 13px;
    font-weight: 500;
    min-height: 20px;
"""

_BTN_DEFAULT = f"""
QPushButton {{
    background-color: #ffffff;
    color: #3c4043;
    border: 1px solid #dadce0;
    {_BTN_BASE}
}}
QPushButton:hover {{
    background-color: #f8f9fa;
    border-color: #dadce0;
    color: #202124;
}}
QPushButton:pressed {{
    background-color: #f1f3f4;
}}
QPushButton:disabled {{
    background-color: #f1f3f4;
    color: #9aa0a6;
    border-color: #e8eaed;
}}
"""

_BTN_PRIMARY = f"""
QPushButton {{
    background-color: #1a73e8;
    color: #ffffff;
    border: none;
    {_BTN_BASE}
}}
QPushButton:hover {{
    background-color: #1765cc;
}}
QPushButton:pressed {{
    background-color: #1557b0;
}}
QPushButton:disabled {{
    background-color: #a8c7fa;
    color: #ffffff;
}}
"""

_BTN_DANGER = f"""
QPushButton {{
    background-color: #ffffff;
    color: #d93025;
    border: 1px solid #dadce0;
    {_BTN_BASE}
}}
QPushButton:hover {{
    background-color: #fce8e6;
    border-color: #f6aea9;
}}
QPushButton:pressed {{
    background-color: #fad2cf;
}}
"""

_BTN_ICON = f"""
QPushButton {{
    background-color: #ffffff;
    color: #3c4043;
    border: 1px solid #dadce0;
    border-radius: 2px;
    padding: 8px 12px;
    font-size: 14px;
    font-weight: 500;
    min-width: 40px;
}}
QPushButton:hover {{
    background-color: #f8f9fa;
}}
QPushButton:pressed {{
    background-color: #f1f3f4;
}}
"""


def _make_button(
    text: str,
    *,
    primary: bool = False,
    danger: bool = False,
    icon: bool = False,
    tooltip: str = "",
) -> QPushButton:
    btn = QPushButton(text)
    if tooltip:
        btn.setToolTip(tooltip)
    if icon:
        btn.setStyleSheet(_BTN_ICON)
    elif primary:
        btn.setStyleSheet(_BTN_PRIMARY)
    elif danger:
        btn.setStyleSheet(_BTN_DANGER)
    else:
        btn.setStyleSheet(_BTN_DEFAULT)

    shadow = QGraphicsDropShadowEffect(btn)
    shadow.setBlurRadius(10)
    shadow.setXOffset(0)
    shadow.setYOffset(2)
    shadow.setColor(QColor(60, 64, 67, 45))
    btn.setGraphicsEffect(shadow)
    btn.setCursor(Qt.CursorShape.PointingHandCursor)
    return btn


def _make_scroll_area(content: QWidget) -> QScrollArea:
    """Прокручиваемая область для содержимого вкладок / правой панели."""
    scroll = QScrollArea()
    scroll.setWidgetResizable(True)
    scroll.setFrameShape(QFrame.Shape.NoFrame)
    scroll.setHorizontalScrollBarPolicy(Qt.ScrollBarPolicy.ScrollBarAlwaysOff)
    scroll.setVerticalScrollBarPolicy(Qt.ScrollBarPolicy.ScrollBarAsNeeded)
    scroll.setSizePolicy(QSizePolicy.Policy.Expanding, QSizePolicy.Policy.Expanding)
    content.setSizePolicy(QSizePolicy.Policy.Expanding, QSizePolicy.Policy.Minimum)
    scroll.setWidget(content)
    return scroll


class InsertPositionDialog(QDialog):
    def __init__(self, max_page: int, parent=None):
        super().__init__(parent)
        self.setWindowTitle("Позиция вставки")
        layout = QFormLayout(self)

        self.spin = QSpinBox()
        self.spin.setMinimum(0)
        self.spin.setMaximum(max_page)
        self.spin.setValue(max_page)
        self.spin.setSpecialValueText("В начало документа")
        layout.addRow("Вставить после страницы:", self.spin)

        hint = QLabel("0 = в начало, N = после страницы N")
        hint.setStyleSheet("color: gray; font-size: 11px;")
        layout.addRow(hint)

        buttons = QDialogButtonBox(
            QDialogButtonBox.StandardButton.Ok | QDialogButtonBox.StandardButton.Cancel
        )
        buttons.accepted.connect(self.accept)
        buttons.rejected.connect(self.reject)
        layout.addRow(buttons)

    def position(self) -> int:
        return self.spin.value()


class MainWindow(QMainWindow):
    def __init__(self):
        super().__init__()
        self.setWindowTitle("PDF Tool")
        self.setMinimumSize(900, 600)
        self.resize(1100, 700)

        self._current_pdf: Path | None = None
        self._temp_dir = tempfile.mkdtemp(prefix="pdf-tool-")
        self._working_pdf: Path | None = None
        self._dirty = False  # есть несохранённые изменения

        self._build_menu()
        self._build_ui()
        self._update_ui_state()

    def _build_menu(self) -> None:
        menubar = self.menuBar()

        file_menu = menubar.addMenu("Файл")

        new_action = QAction("Новый документ", self)
        new_action.setShortcut(QKeySequence.StandardKey.New)
        new_action.triggered.connect(self._new_document)
        file_menu.addAction(new_action)

        open_action = QAction("Открыть PDF…", self)
        open_action.setShortcut(QKeySequence.StandardKey.Open)
        open_action.triggered.connect(self._open_pdf)
        file_menu.addAction(open_action)

        save_action = QAction("Сохранить как…", self)
        save_action.setShortcut(QKeySequence.StandardKey.SaveAs)
        save_action.triggered.connect(self._save_pdf)
        file_menu.addAction(save_action)

        file_menu.addSeparator()

        quit_action = QAction("Выход", self)
        quit_action.setShortcut(QKeySequence.StandardKey.Quit)
        quit_action.triggered.connect(self.close)
        file_menu.addAction(quit_action)

    def _build_ui(self) -> None:
        central = QWidget()
        self.setCentralWidget(central)
        main_layout = QHBoxLayout(central)
        main_layout.setSpacing(16)
        main_layout.setContentsMargins(12, 12, 12, 12)

        # Левая панель — список страниц
        left_panel = QVBoxLayout()
        pages_label = QLabel("Страницы документа")
        pages_label.setStyleSheet("font-weight: bold; font-size: 14px; color: #202124;")
        left_panel.addWidget(pages_label)

        drag_hint = QLabel("Перетащите миниатюры, чтобы изменить порядок")
        drag_hint.setStyleSheet("color: #5f6368; font-size: 11px;")
        left_panel.addWidget(drag_hint)

        self.page_list = PageListWidget()
        self.page_list.selection_changed.connect(self._on_page_selection)
        self.page_list.order_changed.connect(self._on_order_changed)
        left_panel.addWidget(self.page_list)

        reorder_btns = QHBoxLayout()
        reorder_btns.setSpacing(8)
        btn_to_start = _make_button("⏮", icon=True, tooltip="В начало")
        btn_to_start.clicked.connect(lambda: self._move_pages("start"))
        btn_up = _make_button("▲", icon=True, tooltip="Выше")
        btn_up.clicked.connect(lambda: self._move_pages("up"))
        btn_down = _make_button("▼", icon=True, tooltip="Ниже")
        btn_down.clicked.connect(lambda: self._move_pages("down"))
        btn_to_end = _make_button("⏭", icon=True, tooltip="В конец")
        btn_to_end.clicked.connect(lambda: self._move_pages("end"))
        for btn in (btn_to_start, btn_up, btn_down, btn_to_end):
            reorder_btns.addWidget(btn)
        left_panel.addLayout(reorder_btns)

        self.page_info_label = QLabel("Документ не открыт")
        self.page_info_label.setStyleSheet("color: #5f6368;")
        left_panel.addWidget(self.page_info_label)

        main_layout.addLayout(left_panel, stretch=2)

        # Правая панель — целиком в QScrollArea
        right_inner = QWidget()
        right_panel = QVBoxLayout(right_inner)
        right_panel.setSpacing(12)
        right_panel.setContentsMargins(8, 4, 12, 8)

        doc_label = QLabel("Документ")
        doc_label.setStyleSheet("font-weight: bold; font-size: 14px; color: #202124;")
        right_panel.addWidget(doc_label)

        btn_new = _make_button("Создать новый документ", primary=True)
        btn_new.clicked.connect(self._new_document)
        right_panel.addWidget(btn_new)

        btn_open = _make_button("Открыть PDF…")
        btn_open.clicked.connect(self._open_pdf)
        right_panel.addWidget(btn_open)

        btn_save = _make_button("Сохранить как…", primary=True)
        btn_save.clicked.connect(self._save_pdf)
        right_panel.addWidget(btn_save)

        tools = QTabWidget()

        # --- Вкладка: Создать PDF из изображений ---
        create_tab = QWidget()
        create_layout = QVBoxLayout(create_tab)
        create_layout.setContentsMargins(8, 8, 8, 8)

        create_desc = QLabel(
            "Выберите изображения и соберите из них PDF-документ.\n"
            "Поддерживаются: PNG, JPEG, BMP, GIF, TIFF, WebP."
        )
        create_desc.setWordWrap(True)
        create_desc.setStyleSheet("color: #3c4043;")
        create_layout.addWidget(create_desc)

        btn_select_images = _make_button("Выбрать изображения…")
        btn_select_images.clicked.connect(self._create_pdf_from_images)
        create_layout.addWidget(btn_select_images)

        create_layout.addStretch()
        tools.addTab(create_tab, "Создать PDF")

        # --- Вкладка: Управление страницами ---
        pages_tab = QWidget()
        pages_layout = QVBoxLayout(pages_tab)
        pages_layout.setContentsMargins(8, 8, 8, 8)
        pages_layout.setSpacing(10)

        reorder_group = QGroupBox("Порядок страниц")
        reorder_layout = QVBoxLayout(reorder_group)
        reorder_hint = QLabel(
            "Перетащите миниатюры слева или выберите страницы "
            "и нажмите кнопки ▲ ▼ ⏮ ⏭ под списком."
        )
        reorder_hint.setWordWrap(True)
        reorder_hint.setStyleSheet("color: #3c4043;")
        reorder_layout.addWidget(reorder_hint)
        pages_layout.addWidget(reorder_group)

        delete_group = QGroupBox("Удаление страниц")
        delete_layout = QVBoxLayout(delete_group)
        delete_hint = QLabel("Выберите страницы в списке слева и нажмите «Удалить».")
        delete_hint.setWordWrap(True)
        delete_layout.addWidget(delete_hint)
        btn_delete = _make_button("Удалить выбранные страницы", danger=True)
        btn_delete.clicked.connect(self._delete_selected_pages)
        delete_layout.addWidget(btn_delete)
        pages_layout.addWidget(delete_group)

        add_group = QGroupBox("Добавление страниц")
        add_layout = QVBoxLayout(add_group)
        add_layout.setSpacing(10)

        btn_add_images = _make_button("Добавить изображения…")
        btn_add_images.clicked.connect(self._add_images)
        add_layout.addWidget(btn_add_images)

        btn_add_pdf = _make_button("Добавить страницы из PDF…")
        btn_add_pdf.clicked.connect(self._add_from_pdf)
        add_layout.addWidget(btn_add_pdf)

        pages_layout.addWidget(add_group)

        split_group = QGroupBox("Разделение на страницы")
        split_layout = QVBoxLayout(split_group)
        split_layout.setSpacing(10)
        split_hint = QLabel(
            "Каждая страница сохраняется как отдельный PDF-файл в выбранную папку."
        )
        split_hint.setWordWrap(True)
        split_layout.addWidget(split_hint)
        btn_split_all = _make_button("Разделить все страницы…")
        btn_split_all.clicked.connect(lambda: self._split_pages(selected_only=False))
        split_layout.addWidget(btn_split_all)

        btn_split_selected = _make_button("Разделить выбранные страницы…")
        btn_split_selected.clicked.connect(lambda: self._split_pages(selected_only=True))
        split_layout.addWidget(btn_split_selected)

        pages_layout.addWidget(split_group)
        pages_layout.addStretch()
        tools.addTab(pages_tab, "Страницы")

        # --- Вкладка: Качество ---
        quality_tab = QWidget()
        quality_layout = QVBoxLayout(quality_tab)
        quality_layout.setContentsMargins(8, 8, 8, 8)

        quality_desc = QLabel(
            "Измените качество PDF через Ghostscript (если установлен) "
            "или встроенный движок. Меньшее значение — меньший размер файла."
        )
        quality_desc.setWordWrap(True)
        quality_desc.setStyleSheet("color: #3c4043;")
        quality_layout.addWidget(quality_desc)

        self.engine_label = QLabel()
        self.engine_label.setStyleSheet("color: #5f6368; font-size: 11px;")
        self.engine_label.setWordWrap(True)
        if ghostscript_available():
            self.engine_label.setText("Движок: Ghostscript")
        else:
            self.engine_label.setText(
                "Движок: PyMuPDF (установите ghostscript для лучшего сжатия)"
            )
        quality_layout.addWidget(self.engine_label)

        slider_layout = QHBoxLayout()
        slider_layout.addWidget(QLabel("Качество:"))

        self.quality_slider = QSlider(Qt.Orientation.Horizontal)
        self.quality_slider.setMinimum(1)
        self.quality_slider.setMaximum(100)
        self.quality_slider.setValue(50)
        self.quality_slider.setTickPosition(QSlider.TickPosition.TicksBelow)
        self.quality_slider.setTickInterval(25)
        self.quality_slider.valueChanged.connect(self._update_quality_label)
        slider_layout.addWidget(self.quality_slider)

        self.quality_label = QLabel("50")
        self.quality_label.setMinimumWidth(30)
        slider_layout.addWidget(self.quality_label)

        quality_layout.addLayout(slider_layout)

        self.quality_size_hint = QLabel("")
        self.quality_size_hint.setStyleSheet("color: #5f6368;")
        self.quality_size_hint.setWordWrap(True)
        quality_layout.addWidget(self.quality_size_hint)
        self._update_quality_label(50)

        btn_compress = _make_button("Применить сжатие")
        btn_compress.clicked.connect(self._compress_current_pdf)
        quality_layout.addWidget(btn_compress)

        quality_layout.addStretch()
        tools.addTab(quality_tab, "Качество")

        # Высота вкладок по содержимому «Страницы», чтобы скролл правой панели работал
        tools.setMinimumHeight(520)
        right_panel.addWidget(tools)

        right_scroll = _make_scroll_area(right_inner)
        right_scroll.setMinimumWidth(300)
        main_layout.addWidget(right_scroll, stretch=1)

        self.status_bar = QStatusBar()
        self.setStatusBar(self.status_bar)

        self.setStyleSheet("""
            QMainWindow {
                background-color: #f8f9fa;
            }
            QMenuBar {
                background-color: #ffffff;
                color: #202124;
                border-bottom: 1px solid #dadce0;
                padding: 2px;
            }
            QMenuBar::item {
                background: transparent;
                color: #202124;
                padding: 6px 12px;
                border-radius: 2px;
            }
            QMenuBar::item:selected {
                background-color: #e8f0fe;
                color: #1967d2;
            }
            QMenuBar::item:pressed {
                background-color: #d2e3fc;
                color: #174ea6;
            }
            QMenu {
                background-color: #ffffff;
                color: #202124;
                border: 1px solid #dadce0;
                border-radius: 2px;
                padding: 4px;
            }
            QMenu::item {
                background-color: transparent;
                color: #202124;
                padding: 8px 28px 8px 16px;
                border-radius: 2px;
            }
            QMenu::item:selected {
                background-color: #e8f0fe;
                color: #1967d2;
            }
            QMenu::item:disabled {
                color: #9aa0a6;
            }
            QMenu::separator {
                height: 1px;
                background: #dadce0;
                margin: 4px 8px;
            }
            QTabWidget::pane {
                border: 1px solid #dadce0;
                border-radius: 2px;
                background: #ffffff;
                top: -1px;
            }
            QTabBar::tab {
                background: #f1f3f4;
                color: #5f6368;
                padding: 8px 16px;
                border: 1px solid #dadce0;
                border-bottom: none;
                border-top-left-radius: 2px;
                border-top-right-radius: 2px;
                margin-right: 2px;
            }
            QTabBar::tab:selected {
                background: #ffffff;
                color: #1a73e8;
                font-weight: 500;
            }
            QGroupBox {
                font-weight: 500;
                color: #202124;
                border: 1px solid #dadce0;
                border-radius: 2px;
                margin-top: 12px;
                padding-top: 12px;
                background: #ffffff;
            }
            QGroupBox::title {
                subcontrol-origin: margin;
                left: 10px;
                padding: 0 4px;
            }
            QStatusBar {
                background: #ffffff;
                color: #5f6368;
                border-top: 1px solid #dadce0;
            }
            QScrollArea {
                background: transparent;
                border: none;
            }
            QScrollBar:vertical {
                background: #f1f3f4;
                width: 10px;
                margin: 0;
                border-radius: 2px;
            }
            QScrollBar::handle:vertical {
                background: #bdc1c6;
                min-height: 30px;
                border-radius: 2px;
            }
            QScrollBar::handle:vertical:hover {
                background: #9aa0a6;
            }
            QScrollBar::add-line:vertical,
            QScrollBar::sub-line:vertical {
                height: 0;
            }
        """)

    def _update_quality_label(self, value: int) -> None:
        self.quality_label.setText(str(value))
        if ghostscript_available():
            self.quality_size_hint.setText(f"Пресет Ghostscript: {quality_preset_name(value)}")
        else:
            if value >= 90:
                hint = "Высокое качество, минимальное сжатие"
            elif value >= 60:
                hint = "Сбалансированное качество"
            elif value >= 30:
                hint = "Среднее сжатие"
            else:
                hint = "Максимальное сжатие, низкое качество"
            self.quality_size_hint.setText(hint)

    def _temp_path(self, stem: str) -> Path:
        """Уникальный путь во временной папке — никогда не совпадает с рабочим файлом."""
        return Path(self._temp_dir) / f"{stem}_{uuid.uuid4().hex[:8]}.pdf"

    def _confirm_discard(self, title: str) -> bool:
        """Спрашивает, можно ли потерять несохранённые изменения."""
        if not self._dirty:
            return True
        reply = QMessageBox.question(
            self,
            title,
            "Есть несохранённые изменения. Они будут потеряны.\nПродолжить?",
            QMessageBox.StandardButton.Yes | QMessageBox.StandardButton.No,
            QMessageBox.StandardButton.No,
        )
        return reply == QMessageBox.StandardButton.Yes

    def _update_ui_state(self) -> None:
        has_doc = self._working_pdf is not None and self._working_pdf.exists()
        if has_doc:
            name = self._current_pdf.name if self._current_pdf else "Новый документ"
            if self._dirty:
                name += " *"
            size_kb = self._working_pdf.stat().st_size / 1024
            try:
                count = get_page_count(self._working_pdf)
                self.page_info_label.setText(f"{name} — {count} стр., {size_kb:.0f} КБ")
            except Exception:
                self.page_info_label.setText(f"{name} — {size_kb:.0f} КБ (ошибка чтения)")
        else:
            self.page_info_label.setText("Документ не открыт")

    def _reload_pages(self) -> None:
        try:
            if self._working_pdf and self._working_pdf.exists():
                self.page_list.load_pdf(self._working_pdf)
            else:
                self.page_list.clear()
        except Exception as e:
            self.page_list.clear()
            QMessageBox.critical(self, "Ошибка", f"Не удалось отобразить страницы:\n{e}")
        self._update_ui_state()

    def _on_page_selection(self, pages: list[int]) -> None:
        if pages:
            self.status_bar.showMessage(f"Выбрано страниц: {len(pages)} — {pages}")
        else:
            self.status_bar.showMessage("")

    def _move_pages(self, action: str) -> None:
        if not self._working_pdf:
            QMessageBox.warning(self, "Нет документа", "Сначала откройте PDF.")
            return
        if not self.page_list.get_selected_pages():
            QMessageBox.information(self, "Не выбраны страницы", "Выберите страницы для перемещения.")
            return

        if action == "up":
            self.page_list.move_selected(-1)
        elif action == "down":
            self.page_list.move_selected(1)
        elif action == "start":
            self.page_list.move_selected_to_edge(to_start=True)
        elif action == "end":
            self.page_list.move_selected_to_edge(to_start=False)

    def _on_order_changed(self, new_order: list[int]) -> None:
        if not self._working_pdf or not self._working_pdf.exists():
            return

        expected = list(range(1, len(new_order) + 1))
        if new_order == expected:
            return

        dest = self._temp_path("reordered")
        try:
            reorder_pages(self._working_pdf, new_order, dest)
            shutil.move(dest, self._working_pdf)
        except Exception as e:
            QMessageBox.critical(self, "Ошибка", f"Не удалось изменить порядок:\n{e}")
            self._reload_pages()
            return

        self._dirty = True
        self._reload_pages()
        self.status_bar.showMessage("Порядок страниц изменён", 3000)

    def _new_document(self) -> None:
        if not self._confirm_discard("Новый документ"):
            return

        dest = self._temp_path("new_document")
        try:
            create_empty_pdf(dest)
        except Exception as e:
            QMessageBox.critical(self, "Ошибка", f"Не удалось создать документ:\n{e}")
            return

        self._current_pdf = None
        self._working_pdf = dest
        self._dirty = False
        self._reload_pages()
        self.status_bar.showMessage("Создан новый пустой документ", 3000)

    def _open_pdf(self) -> None:
        path, _ = QFileDialog.getOpenFileName(
            self, "Открыть PDF", "", "PDF файлы (*.pdf);;Все файлы (*)"
        )
        if not path:
            return
        if not self._confirm_discard("Открыть PDF"):
            return

        src = Path(path)
        dest = self._temp_path(f"work_{src.stem}")
        try:
            validate_pdf(src)
            shutil.copy2(src, dest)
        except Exception as e:
            QMessageBox.critical(self, "Не удалось открыть PDF", str(e))
            return

        self._current_pdf = src
        self._working_pdf = dest
        self._dirty = False
        self._reload_pages()
        self.status_bar.showMessage(f"Открыт: {src.name}", 3000)

    def _save_pdf(self) -> None:
        if not self._working_pdf or not self._working_pdf.exists():
            QMessageBox.warning(self, "Нет документа", "Сначала откройте или создайте PDF.")
            return

        default_name = self._current_pdf.stem if self._current_pdf else "document"
        path, _ = QFileDialog.getSaveFileName(
            self, "Сохранить PDF", f"{default_name}.pdf", "PDF файлы (*.pdf)"
        )
        if not path:
            return

        try:
            shutil.copy2(self._working_pdf, path)
        except Exception as e:
            QMessageBox.critical(self, "Ошибка", f"Не удалось сохранить файл:\n{e}")
            return
        self._current_pdf = Path(path)
        self._dirty = False
        self._update_ui_state()
        self.status_bar.showMessage(f"Сохранено: {path}", 3000)

    def _create_pdf_from_images(self) -> None:
        paths, _ = QFileDialog.getOpenFileNames(
            self,
            "Выбрать изображения",
            "",
            "Изображения (*.png *.jpg *.jpeg *.bmp *.gif *.tiff *.tif *.webp);;Все файлы (*)",
        )
        if not paths:
            return
        if not self._confirm_discard("Создать PDF"):
            return

        image_paths = [Path(p) for p in paths]
        dest = self._temp_path("from_images")

        try:
            images_to_pdf(image_paths, dest)
        except Exception as e:
            QMessageBox.critical(self, "Ошибка", f"Не удалось создать PDF:\n{e}")
            return

        self._current_pdf = None
        self._working_pdf = dest
        self._dirty = True
        self._reload_pages()
        self.status_bar.showMessage(f"Создан PDF из {len(image_paths)} изображений", 3000)

    def _delete_selected_pages(self) -> None:
        if not self._working_pdf:
            QMessageBox.warning(self, "Нет документа", "Сначала откройте PDF.")
            return

        pages = self.page_list.get_selected_pages()
        if not pages:
            QMessageBox.information(self, "Не выбраны страницы", "Выберите страницы для удаления.")
            return

        total = get_page_count(self._working_pdf)
        if len(pages) >= total:
            QMessageBox.warning(self, "Ошибка", "Нельзя удалить все страницы.")
            return

        reply = QMessageBox.question(
            self,
            "Подтверждение",
            f"Удалить {len(pages)} страниц(у/ы): {pages}?",
            QMessageBox.StandardButton.Yes | QMessageBox.StandardButton.No,
        )
        if reply != QMessageBox.StandardButton.Yes:
            return

        dest = self._temp_path("deleted")
        try:
            delete_pages(self._working_pdf, set(pages), dest)
            shutil.move(dest, self._working_pdf)
        except Exception as e:
            QMessageBox.critical(self, "Ошибка", f"Не удалось удалить страницы:\n{e}")
            return

        self._dirty = True
        self._reload_pages()
        self.status_bar.showMessage(f"Удалено страниц: {len(pages)}", 3000)

    def _add_images(self) -> None:
        paths, _ = QFileDialog.getOpenFileNames(
            self,
            "Добавить изображения",
            "",
            "Изображения (*.png *.jpg *.jpeg *.bmp *.gif *.tiff *.tif *.webp);;Все файлы (*)",
        )
        if not paths:
            return

        max_page = get_page_count(self._working_pdf) if self._working_pdf else 0
        dlg = InsertPositionDialog(max_page, self)
        if dlg.exec() != QDialog.DialogCode.Accepted:
            return

        dest = self._temp_path("added_images")
        try:
            add_pages_from_images(
                self._working_pdf,
                [Path(p) for p in paths],
                dlg.position(),
                dest,
            )
            if self._working_pdf:
                shutil.move(dest, self._working_pdf)
            else:
                self._working_pdf = dest
        except Exception as e:
            QMessageBox.critical(self, "Ошибка", f"Не удалось добавить изображения:\n{e}")
            return

        self._dirty = True
        self._reload_pages()
        self.status_bar.showMessage(f"Добавлено {len(paths)} изображений", 3000)

    def _add_from_pdf(self) -> None:
        if not self._working_pdf:
            QMessageBox.warning(self, "Нет документа", "Сначала откройте PDF.")
            return

        source_path, _ = QFileDialog.getOpenFileName(
            self, "Выбрать PDF", "", "PDF файлы (*.pdf);;Все файлы (*)"
        )
        if not source_path:
            return

        source = Path(source_path)
        try:
            source_count = validate_pdf(source)
        except ValueError as e:
            QMessageBox.critical(self, "Не удалось открыть PDF", str(e))
            return

        max_page = get_page_count(self._working_pdf)
        dlg = InsertPositionDialog(max_page, self)
        if dlg.exec() != QDialog.DialogCode.Accepted:
            return

        dest = self._temp_path("added_pdf")
        try:
            add_pages_from_pdf(
                self._working_pdf,
                source,
                source_pages=None,
                insert_after=dlg.position(),
                output_path=dest,
            )
            shutil.move(dest, self._working_pdf)
        except Exception as e:
            QMessageBox.critical(self, "Ошибка", f"Не удалось добавить страницы:\n{e}")
            return

        self._dirty = True
        self._reload_pages()
        self.status_bar.showMessage(f"Добавлено {source_count} страниц из {source.name}", 3000)

    def _split_pages(self, selected_only: bool) -> None:
        if not self._working_pdf or not self._working_pdf.exists():
            QMessageBox.warning(self, "Нет документа", "Сначала откройте или создайте PDF.")
            return

        total = get_page_count(self._working_pdf)
        if total == 0:
            QMessageBox.warning(self, "Пустой документ", "В документе нет страниц для разделения.")
            return

        pages: list[int] | None
        if selected_only:
            pages = self.page_list.get_selected_pages()
            if not pages:
                QMessageBox.information(
                    self,
                    "Не выбраны страницы",
                    "Выберите страницы в списке слева или используйте «Разделить все страницы».",
                )
                return
        else:
            pages = None

        default_dir = str(self._current_pdf.parent) if self._current_pdf else str(Path.home())
        output_dir = QFileDialog.getExistingDirectory(
            self, "Папка для сохранения страниц", default_dir
        )
        if not output_dir:
            return

        prefix = self._current_pdf.stem if self._current_pdf else "document"
        try:
            created = split_pdf_to_pages(
                self._working_pdf,
                Path(output_dir),
                pages=pages,
                name_prefix=prefix,
            )
        except Exception as e:
            QMessageBox.critical(self, "Ошибка", f"Не удалось разделить документ:\n{e}")
            return

        self.status_bar.showMessage(
            f"Сохранено {len(created)} файл(ов) в {output_dir}",
            5000,
        )
        QMessageBox.information(
            self,
            "Готово",
            f"Создано файлов: {len(created)}\nПапка: {output_dir}",
        )

    def _compress_current_pdf(self) -> None:
        if not self._working_pdf:
            QMessageBox.warning(self, "Нет документа", "Сначала откройте PDF.")
            return

        quality = self.quality_slider.value()
        dest = self._temp_path("compressed")

        old_size = self._working_pdf.stat().st_size
        try:
            engine = compress_pdf(self._working_pdf, dest, quality)
            shutil.move(dest, self._working_pdf)
        except Exception as e:
            QMessageBox.critical(self, "Ошибка", f"Не удалось сжать PDF:\n{e}")
            return

        self._dirty = True
        new_size = self._working_pdf.stat().st_size
        reduction = (1 - new_size / old_size) * 100 if old_size > 0 else 0
        self._reload_pages()
        engine_name = "Ghostscript" if engine == "ghostscript" else "PyMuPDF"
        self.status_bar.showMessage(
            f"Сжатие ({engine_name}): {old_size / 1024:.0f} КБ → {new_size / 1024:.0f} КБ "
            f"({'−' if reduction > 0 else '+'}{abs(reduction):.0f}%)",
            5000,
        )

    def closeEvent(self, event) -> None:
        if not self._confirm_discard("Выход"):
            event.ignore()
            return
        if Path(self._temp_dir).exists():
            shutil.rmtree(self._temp_dir, ignore_errors=True)
        super().closeEvent(event)


def _excepthook(exc_type, exc, tb) -> None:
    """Показывает непредвиденную ошибку вместо аварийного завершения PyQt6."""
    traceback.print_exception(exc_type, exc, tb)
    if QApplication.instance() is not None:
        QMessageBox.critical(None, "Непредвиденная ошибка", f"{exc_type.__name__}: {exc}")


def run() -> None:
    sys.excepthook = _excepthook
    app = QApplication(sys.argv)
    app.setApplicationName("PDF Tool")
    app.setStyle("Fusion")

    window = MainWindow()
    window.show()
    sys.exit(app.exec())
