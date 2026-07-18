#!/usr/bin/env bash
# Install the Cauldron icon + desktop entry for the current user.
# Safe to re-run; only touches ~/.local/share.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ICONS="$HOME/.local/share/icons/hicolor"
APPS="$HOME/.local/share/applications"

# hicolor PNGs
for d in "$HERE"/hicolor/*x*/apps; do
    size_dir="$(basename "$(dirname "$d")")"
    mkdir -p "$ICONS/$size_dir/apps"
    install -m644 "$d/com.coffee.cauldron.png" "$ICONS/$size_dir/apps/com.coffee.cauldron.png"
done

# scalable SVG
mkdir -p "$ICONS/scalable/apps"
install -m644 "$HERE/cauldron.svg" "$ICONS/scalable/apps/com.coffee.cauldron.svg"

# desktop entry (app_id com.coffee.cauldron -> Rune dock matches via StartupWMClass/Icon)
mkdir -p "$APPS"
cat > "$APPS/com.coffee.cauldron.desktop" <<'EOF'
[Desktop Entry]
Type=Application
Name=Cauldron
GenericName=IDE
Comment=Rune-native IDE
Exec=cauldron %F
Icon=com.coffee.cauldron
Terminal=false
Categories=Development;IDE;
StartupWMClass=com.coffee.cauldron
EOF

if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -f -t "$ICONS" 2>/dev/null || true
fi
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$APPS" 2>/dev/null || true
fi

echo "Cauldron icon + desktop entry installed:"
echo "  $ICONS/<size>/apps/com.coffee.cauldron.png (+ scalable SVG)"
echo "  $APPS/com.coffee.cauldron.desktop"
