//! Vector-painted file-type + folder icons for the file tree, tabs and quick-open.
//!
//! Everything here is drawn live with egui painter primitives (rounded-rect chip
//! + shapes + a monospace glyph), so the icons are crisp at any size and any DPI —
//! there are no textures to blur. Autumn-tuned "JetBrains-colorful" palette on the Cauldron
//! near-black (`#141318`) chrome.
//!
//! Integration (later, by the app): add `mod icons;` in `main.rs`, then paint with
//! `icons::file_icon(ui, rect, path)` / `icons::folder_icon(ui, rect, open)`.
#![allow(dead_code)] // wired in by the integrator; nothing here is called yet

use egui::{pos2, vec2, Align2, Color32, FontId, Painter, Pos2, Rect, Rounding, Shape, Stroke};
use std::path::Path;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Paint a crisp vector icon for `path`'s file type into `rect`.
pub fn file_icon(ui: &mut egui::Ui, rect: egui::Rect, path: &std::path::Path) {
    paint_kind(ui.painter(), rect, kind_for_path(path));
}

/// Paint a flat folder in dim amber; slightly brighter (and lipped open) when `open`.
pub fn folder_icon(ui: &mut egui::Ui, rect: egui::Rect, open: bool) {
    let p = ui.painter();
    let s = rect.width().min(rect.height());
    let base = folder_color(open);

    // back tab
    let tab = Rect::from_min_size(
        rect.left_top() + vec2(s * 0.10, s * 0.16),
        vec2(s * 0.34, s * 0.20),
    );
    p.rect_filled(tab, Rounding::same(s * 0.06), base);

    // body
    let body = Rect::from_min_max(
        rect.left_top() + vec2(s * 0.08, s * 0.28),
        rect.right_bottom() - vec2(s * 0.08, s * 0.14),
    );
    p.rect_filled(body, Rounding::same(s * 0.08), base);

    if open {
        // slanted, brighter front flap
        let flap = vec![
            pos2(body.left() + s * 0.14, body.top() + s * 0.12),
            pos2(body.right() + s * 0.02, body.top() + s * 0.12),
            pos2(body.right() - s * 0.08, body.bottom()),
            pos2(body.left() + s * 0.02, body.bottom()),
        ];
        p.add(Shape::convex_polygon(flap, FOLDER_FLAP, Stroke::NONE));
    }
}

// ---------------------------------------------------------------------------
// File kinds (pure, unit-tested)
// ---------------------------------------------------------------------------

/// Everything the tree knows how to color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileKind {
    Rust,
    C,
    Cpp,
    CSharp,
    Python,
    Config,   // .toml / .cfg / .ini / .conf — amber gear
    Markdown, // bone "M↓"
    Json,     // golden braces
    Shell,    // moss ">_"
    Text,     // dim page
    Image,    // plum landscape chip
    Lock,     // ash padlock
    Build,    // makefiles / CMake — amber wrench
    Other,    // dim page with folded corner
}

/// Lower-cased extension of `path`, if any.
pub(crate) fn ext_of(path: &Path) -> Option<String> {
    path.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
}

