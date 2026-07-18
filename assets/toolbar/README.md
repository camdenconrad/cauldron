# Toolbar icons

SVG masters for the main-toolbar icon buttons (`crates/cauldron/src/icons.rs`,
`ToolIcon`). 24×24 viewBox, 2px optical stroke, rounded caps/joins, one color
per icon on transparent:

| icon | file | color |
|---|---|---|
| Run | `run.svg` | `#78C86E` |
| Build | `build.svg` | `#D9A441` |
| Debug | `debug.svg` | `#E0695C` |
| Settings | `settings.svg` | `#C8C4C0` |
| Search | `search.svg` | `#61AFEF` |

The app embeds the PNGs in `png/` (via `include_bytes!`) at 2x (48px) and
3x (72px); the SVGs are the source of truth — edit them, then regenerate:

```sh
cd assets/toolbar
for n in run build debug settings search; do
  rsvg-convert -w 48 -h 48 "$n.svg" -o "png/${n}@2x.png"
  rsvg-convert -w 72 -h 72 "$n.svg" -o "png/${n}@3x.png"
done
```
