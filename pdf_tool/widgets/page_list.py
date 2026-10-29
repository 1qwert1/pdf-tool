"""Виджет списка страниц PDF с миниатюрами."""

from __future__ import annotations

from pathlib import Path

from PyQt6.QtCore import Qt, QSize, pyqtSignal
from PyQt6.QtGui import QIcon, QPixmap
from PyQt6.QtWidgets import (
    QAbstractItemView,
    QListWidget,
    QListWidgetItem,
    QVBoxLayout,
    QWidget,
)

from pdf_tool.pdf_operations import get_page_count, render_page_thumbnail


class _DraggableList(QListWidget):
    """QListWidget, сигнализирующий о завершении перетаскивания."""

    dropped = pyqtSignal()

    def dropEvent(self, event) -> None:
        super().dropEvent(event)
        self.dropped.emit()


class PageListWidget(QWidget):
    selection_changed = pyqtSignal(list)
    order_changed = pyqtSignal(list)

    def __init__(self, parent=None):
        super().__init__(parent)
        self._pdf_path: Path | None = None

        layout = QVBoxLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)

        self.list_widget = _DraggableList()
        self.list_widget.setSelectionMode(QAbstractItemView.SelectionMode.ExtendedSelection)
        self.list_widget.setViewMode(QListWidget.ViewMode.IconMode)
        self.list_widget.setResizeMode(QListWidget.ResizeMode.Adjust)
        self.list_widget.setMovement(QListWidget.Movement.Snap)
        self.list_widget.setDragDropMode(QAbstractItemView.DragDropMode.InternalMove)
        self.list_widget.setDefaultDropAction(Qt.DropAction.MoveAction)
        self.list_widget.setSpacing(8)
        thumb_size = QSize(120, 160)
        self.list_widget.setIconSize(thumb_size)
        self.list_widget.setGridSize(QSize(140, 190))
        self.list_widget.itemSelectionChanged.connect(self._on_selection_changed)
        self.list_widget.dropped.connect(self._emit_order_changed)

        layout.addWidget(self.list_widget)

    def load_pdf(self, path: Path) -> None:
        self._pdf_path = path
        self.list_widget.clear()

        if not path or not path.exists():
            return

        count = get_page_count(path)
        icon_size = self.list_widget.iconSize()

        for i in range(count):
            try:
                thumb_data = render_page_thumbnail(path, i, max_size=icon_size.width())
                pixmap = QPixmap()
                pixmap.loadFromData(thumb_data)
                icon = QIcon(pixmap)
            except Exception:
                icon = QIcon()

            item = QListWidgetItem(icon, f"Стр. {i + 1}")
            item.setData(Qt.ItemDataRole.UserRole, i + 1)
            self.list_widget.addItem(item)

    def get_selected_pages(self) -> list[int]:
        return sorted(
            item.data(Qt.ItemDataRole.UserRole)
            for item in self.list_widget.selectedItems()
        )

    def get_current_order(self) -> list[int]:
        """Текущий визуальный порядок страниц (номера с 1)."""
        return [
            self.list_widget.item(i).data(Qt.ItemDataRole.UserRole)
            for i in range(self.list_widget.count())
        ]

    def move_selected(self, direction: int) -> None:
        """Сдвигает выбранные страницы. direction: -1 вверх, +1 вниз."""
        rows = sorted(self.list_widget.row(item) for item in self.list_widget.selectedItems())
        if not rows:
            return

        count = self.list_widget.count()
        if direction < 0:
            if rows[0] == 0:
                return
            for row in rows:
                item = self.list_widget.takeItem(row)
                self.list_widget.insertItem(row - 1, item)
            new_rows = [r - 1 for r in rows]
        else:
            if rows[-1] >= count - 1:
                return
            for row in reversed(rows):
                item = self.list_widget.takeItem(row)
                self.list_widget.insertItem(row + 1, item)
            new_rows = [r + 1 for r in rows]

        self.list_widget.clearSelection()
        for row in new_rows:
            self.list_widget.item(row).setSelected(True)

        self._emit_order_changed()

    def move_selected_to_edge(self, to_start: bool) -> None:
        """Перемещает выбранные страницы в начало или в конец."""
        rows = sorted(self.list_widget.row(item) for item in self.list_widget.selectedItems())
        if not rows:
            return

        items = []
        for row in reversed(rows):
            items.insert(0, self.list_widget.takeItem(row))

        if to_start:
            for i, item in enumerate(items):
                self.list_widget.insertItem(i, item)
            new_rows = list(range(len(items)))
        else:
            for item in items:
                self.list_widget.addItem(item)
            start = self.list_widget.count() - len(items)
            new_rows = list(range(start, self.list_widget.count()))

        self.list_widget.clearSelection()
        for row in new_rows:
            self.list_widget.item(row).setSelected(True)

        self._emit_order_changed()

    def clear(self) -> None:
        self._pdf_path = None
        self.list_widget.clear()

    @property
    def pdf_path(self) -> Path | None:
        return self._pdf_path

    def _on_selection_changed(self) -> None:
        self.selection_changed.emit(self.get_selected_pages())

    def _emit_order_changed(self) -> None:
        order = self.get_current_order()
        if order:
            self.order_changed.emit(order)
