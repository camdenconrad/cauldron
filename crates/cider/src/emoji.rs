//! Colour emoji as textures, painted per grid cell.
//!
//! egui cannot render COLR/CBDT colour fonts as text glyphs, so cider goes around the text
//! pipeline (the same way Luna does in `crates/luna/src/ui/emoji.rs`): [`Emoji::texture`] pulls
//! the emoji's PNG bitmap out of the system NotoColorEmoji's CBDT table (ttf-parser), decodes it
//! (`image`), uploads it once as an egui texture, and caches the handle. The renderer paints the
//! handle inside the cell rect alacritty already sized for the character, and falls back to the
//! plain text path when the lookup misses (font absent, or no bitmap for the codepoint).

use std::collections::HashMap;
use std::sync::OnceLock;

use egui::TextureHandle;

/// Where the Noto packages install Google's CBDT colour-emoji font.
const NOTO_COLOR_EMOJI: &str = "/usr/share/fonts/noto/NotoColorEmoji.ttf";

/// The colour-emoji rasteriser: owns the font bytes and one texture per emoji already shown.
pub struct Emoji {
    /// Raw font file, read LAZILY on the first [`Emoji::texture`] miss — NotoColorEmoji is
    /// 10.67MB, and a pane that never shows an emoji (or starts closed, as Cauldron's terminal
    /// does) shouldn't pay that read at construction. Inner `None` = the font isn't installed
    /// (the miss is cached, so every later lookup fails cleanly without retrying the disk).
    font: OnceLock<Option<Vec<u8>>>,
    /// Texture per base character, rasterised on first use. Misses are cached too (`None`) so an
    /// unsupported codepoint doesn't re-parse the font every frame it's on screen.
    cache: HashMap<char, Option<TextureHandle>>,
}

impl Emoji {
    /// Set up the rasteriser; does ZERO I/O (the font file is read on first rasterisation
    /// request). Never fails: a missing font just means `texture` always returns `None` and the
    /// renderer keeps its text fallback. The name is kept for API stability — cider's app and
    /// Cauldron's `TerminalPane` call it unchanged.
    pub fn load_system() -> Self {
        Self { font: OnceLock::new(), cache: HashMap::new() }
    }

    /// The font bytes, read from disk on first call and cached (hit or miss) for `self`'s life.
    fn font_bytes(&self) -> Option<&[u8]> {
        self.font.get_or_init(|| std::fs::read(NOTO_COLOR_EMOJI).ok()).as_deref()
    }

    /// The colour bitmap for `c` as a ready-to-paint texture, or `None` if it can't be rasterised
    /// (caller paints the text form instead).
    pub fn texture(&mut self, ctx: &egui::Context, c: char) -> Option<TextureHandle> {
        if let Some(hit) = self.cache.get(&c) {
            return hit.clone();
        }
        let tex = self.rasterize(ctx, c);
        self.cache.insert(c, tex.clone());
        tex
    }

    fn rasterize(&self, ctx: &egui::Context, c: char) -> Option<TextureHandle> {
        let bytes = self.font_bytes()?;
        // Face::parse is zero-copy header validation — cheap enough to redo per cache miss,
        // which sidesteps storing a self-referential Face-borrowing-font field.
        let face = ttf_parser::Face::parse(bytes, 0).ok()?;
        let glyph = face.glyph_index(c)?;
        // u16::MAX selects the largest strike (128px/em in NotoColorEmoji); CBDT stores PNG.
        let raster = face.glyph_raster_image(glyph, u16::MAX)?;
        if raster.format != ttf_parser::RasterImageFormat::PNG || raster.data.is_empty() {
            return None;
        }
        let decoded = image::load_from_memory(raster.data).ok()?.to_rgba8();
        let (w, h) = decoded.dimensions();
        if w == 0 || h == 0 {
            return None;
        }
        let img = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], decoded.as_raw());
        Some(ctx.load_texture(format!("emoji:{c}"), img, egui::TextureOptions::LINEAR))
    }
}

/// Should this cell take the colour-texture path?
///
/// Two gates, both required:
/// - `is_emoji(c)`: the base character lives in an emoji block at all.
/// - `wide || vs16`: the cell is double-width (alacritty's wcwidth said "emoji presentation"), or
///   the program attached U+FE0F to a narrow base (❤ + VS16) explicitly requesting it.
///
/// Narrow symbols WITHOUT a VS16 (→, ★, ☂ printed bare) stay on the text path — that matches what
/// terminals conventionally show, and a colour image is honest only where the emoji presentation
/// was actually selected. A `true` whose bitmap lookup then misses just falls back to text.
pub fn wants_texture(c: char, wide: bool, vs16: bool) -> bool {
    (wide || vs16) && is_emoji(c)
}

