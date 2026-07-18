//! config.rs — cider's user config file and its colour palette.
//!
//! Loads `~/.config/cider/config.toml` (XDG-aware, mirroring how the launcher finds its config dir).
//! Every field is optional; a missing or unparseable file yields sensible Rune defaults — a warm
//! charcoal background, a burnt-orange cursor and an accurate 16-colour ANSI set. The palette also
//! owns the mapping from Alacritty's abstract cell colours (named / 256-indexed / truecolor) to
//! concrete RGB, honouring any dynamic OSC overrides the running program set.

use std::path::PathBuf;

use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use serde::Deserialize;

/// Resolved runtime configuration.
pub struct Config {
    /// Monospace font size in points. (The family is recorded but v1 renders with egui's built-in
    /// monospace face — see the note in `app::render`.)
    pub font_size: f32,
    pub font_family: String,
    /// Scrollback history in lines.
    pub scrollback: usize,
    /// Window/background opacity in 0.0..=1.0 (1.0 = fully opaque).
    pub opacity: f32,
    pub palette: Palette,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            font_size: 13.0,
            font_family: "monospace".into(),
            scrollback: 10_000,
            opacity: 1.0,
            palette: Palette::default(),
        }
    }
}

impl Config {
    /// Load the config, falling back to defaults for anything missing or malformed.
    pub fn load() -> Self {
        let path = config_dir().join("cider").join("config.toml");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        let raw: RawConfig = match toml::from_str(&text) {
            Ok(r) => r,
            Err(e) => {
                log::warn!("cider: ignoring bad config {}: {e}", path.display());
                return Self::default();
            }
        };
        let def = Self::default();
        let mut palette = Palette::default();
        if let Some(p) = raw.palette {
            if let Some(c) = p.foreground.as_deref().and_then(parse_hex) {
                palette.fg = c;
            }
            if let Some(c) = p.background.as_deref().and_then(parse_hex) {
                palette.bg = c;
            }
            if let Some(c) = p.cursor.as_deref().and_then(parse_hex) {
                palette.cursor = c;
            }
            // Optional full 16-colour ANSI override; only applied when exactly 16 valid hex entries.
            if let Some(list) = p.ansi {
                if list.len() == 16 {
                    let parsed: Vec<Rgb> = list.iter().filter_map(|s| parse_hex(s)).collect();
                    if parsed.len() == 16 {
                        palette.ansi.copy_from_slice(&parsed);
                    }
                }
            }
        }
        Self {
            font_size: raw.font_size.unwrap_or(def.font_size).clamp(6.0, 72.0),
            font_family: raw.font_family.unwrap_or(def.font_family),
            // Capped at Alacritty's own scrollback ceiling — an absurd value in the config file
            // must not translate into an unbounded grid allocation.
            scrollback: raw.scrollback.unwrap_or(def.scrollback).min(100_000),
            opacity: raw.opacity.unwrap_or(def.opacity).clamp(0.2, 1.0),
            palette,
        }
    }
}

/// The 16 ANSI colours plus the default fg / bg / cursor. Deliberately accurate ANSI (so `ls`,
/// prompts and TUIs look right) over a warm Rune-charcoal ground with a burnt-orange cursor.
#[derive(Clone)]
pub struct Palette {
    pub fg: Rgb,
    pub bg: Rgb,
    pub cursor: Rgb,
    pub ansi: [Rgb; 16],
}

const fn rgb(r: u8, g: u8, b: u8) -> Rgb {
    Rgb { r, g, b }
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            fg: rgb(238, 235, 232),  // uikit warm off-white
            bg: rgb(19, 18, 22),     // uikit CHROME charcoal
            cursor: rgb(233, 110, 44), // uikit burnt orange
            ansi: [
                rgb(33, 33, 38),    // 0 black
                rgb(224, 86, 72),   // 1 red
                rgb(152, 195, 121), // 2 green
                rgb(229, 181, 103), // 3 yellow
                rgb(97, 148, 232),  // 4 blue
                rgb(198, 120, 221), // 5 magenta
                rgb(86, 182, 194),  // 6 cyan
                rgb(198, 198, 198), // 7 white
                rgb(90, 92, 98),    // 8 bright black
                rgb(240, 113, 100), // 9 bright red
                rgb(175, 215, 120), // 10 bright green
                rgb(240, 205, 130), // 11 bright yellow
                rgb(120, 170, 255), // 12 bright blue
                rgb(215, 150, 235), // 13 bright magenta
                rgb(110, 205, 215), // 14 bright cyan
                rgb(238, 235, 232), // 15 bright white
            ],
        }
    }
}

impl Palette {
    /// Resolve a cell colour to concrete RGB. Dynamic OSC overrides (`dyn_colors`, set by the running
    /// program via OSC 4/10/11…) win over the static palette; truecolor passes straight through.
    pub fn resolve(&self, c: Color, dyn_colors: &Colors) -> Rgb {
        match c {
            Color::Spec(rgb) => rgb,
            Color::Named(n) => dyn_colors[n].unwrap_or_else(|| self.named(n)),
            Color::Indexed(i) => dyn_colors[i as usize].unwrap_or_else(|| self.indexed(i)),
        }
    }