/// Lower-cased final file name of `path` (empty for e.g. `/`).
pub(crate) fn file_name_lower(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

/// Build-system files recognised by *name* rather than extension.
pub(crate) fn is_build_file(name_lower: &str) -> bool {
    matches!(name_lower, "makefile" | "gnumakefile" | "justfile" | "cmakelists.txt")
        || name_lower.ends_with(".mk")
        || name_lower.ends_with(".cmake")
}

/// Map a lower-cased extension to a [`FileKind`].
pub(crate) fn kind_for_ext(ext: &str) -> FileKind {
    match ext {
        "rs" => FileKind::Rust,
        "c" | "h" => FileKind::C,
        "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => FileKind::Cpp,
        "cs" => FileKind::CSharp,
        "py" => FileKind::Python,
        "toml" | "cfg" | "ini" | "conf" => FileKind::Config,
        "md" | "markdown" => FileKind::Markdown,
        "json" => FileKind::Json,
        "sh" | "bash" | "zsh" => FileKind::Shell,
        "txt" => FileKind::Text,
        "png" | "svg" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" => FileKind::Image,
        "lock" => FileKind::Lock,
        "mk" | "cmake" => FileKind::Build,
        _ => FileKind::Other,
    }
}

/// Full path → kind: special file names first, then extension.
pub(crate) fn kind_for_path(path: &Path) -> FileKind {
    if is_build_file(&file_name_lower(path)) {
        return FileKind::Build;
    }
    match ext_of(path) {
        Some(ext) => kind_for_ext(&ext),
        None => FileKind::Other,
    }
}

/// Accent color per kind — JetBrains-colorful, autumn-tuned.
pub(crate) fn kind_color(kind: FileKind) -> Color32 {
    match kind {
        FileKind::Rust => Color32::from_rgb(0xE9, 0x6E, 0x2C),     // burnt orange
        FileKind::C => Color32::from_rgb(0x46, 0x6B, 0x8F),        // deep blue-slate
        FileKind::Cpp => Color32::from_rgb(0x6C, 0x7E, 0x95),      // slate
        FileKind::CSharp => Color32::from_rgb(0x8A, 0x4E, 0x9E),   // .NET purple
        FileKind::Python => Color32::from_rgb(0xB5, 0xA6, 0x3C),   // moss-yellow
        FileKind::Config => Color32::from_rgb(0xD7, 0x99, 0x21),   // amber
        FileKind::Markdown => Color32::from_rgb(0xD8, 0xCF, 0xC0), // bone
        FileKind::Json => Color32::from_rgb(0xE0, 0xA7, 0x3A),     // golden
        FileKind::Shell => Color32::from_rgb(0x87, 0x9A, 0x48),    // moss
        FileKind::Text => Color32::from_rgb(0x8F, 0x8A, 0x7E),     // dim bone
        FileKind::Image => Color32::from_rgb(0xA8, 0x62, 0x9A),    // plum
        FileKind::Lock => Color32::from_rgb(0x84, 0x81, 0x7B),     // ash
        FileKind::Build => Color32::from_rgb(0xC9, 0x8A, 0x2E),    // tool amber
        FileKind::Other => Color32::from_rgb(0x74, 0x70, 0x6B),    // dim
    }
}

/// The 1–2 character monospace glyph painted on the chip (None = pure-shape icon).
pub(crate) fn kind_glyph(kind: FileKind) -> Option<&'static str> {
    match kind {
        FileKind::Rust => Some("R"),
        FileKind::C => Some("C"),
        FileKind::Cpp => Some("C+"),
        FileKind::CSharp => Some("C#"),
        FileKind::Python => Some("Py"),
        FileKind::Json => Some("{}"),
        FileKind::Shell => Some(">_"),
        FileKind::Markdown => Some("M"), // the ↓ arrow is painted, not a glyph
        _ => None,
    }
}

/// Linear per-channel mix of two colors, `t` clamped to `0..=1`.
pub(crate) fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let lerp = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(lerp(a.r(), b.r()), lerp(a.g(), b.g()), lerp(a.b(), b.b()))
}

/// Dark chip background tinted faintly toward the accent.
pub(crate) fn chip_fill(accent: Color32) -> Color32 {
    mix(Color32::from_rgb(0x17, 0x15, 0x1C), accent, 0.16)
}

/// Folder fill: dim amber, brighter when open.
pub(crate) fn folder_color(open: bool) -> Color32 {
    if open {
        Color32::from_rgb(0xB3, 0x8B, 0x3E)
    } else {
        Color32::from_rgb(0x9A, 0x76, 0x33)
    }
}

const FOLDER_FLAP: Color32 = Color32::from_rgb(0xC8, 0x9F, 0x4C);

fn with_alpha(c: Color32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
}

