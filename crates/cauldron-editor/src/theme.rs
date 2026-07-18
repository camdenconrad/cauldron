//! Editor theming — a lock-free light/dark switch the app flips alongside its chrome theme.
//!
//! The editor is a standalone crate (it can't see the app's `style::colors`), so it keeps its
//! own tiny palette here. Every painted color — chrome (caret, gutter, selection) and syntax
//! (`highlight::color`) — reads [`is_light`] on access, so flipping the theme re-paints the
//! whole editor with no per-buffer state to update.

use std::sync::atomic::{AtomicBool, Ordering};

use egui::Color32;

static LIGHT: AtomicBool = AtomicBool::new(false);

/// Switch the editor palette. Called by the app when its theme changes.
pub fn set_light(light: bool) {
    LIGHT.store(light, Ordering::Relaxed);
}

/// True while the light editor palette is active.
#[inline]
pub fn is_light() -> bool {
    LIGHT.load(Ordering::Relaxed)
}

/// Pick the dark or light value for the active theme.
#[inline]
pub fn pick(dark: Color32, light: Color32) -> Color32 {
    if is_light() { light } else { dark }
}