    fn named(&self, n: NamedColor) -> Rgb {
        use NamedColor::*;
        match n {
            Black => self.ansi[0],
            Red => self.ansi[1],
            Green => self.ansi[2],
            Yellow => self.ansi[3],
            Blue => self.ansi[4],
            Magenta => self.ansi[5],
            Cyan => self.ansi[6],
            White => self.ansi[7],
            BrightBlack => self.ansi[8],
            BrightRed => self.ansi[9],
            BrightGreen => self.ansi[10],
            BrightYellow => self.ansi[11],
            BrightBlue => self.ansi[12],
            BrightMagenta => self.ansi[13],
            BrightCyan => self.ansi[14],
            BrightWhite => self.ansi[15],
            Foreground | BrightForeground => self.fg,
            Background => self.bg,
            Cursor => self.cursor,
            DimForeground => dim(self.fg),
            DimBlack => dim(self.ansi[0]),
            DimRed => dim(self.ansi[1]),
            DimGreen => dim(self.ansi[2]),
            DimYellow => dim(self.ansi[3]),
            DimBlue => dim(self.ansi[4]),
            DimMagenta => dim(self.ansi[5]),
            DimCyan => dim(self.ansi[6]),
            DimWhite => dim(self.ansi[7]),
        }
    }

    /// Map an xterm 256-colour index to RGB: 0..16 = the ANSI palette, 16..232 = the 6×6×6 cube,
    /// 232..256 = the 24-step grayscale ramp (the standard xterm formulas).
    fn indexed(&self, i: u8) -> Rgb {
        match i {
            0..=15 => self.ansi[i as usize],
            16..=231 => {
                let i = i - 16;
                let step = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
                rgb(step(i / 36), step((i % 36) / 6), step(i % 6))
            }
            232..=255 => {
                let v = 8 + (i - 232) * 10;
                rgb(v, v, v)
            }
        }
    }
}

/// Alacritty's own default-dim scaling (×2/3), so DIM cells match the engine's expectations.
pub fn dim(c: Rgb) -> Rgb {
    let s = |v: u8| (v as f32 * 0.66) as u8;
    rgb(s(c.r), s(c.g), s(c.b))
}

/// Parse `#rrggbb` / `rrggbb` (and `#rgb`) hex into RGB. None on any malformation.
fn parse_hex(s: &str) -> Option<Rgb> {
    let h = s.trim().trim_start_matches('#');
    // Byte-range slicing below panics on a UTF-8 char boundary, so reject non-ASCII up front
    // (hex digits are ASCII anyway) — a config with "é0" must yield None, never a panic.
    if !h.is_ascii() {
        return None;
    }
    let (r, g, b) = match h.len() {
        6 => (
            u8::from_str_radix(&h[0..2], 16).ok()?,
            u8::from_str_radix(&h[2..4], 16).ok()?,
            u8::from_str_radix(&h[4..6], 16).ok()?,
        ),
        3 => {
            let d = |c: &str| u8::from_str_radix(c, 16).ok().map(|v| v * 17);
            (d(&h[0..1])?, d(&h[1..2])?, d(&h[2..3])?)
        }
        _ => return None,
    };
    Some(rgb(r, g, b))
}

fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"))
}

/// The on-disk schema (all optional). Kept separate from [`Config`] so parsing can be lenient.
#[derive(Deserialize)]
struct RawConfig {
    font_size: Option<f32>,
    font_family: Option<String>,
    scrollback: Option<usize>,
    opacity: Option<f32>,
    palette: Option<RawPalette>,
}

#[derive(Deserialize)]
struct RawPalette {
    foreground: Option<String>,
    background: Option<String>,
    cursor: Option<String>,
    ansi: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_parsing() {
        assert_eq!(parse_hex("#ff8800"), Some(rgb(255, 136, 0)));
        assert_eq!(parse_hex("00ff00"), Some(rgb(0, 255, 0)));
        assert_eq!(parse_hex("#f80"), Some(rgb(255, 136, 0)));
        assert_eq!(parse_hex("nope"), None);
        // Non-ASCII input whose BYTE length matches a hex form must not panic on a char boundary.
        assert_eq!(parse_hex("é0"), None); // 3 bytes → the #rgb arm
        assert_eq!(parse_hex("#ééé"), None); // 6 bytes → the #rrggbb arm
        assert_eq!(parse_hex(""), None);
    }

    #[test]
    fn cube_and_grayscale() {
        let p = Palette::default();
        // 16 = cube origin (0,0,0); 231 = cube max (255,255,255); 255 = brightest gray.
        assert_eq!(p.indexed(16), rgb(0, 0, 0));
        assert_eq!(p.indexed(231), rgb(255, 255, 255));
        assert_eq!(p.indexed(232), rgb(8, 8, 8));
        assert_eq!(p.indexed(255), rgb(238, 238, 238));
    }

    #[test]
    fn truecolor_passthrough() {
        let p = Palette::default();
        let dyn_colors = Colors::default();
        assert_eq!(p.resolve(Color::Spec(rgb(1, 2, 3)), &dyn_colors), rgb(1, 2, 3));
    }
}
