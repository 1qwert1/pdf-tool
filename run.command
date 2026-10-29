#!/usr/bin/env bash
# Двойной клик в Finder на macOS открывает Terminal и запускает PDF Tool.
cd "$(dirname "$0")" || exit 1

CHECK='import fitz, pikepdf, PIL, PyQt6'
if [ -x .venv/bin/python ] && .venv/bin/python -c "$CHECK" 2>/dev/null; then
  exec .venv/bin/python main.py "$@"
elif command -v python3 >/dev/null 2>&1; then
  exec python3 main.py "$@"
else
  echo "Python 3 не найден. Установите: brew install python"
  read -r -p "Нажмите Enter…"
  exit 1
fi