// ---------------------------------------------------------------------------
// Painting (vector primitives only — crisp at any DPI)
// ---------------------------------------------------------------------------

fn paint_kind(p: &Painter, rect: Rect, kind: FileKind) {
    let s = rect.width().min(rect.height());
    let rect = rect.shrink((s * 0.03).max(0.5));
    let accent = kind_color(kind);
    let rounding = Rounding::same(s * 0.22);

    // chip: dark tinted fill + thin accent stroke
    p.rect_filled(rect, rounding, chip_fill(accent));
    p.rect_stroke(rect, rounding, Stroke::new((s * 0.055).max(1.0), with_alpha(accent, 130)));

    let c = rect.center();
    match kind {
        FileKind::Rust => {
            // faint gear halo behind the R (gear-ish nod to the crate icon)
            paint_gear(p, c, s * 0.36, with_alpha(accent, 64), (s * 0.05).max(1.0));
            p.text(c, Align2::CENTER_CENTER, "R", FontId::monospace(s * 0.58), accent);
        }
        FileKind::Config => {
            paint_gear(p, c, s * 0.30, accent, (s * 0.07).max(1.0));
            p.circle_filled(c, s * 0.075, accent);
        }
        FileKind::Markdown => {
            p.text(
                c + vec2(-s * 0.13, 0.0),
                Align2::CENTER_CENTER,
                "M",
                FontId::monospace(s * 0.52),
                accent,
            );
            paint_down_arrow(p, c + vec2(s * 0.21, 0.0), s, accent);
        }
        FileKind::Image => paint_landscape(p, c, s, accent),
        FileKind::Lock => paint_padlock(p, c, s, accent),
        FileKind::Build => paint_wrench(p, c, s, accent),
        FileKind::Text | FileKind::Other => paint_page(p, c, s, accent),
        _ => {
            if let Some(glyph) = kind_glyph(kind) {
                let size = if glyph.chars().count() >= 2 { s * 0.44 } else { s * 0.60 };
                p.text(c, Align2::CENTER_CENTER, glyph, FontId::monospace(size), accent);
            }
        }
    }
}

/// Gear: ring + radial teeth.
fn paint_gear(p: &Painter, c: Pos2, r: f32, color: Color32, w: f32) {
    let ring = r * 0.72;
    p.circle_stroke(c, ring, Stroke::new(w, color));
    for i in 0..8 {
        let a = i as f32 * std::f32::consts::TAU / 8.0;
        let dir = vec2(a.cos(), a.sin());
        p.line_segment([c + dir * ring, c + dir * r], Stroke::new(w, color));
    }
}

/// Small "↓" drawn with strokes (font-independent).
fn paint_down_arrow(p: &Painter, c: Pos2, s: f32, color: Color32) {
    let w = Stroke::new((s * 0.07).max(1.0), color);
    let top = c + vec2(0.0, -s * 0.15);
    let tip = c + vec2(0.0, s * 0.15);
    p.line_segment([top, tip], w);
    p.line_segment([tip, tip + vec2(-s * 0.10, -s * 0.10)], w);
    p.line_segment([tip, tip + vec2(s * 0.10, -s * 0.10)], w);
}

/// Plum landscape: two mountains + a pale sun.
fn paint_landscape(p: &Painter, c: Pos2, s: f32, accent: Color32) {
    let base_y = c.y + s * 0.22;
    p.add(Shape::convex_polygon(
        vec![
            pos2(c.x - s * 0.30, base_y),
            pos2(c.x - s * 0.06, c.y - s * 0.12),
            pos2(c.x + s * 0.18, base_y),
        ],
        accent,
        Stroke::NONE,
    ));
    p.add(Shape::convex_polygon(
        vec![
            pos2(c.x + s * 0.02, base_y),
            pos2(c.x + s * 0.18, c.y - s * 0.01),
            pos2(c.x + s * 0.32, base_y),
        ],
        mix(accent, Color32::WHITE, 0.28),
        Stroke::NONE,
    ));
    p.circle_filled(c + vec2(s * 0.14, -s * 0.17), s * 0.075, mix(accent, Color32::WHITE, 0.55));
}

