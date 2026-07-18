//! Full-script text: install the system Noto fonts as egui fallbacks at startup so program output
//! in ANY language renders real glyphs instead of tofu (egui's bundled fonts only cover roughly
//! Latin/Greek/Cyrillic). Port of Luna's `crates/luna/src/ui/fonts.rs`.
//!
//! Known limitation: egui's layouter picks glyphs per-codepoint and does no complex-script
//! SHAPING, so Arabic won't join and Indic conjuncts won't form — but every character shows a
//! correct standalone glyph. Colour emoji are handled separately as textures (see [`crate::emoji`]);
//! a colour font in this chain would do nothing.
//!
//! Grid safety: cell metrics come from the default monospace face's `M` (`app::update`), and the
//! chain is APPENDED after egui's defaults, so installing it never changes the cell geometry —
//! fallback glyphs just paint into the cells alacritty already allotted them.
//!
//! Cost model (why this file looks the way it does): the chain is ~21.8MB on disk, and egui
//! CLONES every `Cow::Owned` font at each `Fonts::new` — once at first `begin_pass` and again on
//! every `pixels_per_point` change (`egui-0.29.1/src/context.rs` `update_fonts_mut`:
//! `self.font_definitions.clone()`; `epaint-0.29.1/src/text/fonts.rs`
//! `ab_glyph_font_from_font_data`: the `Cow::Owned` arm calls `bytes.clone()`, the
//! `Cow::Borrowed` arm parses the slice in place). So:
//!
//! - ZERO-COPY: each face is read from disk exactly once per process, leaked to `&'static [u8]`
//!   (fonts never uninstall — the leak is the point), and handed to egui via
//!   [`FontData::from_static`] so every later `FontDefinitions` clone copies a fat pointer, not
//!   megabytes.
//! - STAGED: [`install_core`] synchronously installs only the small faces an IDE's chrome needs
//!   at first paint (~1.5MB), and [`install_full_async`] tops up to the full chain from a
//!   background thread. [`install`] keeps the old synchronous everything-at-once behavior.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use egui::{FontData, FontDefinitions, FontFamily};

/// The fallback chain, appended after egui's defaults. Order matters: the Latin base first so it
/// wins shared codepoints, per-script fonts next, and the enormous CJK collection last so its
/// broad cmap doesn't shadow the better script-specific faces. `.ttc` entries carry the face
/// index to use (0 = the JP face of NotoSansCJK, whose cmap spans all the CJK locales).
///
/// The final `bool` marks the CORE faces: small files (Latin base + symbol/pictograph coverage,
/// ~1.5MB total) that IDE chrome wants at first paint. Everything else — the per-script faces and
/// the 19.5MB CJK collection — is the deferred remainder loaded by [`install_full_async`].
const CHAIN: &[(&str, &str, u32, bool)] = &[
    ("noto-sans", "/usr/share/fonts/noto/NotoSans-Regular.ttf", 0, true),
    ("noto-arabic", "/usr/share/fonts/noto/NotoSansArabic-Regular.ttf", 0, false),
    ("noto-hebrew", "/usr/share/fonts/noto/NotoSansHebrew-Regular.ttf", 0, false),
    ("noto-devanagari", "/usr/share/fonts/noto/NotoSansDevanagari-Regular.ttf", 0, false),
    ("noto-bengali", "/usr/share/fonts/noto/NotoSansBengali-Regular.ttf", 0, false),
    ("noto-tamil", "/usr/share/fonts/noto/NotoSansTamil-Regular.ttf", 0, false),
    ("noto-thai", "/usr/share/fonts/noto/NotoSansThai-Regular.ttf", 0, false),
    ("noto-symbols", "/usr/share/fonts/noto/NotoSansSymbols-Regular.ttf", 0, true),
    ("noto-symbols2", "/usr/share/fonts/noto/NotoSansSymbols2-Regular.ttf", 0, true),
    ("noto-cjk", "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc", 0, false),
];

/// One cell per chain entry: `None` (inner) = the file was missing at first request. Filled at
/// most once per process — repeated installs reuse the same leaked slice, never re-read or
/// re-leak.
static FACE_BYTES: [OnceLock<Option<&'static [u8]>>; CHAIN.len()] =
    [const { OnceLock::new() }; CHAIN.len()];

/// The bytes of chain face `i`, read+leaked on first request and cached for the process lifetime.
/// `None` when the file is absent on this box (a shorter chain must never take the terminal down)
/// — and the miss is cached too, so an absent face costs one failed `read` ever.
fn face_bytes(i: usize) -> Option<&'static [u8]> {
    *FACE_BYTES[i].get_or_init(|| {
        std::fs::read(CHAIN[i].1).ok().map(|bytes| &*Box::leak(bytes.into_boxed_slice()))
    })
}