/// Is `c` in the emoji blocks? Deliberately generous — a `true` that fails to rasterise falls back
/// to text, while a `false` on a real emoji would paint tofu. (Same table Luna ships.)
pub fn is_emoji(c: char) -> bool {
    matches!(u32::from(c),
        0x1F300..=0x1FAFF          // pictographs, emoticons, transport, supplemental, extended-A
        | 0x1F1E6..=0x1F1FF        // regional indicators (flag pairs)
        | 0x1F004 | 0x1F0CF        // mahjong red dragon, joker
        | 0x2600..=0x27BF          // misc symbols + dingbats (sun, hearts, hands, sparkles)
        | 0x2B00..=0x2BFF          // stars and heavy arrows
        | 0x231A..=0x231B | 0x2328 // watch, hourglass, keyboard
        | 0x23E9..=0x23FA          // media-control symbols
        | 0x25AA..=0x25AB | 0x25B6 | 0x25C0 | 0x25FB..=0x25FE // geometric shapes used as emoji
        | 0x203C | 0x2049          // !! and !? exclamations
        | 0x2934 | 0x2935          // curved arrows
        | 0x3030 | 0x303D | 0x3297 | 0x3299 // wavy dash, part-alternation, ideograph circles
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Headless proof of the pipeline short of the GPU upload: the system NotoColorEmoji's CBDT
    /// table yields PNG bytes for 😀 that the `image` crate decodes to a real RGBA bitmap.
    /// Skips (passes) when the font isn't installed on this box.
    #[test]
    fn grinning_face_rasterises() {
        let Ok(bytes) = std::fs::read(NOTO_COLOR_EMOJI) else {
            eprintln!("skipping: {NOTO_COLOR_EMOJI} not installed");
            return;
        };
        let face = ttf_parser::Face::parse(&bytes, 0).expect("parse NotoColorEmoji");
        let glyph = face.glyph_index('😀').expect("cmap entry for U+1F600");
        let raster = face.glyph_raster_image(glyph, u16::MAX).expect("CBDT strike for 😀");
        assert_eq!(raster.format, ttf_parser::RasterImageFormat::PNG);
        assert!(!raster.data.is_empty(), "PNG bytes must be non-empty");
        let decoded = image::load_from_memory(raster.data).expect("decode emoji PNG").to_rgba8();
        assert!(decoded.width() > 0 && decoded.height() > 0, "decoded image must have area");
    }

    /// Constructing the rasteriser must do no I/O: the font cell stays unfilled until the first
    /// rasterisation request. (This is what lets Cauldron build a closed-by-default terminal pane
    /// without paying NotoColorEmoji's 10.67MB read at boot.)
    #[test]
    fn construction_reads_nothing() {
        let emoji = Emoji::load_system();
        assert!(emoji.font.get().is_none(), "font bytes must not be read at construction");
        // First actual request fills the cell (with the bytes, or a cached miss).
        assert!(emoji.font_bytes().is_some() || emoji.font.get().is_some());
    }

    #[test]
    fn texture_gate() {
        // Wide emoji: texture regardless of VS16 (😀 is wcwidth 2 → WIDE_CHAR in the grid).
        assert!(wants_texture('😀', true, false));
        assert!(wants_texture('🔥', true, false));
        // Narrow base + explicit VS16: texture (❤️ as programs actually emit it).
        assert!(wants_texture('❤', false, true));
        // Narrow symbol printed bare: text path, even though it's in an emoji block.
        assert!(!wants_texture('❤', false, false));
        assert!(!wants_texture('☂', false, false));
        // Plain text never takes the texture path, whatever the flags claim.
        assert!(!wants_texture('M', true, true));
        assert!(!wants_texture('字', true, false));
    }

    #[test]
    fn is_emoji_rejects_plain_text() {
        for c in ['a', 'ä', '字', '1', ' ', '→'] {
            assert!(!is_emoji(c), "{c:?} should not classify as emoji");
        }
        for c in ['😀', '👍', '🔥', '❤', '✨'] {
            assert!(is_emoji(c), "{c:?} should classify as emoji");
        }
    }
}