/// Ash padlock: shackle arc (upper half of a circle) + body over its lower half.
fn paint_padlock(p: &Painter, c: Pos2, s: f32, accent: Color32) {
    p.circle_stroke(c + vec2(0.0, -s * 0.10), s * 0.14, Stroke::new((s * 0.07).max(1.0), accent));
    let body = Rect::from_min_max(
        c + vec2(-s * 0.19, -s * 0.05),
        c + vec2(s * 0.19, s * 0.24),
    );
    p.rect_filled(body, Rounding::same(s * 0.05), accent);
    p.circle_filled(c + vec2(0.0, s * 0.08), s * 0.05, chip_fill(accent)); // keyhole
}

/// Amber wrench: open-jaw head + rounded handle.
fn paint_wrench(p: &Painter, c: Pos2, s: f32, accent: Color32) {
    let hc = c + vec2(-s * 0.13, -s * 0.13); // head center
    let hr = s * 0.14;
    let w = (s * 0.10).max(1.0);
    p.circle_stroke(hc, hr, Stroke::new(w * 0.9, accent));
    // cut the jaw open toward the upper-left with a chip-colored wedge
    let out = vec2(-1.0, -1.0).normalized();
    let perp = vec2(-out.y, out.x);
    p.add(Shape::convex_polygon(
        vec![
            hc + out * hr * 0.3,
            hc + out * hr * 1.8 + perp * hr * 0.55,
            hc + out * hr * 1.8 - perp * hr * 0.55,
        ],
        chip_fill(accent),
        Stroke::NONE,
    ));
    // handle
    let start = hc + vec2(hr * 0.55, hr * 0.55);
    let end = c + vec2(s * 0.20, s * 0.20);
    p.line_segment([start, end], Stroke::new(w, accent));
    p.circle_filled(end, w * 0.5, accent); // round the handle tip
}

/// Dim page with a folded corner and two text lines.
fn paint_page(p: &Painter, c: Pos2, s: f32, accent: Color32) {
    let hw = s * 0.19; // half width
    let hh = s * 0.26; // half height
    let fold = s * 0.13;
    let (l, r, t, b) = (c.x - hw, c.x + hw, c.y - hh, c.y + hh);
    p.add(Shape::convex_polygon(
        vec![
            pos2(l, t),
            pos2(r - fold, t),
            pos2(r, t + fold),
            pos2(r, b),
            pos2(l, b),
        ],
        with_alpha(accent, 60),
        Stroke::new((s * 0.045).max(1.0), accent),
    ));
    // folded corner
    p.add(Shape::convex_polygon(
        vec![pos2(r - fold, t), pos2(r, t + fold), pos2(r - fold, t + fold)],
        with_alpha(accent, 170),
        Stroke::NONE,
    ));
    // text lines
    let lw = Stroke::new((s * 0.04).max(1.0), with_alpha(accent, 200));
    p.line_segment([pos2(l + s * 0.07, c.y + s * 0.02), pos2(r - s * 0.07, c.y + s * 0.02)], lw);
    p.line_segment([pos2(l + s * 0.07, c.y + s * 0.12), pos2(r - s * 0.07, c.y + s * 0.12)], lw);
}