/// egui's default `FontDefinitions` plus every PRESENT chain face — all of them, or just the core
/// subset — appended to BOTH family fallback lists in chain order. Zero-copy: font bytes go in as
/// `Cow::Borrowed` statics, so egui's per-`pixels_per_point` `FontDefinitions` clones stay cheap.
/// Skipping non-core faces happens BEFORE `face_bytes`, so a core-only build never touches the
/// heavy files on disk.
fn build_defs(core_only: bool) -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    for (i, &(name, _path, index, core)) in CHAIN.iter().enumerate() {
        if core_only && !core {
            continue;
        }
        let Some(bytes) = face_bytes(i) else { continue };
        let mut data = FontData::from_static(bytes);
        data.index = index;
        fonts.font_data.insert(name.to_owned(), data);
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            fonts.families.entry(family).or_default().push(name.to_owned());
        }
    }
    fonts
}

/// Install the full chain synchronously (the original behavior, now zero-copy). Called once from
/// `main` with the `CreationContext`'s egui context. Prefer [`install_core`] +
/// [`install_full_async`] where first-paint latency matters (Cauldron boots its IDE shell before
/// any terminal pane opens).
pub fn install(ctx: &egui::Context) {
    install_core(ctx);
    ctx.set_fonts(build_defs(false));
}

/// Synchronously install ONLY the small core faces (Latin + symbols, ~1.5MB) so first paint has
/// real glyphs for UI chrome without reading the multi-megabyte script/CJK files. Follow up with
/// [`install_full_async`] to get the complete fallback chain.
pub fn install_core(ctx: &egui::Context) {
    ctx.set_fonts(build_defs(true));
}

/// Load the remaining heavy faces on a background thread ("cider-fonts") and swap in the FULL
/// `FontDefinitions` — identical faces, families, and ordering to what [`install`] builds, so
/// fallback behavior matches exactly. `egui::Context` is a cheap `Arc` clone and `Send + Sync`,
/// so `set_fonts` + `request_repaint` are safe from the worker; egui applies the new definitions
/// at the next pass. Double-spawns are ignored (first caller wins, for the process lifetime).
pub fn install_full_async(ctx: &egui::Context) {
    static SPAWNED: AtomicBool = AtomicBool::new(false);
    if SPAWNED.swap(true, Ordering::SeqCst) {
        return;
    }
    let ctx = ctx.clone();
    let spawned = std::thread::Builder::new().name("cider-fonts".into()).spawn(move || {
        let defs = build_defs(false);
        ctx.set_fonts(defs);
        ctx.request_repaint();
    });
    if spawned.is_err() {
        // Thread spawn failing is essentially OOM, but a terminal with a shorter font chain
        // still beats no terminal: allow a later retry.
        SPAWNED.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The full definitions must be IDENTICAL whichever route reaches them: `install()` and the
    /// `install_core` → `install_full_async` worker both call `build_defs(false)`, and this pins
    /// that two builds agree on faces, family ordering, and (zero-copy) the exact same leaked
    /// bytes — no re-read, no re-leak, no `Cow::Owned` sneaking back in.
    #[test]
    fn staged_full_matches_direct_install() {
        let direct = build_defs(false); // what install() sets
        let staged = build_defs(false); // what the cider-fonts worker sets

        // Same family fallback lists, same order.
        assert_eq!(direct.families, staged.families);
        // Same faces by name.
        let mut direct_names: Vec<_> = direct.font_data.keys().collect();
        let mut staged_names: Vec<_> = staged.font_data.keys().collect();
        direct_names.sort();
        staged_names.sort();
        assert_eq!(direct_names, staged_names);
        // Zero-copy invariant: every chain face is Cow::Borrowed of the SAME leaked slice.
        for (name, data) in &direct.font_data {
            let other = &staged.font_data[name];
            assert_eq!(data.index, other.index, "{name}: face index must match");
            match (&data.font, &other.font) {
                (std::borrow::Cow::Borrowed(a), std::borrow::Cow::Borrowed(b)) => {
                    assert!(
                        std::ptr::eq(*a, *b),
                        "{name}: repeated builds must reuse the same leaked bytes"
                    );
                }
                // egui's bundled defaults are from_static too, but tolerate whatever the
                // upstream defaults do — the invariant is about OUR chain faces.
                _ => assert!(
                    !CHAIN.iter().any(|&(n, ..)| n == name),
                    "{name}: chain faces must be Cow::Borrowed (zero-copy)"
                ),
            }
        }
    }

    /// Core is a strict subset of full with relative order preserved: stripping the non-core
    /// names out of the full family lists yields exactly the core family lists, so the staged
    /// upgrade only ever APPENDS fallbacks (no reordering that could change glyph resolution for
    /// text already on screen).
    #[test]
    fn core_is_ordered_subset_of_full() {
        let core = build_defs(true);
        let full = build_defs(false);
        let core_face = |name: &str| {
            CHAIN.iter().find(|&&(n, ..)| n == name).is_none_or(|&(.., is_core)| is_core)
        };
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            let full_stripped: Vec<_> = full.families[&family]
                .iter()
                .filter(|name| core_face(name))
                .cloned()
                .collect();
            assert_eq!(full_stripped, core.families[&family], "{family:?}");
        }
        // And core really is small: it must not include the heavy script/CJK faces.
        for &(name, _, _, is_core) in CHAIN {
            assert_eq!(
                core.font_data.contains_key(name),
                is_core && full.font_data.contains_key(name),
                "{name}: core membership"
            );
        }
    }
}
