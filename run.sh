#!/usr/bin/env bash
# Запуск PDF Tool
cd "$(dirname "$0")" || exit 1

# .venv используем, только если в нём установлены зависимости
# (venv без ensurepip создаётся пустым)
CHECK='import fitz, pikepdf, PIL, PyQt6'
if [ -x .venv/bin/python ] && .venv/bin/python -c "$CHECK" 2>/dev/null; then
  exec .venv/bin/python main.py "$@"
else
  exec python3 main.py "$@"
fi