// ---------------------------------------------------------------------------
// Tests (pure helpers only — painting is exercised live)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::path::Path;

    #[test]
    fn core_language_extensions_map() {
        assert_eq!(kind_for_path(Path::new("src/main.rs")), FileKind::Rust);
        assert_eq!(kind_for_path(Path::new("lib/ffi.c")), FileKind::C);
        assert_eq!(kind_for_path(Path::new("lib/ffi.h")), FileKind::C);
        assert_eq!(kind_for_path(Path::new("core.cpp")), FileKind::Cpp);
        assert_eq!(kind_for_path(Path::new("core.hpp")), FileKind::Cpp);
        assert_eq!(kind_for_path(Path::new("tool.py")), FileKind::Python);
        assert_eq!(kind_for_path(Path::new("Cargo.toml")), FileKind::Config);
        assert_eq!(kind_for_path(Path::new("README.md")), FileKind::Markdown);
        assert_eq!(kind_for_path(Path::new("data.json")), FileKind::Json);
        assert_eq!(kind_for_path(Path::new("run.sh")), FileKind::Shell);
        assert_eq!(kind_for_path(Path::new("notes.txt")), FileKind::Text);
    }

    #[test]
    fn extension_matching_is_case_insensitive() {
        assert_eq!(kind_for_path(Path::new("SRC/MAIN.RS")), FileKind::Rust);
        assert_eq!(kind_for_path(Path::new("Readme.MD")), FileKind::Markdown);
        assert_eq!(ext_of(Path::new("A.TOML")).as_deref(), Some("toml"));
    }

    #[test]
    fn build_files_are_recognised_by_name() {
        for name in ["Makefile", "makefile", "GNUmakefile", "CMakeLists.txt", "rules.mk", "deps.cmake"] {
            assert_eq!(kind_for_path(Path::new(name)), FileKind::Build, "{name}");
        }
        assert!(is_build_file("justfile"));
        assert!(!is_build_file("makefile.bak"));
    }

    #[test]
    fn locks_and_images_map() {
        assert_eq!(kind_for_path(Path::new("Cargo.lock")), FileKind::Lock);
        for img in ["a.png", "b.svg", "c.jpg", "d.jpeg", "e.webp"] {
            assert_eq!(kind_for_path(Path::new(img)), FileKind::Image, "{img}");
        }
    }

    #[test]
    fn unknown_and_extensionless_fall_back_to_other() {
        assert_eq!(kind_for_path(Path::new("LICENSE")), FileKind::Other);
        assert_eq!(kind_for_path(Path::new("weird.xyz")), FileKind::Other);
        assert_eq!(ext_of(Path::new("LICENSE")), None);
    }

    #[test]
    fn glyphs_match_spec() {
        assert_eq!(kind_glyph(FileKind::Rust), Some("R"));
        assert_eq!(kind_glyph(FileKind::Cpp), Some("C+"));
        assert_eq!(kind_glyph(FileKind::Python), Some("Py"));
        assert_eq!(kind_glyph(FileKind::Json), Some("{}"));
        assert_eq!(kind_glyph(FileKind::Shell), Some(">_"));
        assert_eq!(kind_glyph(FileKind::Config), None); // pure gear
        assert_eq!(kind_glyph(FileKind::Lock), None); // pure padlock
    }

    #[test]
    fn accent_colors_are_distinct_across_kinds() {
        let kinds = [
            FileKind::Rust,
            FileKind::C,
            FileKind::Cpp,
            FileKind::Python,
            FileKind::Config,
            FileKind::Markdown,
            FileKind::Json,
            FileKind::Shell,
            FileKind::Text,
            FileKind::Image,
            FileKind::Lock,
            FileKind::Build,
            FileKind::Other,
        ];
        let unique: HashSet<_> = kinds.iter().map(|k| kind_color(*k)).collect();
        assert_eq!(unique.len(), kinds.len());
    }

    #[test]
    fn mix_hits_both_endpoints() {
        let a = Color32::from_rgb(10, 20, 30);
        let b = Color32::from_rgb(200, 150, 100);
        assert_eq!(mix(a, b, 0.0), a);
        assert_eq!(mix(a, b, 1.0), b);
    }

    #[test]
    fn chip_fill_is_darker_than_accent() {
        for kind in [FileKind::Rust, FileKind::Markdown, FileKind::Json] {
            let accent = kind_color(kind);
            let fill = chip_fill(accent);
            let sum = |c: Color32| c.r() as u32 + c.g() as u32 + c.b() as u32;
            assert!(sum(fill) < sum(accent), "{kind:?}");
        }
    }

    #[test]
    fn open_folder_is_brighter_than_closed() {
        let sum = |c: Color32| c.r() as u32 + c.g() as u32 + c.b() as u32;
        assert!(sum(folder_color(true)) > sum(folder_color(false)));
    }
}


