#!/usr/bin/env bash
# Устанавливает PDF Tool для текущего пользователя (без sudo):
# программа — в ~/.local/share/pdf-tool, ярлык — в меню приложений.
set -euo pipefail
cd "$(dirname "$0")"

dest="$HOME/.local/share/pdf-tool"
apps="$HOME/.local/share/applications"
mkdir -p "$dest" "$apps"

cp pdf-tool libpdfium.so "$dest/"
chmod +x "$dest/pdf-tool"
sed "s|^Exec=pdf-tool|Exec=$dest/pdf-tool|" pdf-tool.desktop > "$apps/pdf-tool.desktop"
update-desktop-database "$apps" 2>/dev/null || true

echo "Установлено в $dest. PDF Tool появится в меню приложений."
