# Cauldron app icon

`cauldron.svg` is the single source of truth. Every raster in this directory is
derived from it — edit the SVG, then regenerate.

Design: flat occult/autumn/space tile — black cauldron silhouette, molten
burnt-orange melt (`#E96E2C` family, one radial glow), star-sparks rising,
Futhark-like strokes on the belly, near-black tile (`#141318`) with a faint
deep-violet vignette.

## Regenerate everything

```sh
cd assets/icon

# hicolor PNGs (rsvg-convert; inkscape or magick also work)
for s in 16 32 48 64 128 256 512; do
  mkdir -p "hicolor/${s}x${s}/apps"
  rsvg-convert -w $s -h $s -o "hicolor/${s}x${s}/apps/com.coffee.cauldron.png" cauldron.svg
done

# raw RGBA8 for eframe's ViewportBuilder::with_icon (256*256*4 bytes, no header)
python3 -c "
from PIL import Image
im = Image.open('hicolor/256x256/apps/com.coffee.cauldron.png').convert('RGBA')
open('icon-256.rgba', 'wb').write(im.tobytes())
"
```

## Install for the desktop (Rune dock, app_id `com.coffee.cauldron`)

```sh
./install.sh
```

Copies the hicolor tree into `~/.local/share/icons/hicolor/`, installs the SVG
as the scalable icon, writes `~/.local/share/applications/com.coffee.cauldron.desktop`,
and refreshes the icon cache if `gtk-update-icon-cache` is present.

## Wiring the window icon in eframe (0.29)

```rust
let icon = {
    let rgba = include_bytes!("../assets/icon/icon-256.rgba").to_vec(); // adjust path
    egui::IconData { rgba, width: 256, height: 256 }
};
let native_options = eframe::NativeOptions {
    viewport: egui::ViewportBuilder::default()
        .with_app_id("com.coffee.cauldron")
        .with_icon(icon),
    ..Default::default()
};
```