// =================================================================================================
// toolbar icons — SVG masters (assets/toolbar/*.svg) pre-rendered to PNG at 2x/3x and embedded.
// Decoded once per (icon, dpi tier) and cached as an egui texture; drawn at 22px logical.
// =================================================================================================

/// Which toolbar icon to draw.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolIcon {
    Run,
    Build,
    Debug,
    Settings,
    Search,
    Pin,
    Structure,
    History,
    Sparkle,
}

/// Which pre-rendered raster to sample: 2x (48px) or 3x (72px) of the 24px master.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum DpiTier {
    X2,
    X3,
}

/// Pick the raster tier from the effective scale (`ctx.pixels_per_point()`, which
/// already folds in the zoom factor): anything past 1.5 gets the 3x sheet.
pub(crate) fn dpi_tier(pixels_per_point: f32) -> DpiTier {
    if pixels_per_point > 1.5 { DpiTier::X3 } else { DpiTier::X2 }
}

/// The embedded PNG for an icon at a tier. Regenerate via assets/toolbar/README.md.
pub(crate) fn icon_png_bytes(icon: ToolIcon, tier: DpiTier) -> &'static [u8] {
    match (icon, tier) {
        (ToolIcon::Run, DpiTier::X2) => include_bytes!("../../../assets/toolbar/png/run@2x.png"),
        (ToolIcon::Run, DpiTier::X3) => include_bytes!("../../../assets/toolbar/png/run@3x.png"),
        (ToolIcon::Build, DpiTier::X2) => include_bytes!("../../../assets/toolbar/png/build@2x.png"),
        (ToolIcon::Build, DpiTier::X3) => include_bytes!("../../../assets/toolbar/png/build@3x.png"),
        (ToolIcon::Debug, DpiTier::X2) => include_bytes!("../../../assets/toolbar/png/debug@2x.png"),
        (ToolIcon::Debug, DpiTier::X3) => include_bytes!("../../../assets/toolbar/png/debug@3x.png"),
        (ToolIcon::Settings, DpiTier::X2) => {
            include_bytes!("../../../assets/toolbar/png/settings@2x.png")
        }
        (ToolIcon::Settings, DpiTier::X3) => {
            include_bytes!("../../../assets/toolbar/png/settings@3x.png")
        }
        (ToolIcon::Search, DpiTier::X2) => include_bytes!("../../../assets/toolbar/png/search@2x.png"),
        (ToolIcon::Search, DpiTier::X3) => include_bytes!("../../../assets/toolbar/png/search@3x.png"),
        (ToolIcon::Pin, DpiTier::X2) => include_bytes!("../../../assets/toolbar/png/pin@2x.png"),
        (ToolIcon::Pin, DpiTier::X3) => include_bytes!("../../../assets/toolbar/png/pin@3x.png"),
        (ToolIcon::Structure, DpiTier::X2) => include_bytes!("../../../assets/toolbar/png/structure@2x.png"),
        (ToolIcon::Structure, DpiTier::X3) => include_bytes!("../../../assets/toolbar/png/structure@3x.png"),
        (ToolIcon::History, DpiTier::X2) => include_bytes!("../../../assets/toolbar/png/history@2x.png"),
        (ToolIcon::History, DpiTier::X3) => include_bytes!("../../../assets/toolbar/png/history@3x.png"),
        (ToolIcon::Sparkle, DpiTier::X2) => include_bytes!("../../../assets/toolbar/png/sparkle@2x.png"),
        (ToolIcon::Sparkle, DpiTier::X3) => include_bytes!("../../../assets/toolbar/png/sparkle@3x.png"),
    }
}

/// Debug name for the texture allocator.
fn icon_texture_name(icon: ToolIcon, tier: DpiTier) -> &'static str {
    match (icon, tier) {
        (ToolIcon::Run, DpiTier::X2) => "toolbar/run@2x",
        (ToolIcon::Run, DpiTier::X3) => "toolbar/run@3x",
        (ToolIcon::Build, DpiTier::X2) => "toolbar/build@2x",
        (ToolIcon::Build, DpiTier::X3) => "toolbar/build@3x",
        (ToolIcon::Debug, DpiTier::X2) => "toolbar/debug@2x",
        (ToolIcon::Debug, DpiTier::X3) => "toolbar/debug@3x",
        (ToolIcon::Settings, DpiTier::X2) => "toolbar/settings@2x",
        (ToolIcon::Settings, DpiTier::X3) => "toolbar/settings@3x",
        (ToolIcon::Search, DpiTier::X2) => "toolbar/search@2x",
        (ToolIcon::Search, DpiTier::X3) => "toolbar/search@3x",
        (ToolIcon::Pin, DpiTier::X2) => "toolbar/pin@2x",
        (ToolIcon::Pin, DpiTier::X3) => "toolbar/pin@3x",
        (ToolIcon::Structure, DpiTier::X2) => "toolbar/structure@2x",
        (ToolIcon::Structure, DpiTier::X3) => "toolbar/structure@3x",
        (ToolIcon::History, DpiTier::X2) => "toolbar/history@2x",
        (ToolIcon::History, DpiTier::X3) => "toolbar/history@3x",
        (ToolIcon::Sparkle, DpiTier::X2) => "toolbar/sparkle@2x",
        (ToolIcon::Sparkle, DpiTier::X3) => "toolbar/sparkle@3x",
    }
}

/// Vertex tint for the textured icon: full white on hover, a touch dimmer at rest,
/// ~35% alpha when disabled.
pub(crate) fn icon_tint(enabled: bool, hovered: bool) -> Color32 {
    if !enabled {
        Color32::WHITE.gamma_multiply(0.35)
    } else if hovered {
        Color32::WHITE
    } else {
        Color32::from_gray(0xE6)
    }
}

/// Decode the embedded PNG into an egui image; 1x1 transparent on (impossible) decode failure.
fn decode_icon(icon: ToolIcon, tier: DpiTier) -> egui::ColorImage {
    match image::load_from_memory(icon_png_bytes(icon, tier)) {
        Ok(img) => {
            let rgba = img.to_rgba8();
            let size = [rgba.width() as usize, rgba.height() as usize];
            egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw())
        }
        Err(err) => {
            log::error!("toolbar icon {} failed to decode: {err}", icon_texture_name(icon, tier));
            egui::ColorImage::new([1, 1], Color32::TRANSPARENT)
        }
    }
}

/// Lazily upload + cache the texture for (icon, tier). The UI is single-threaded,
/// so a thread-local map is a per-context cache in practice.
fn icon_texture(ctx: &egui::Context, icon: ToolIcon, tier: DpiTier) -> egui::TextureHandle {
    use std::cell::RefCell;
    use std::collections::HashMap;
    thread_local! {
        static CACHE: RefCell<HashMap<(ToolIcon, DpiTier), egui::TextureHandle>> =
            RefCell::new(HashMap::new());
    }
    CACHE.with(|cache| {
        cache
            .borrow_mut()
            .entry((icon, tier))
            .or_insert_with(|| {
                ctx.load_texture(
                    icon_texture_name(icon, tier),
                    decode_icon(icon, tier),
                    egui::TextureOptions::LINEAR,
                )
            })
            .clone()
    })
}

/// A 22px icon button: hover pill + the SVG-master texture. `enabled=false` dims it (Debug stub).
pub fn tool_icon_button(
    ui: &mut egui::Ui,
    icon: ToolIcon,
    enabled: bool,
    tip: &str,
) -> egui::Response {
    let size = egui::Vec2::splat(22.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let tier = dpi_tier(ui.ctx().pixels_per_point());
        let texture = icon_texture(ui.ctx(), icon, tier);
        let p = ui.painter();
        if resp.hovered() && enabled {
            p.rect_filled(rect, egui::Rounding::same(4.0), crate::style::colors::HOVER_LIFT());
        }
        p.image(
            texture.id(),
            rect,
            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
            icon_tint(enabled, resp.hovered()),
        );
    }
    resp.on_hover_text(tip)
}

/// Rail toggle: like [`tool_icon_button`] but with a persistent "active" state — active gets a
/// raised pill + full-brightness tint, inactive dims until hovered.
pub fn tool_icon_toggle(ui: &mut egui::Ui, icon: ToolIcon, active: bool, tip: &str) -> egui::Response {
    let size = egui::Vec2::splat(26.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let tier = dpi_tier(ui.ctx().pixels_per_point());
        let texture = icon_texture(ui.ctx(), icon, tier);
        let p = ui.painter();
        if active {
            p.rect_filled(rect, egui::Rounding::same(5.0), crate::style::colors::HOVER_LIFT());
        } else if resp.hovered() {
            p.rect_filled(rect, egui::Rounding::same(5.0), crate::style::colors::HOVER_WASH());
        }
        let tint = if active || resp.hovered() {
            Color32::WHITE
        } else {
            Color32::from_gray(150)
        };
        p.image(
            texture.id(),
            rect.shrink(3.0),
            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
            tint,
        );
    }
    resp.on_hover_text(tip)
}

#[cfg(test)]
mod toolbar_tests {
    use super::*;

    const ALL_ICONS: [ToolIcon; 9] = [
        ToolIcon::Pin,
        ToolIcon::Structure,
        ToolIcon::History,
        ToolIcon::Sparkle,
        ToolIcon::Run,
        ToolIcon::Build,
        ToolIcon::Debug,
        ToolIcon::Settings,
        ToolIcon::Search,
    ];

    #[test]
    fn dpi_tier_switches_past_one_and_a_half() {
        assert_eq!(dpi_tier(1.0), DpiTier::X2);
        assert_eq!(dpi_tier(1.25), DpiTier::X2);
        assert_eq!(dpi_tier(1.5), DpiTier::X2);
        assert_eq!(dpi_tier(1.51), DpiTier::X3);
        assert_eq!(dpi_tier(2.0), DpiTier::X3);
        assert_eq!(dpi_tier(3.0), DpiTier::X3);
    }

    #[test]
    fn every_icon_embeds_a_png_at_both_tiers() {
        const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1A, b'\n'];
        for icon in ALL_ICONS {
            for tier in [DpiTier::X2, DpiTier::X3] {
                let bytes = icon_png_bytes(icon, tier);
                assert!(!bytes.is_empty(), "{}", icon_texture_name(icon, tier));
                assert_eq!(&bytes[..8], &PNG_MAGIC, "{}", icon_texture_name(icon, tier));
            }
        }
    }

    #[test]
    fn embedded_pngs_decode_at_expected_sizes() {
        for icon in ALL_ICONS {
            for (tier, px) in [(DpiTier::X2, 48), (DpiTier::X3, 72)] {
                let img = image::load_from_memory(icon_png_bytes(icon, tier))
                    .unwrap_or_else(|e| panic!("{}: {e}", icon_texture_name(icon, tier)));
                assert_eq!((img.width(), img.height()), (px, px), "{}", icon_texture_name(icon, tier));
            }
        }
    }

    #[test]
    fn icon_tint_hover_brightens_and_disabled_fades() {
        let rest = icon_tint(true, false);
        let hover = icon_tint(true, true);
        let disabled = icon_tint(false, false);
        assert_eq!(hover, Color32::WHITE);
        assert!(rest.r() < hover.r());
        assert_eq!(rest.a(), 255);
        assert!(disabled.a() < 128, "disabled alpha ~35%, got {}", disabled.a());
        // hover state is irrelevant while disabled
        assert_eq!(icon_tint(false, true), disabled);
    }
}
