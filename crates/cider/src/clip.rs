//! clip.rs — the system clipboard, over the core `wl_data_device` path.
//!
//! egui only hands the app clipboard text through its own Ctrl+V `Event::Paste`; the terminal also
//! pastes on Shift+Insert, middle-click and the context menu, so it needs to read the clipboard on
//! demand. Under Rune the *only* clipboard protocol an ordinary client may bind is the focus-gated
//! `wl_data_device` — `zwlr_data_control_manager_v1` (what arboard's Wayland backend speaks) is
//! restricted to the `rclip` clipboard manager, so that any app cannot silently sniff copies. So we
//! hold our own smithay-clipboard, built from eframe's Wayland display, and talk the core path.
//!
//! arboard stays as the fallback for non-Wayland (X11 / XWayland) sessions, where it works fine.

use std::sync::{Mutex, OnceLock};

use raw_window_handle::RawDisplayHandle;

/// The Wayland clipboard, created once from the display handle at startup. `None` off Wayland.
static CLIP: OnceLock<Option<Mutex<smithay_clipboard::Clipboard>>> = OnceLock::new();

/// Bind the clipboard to eframe's Wayland connection. Call once, at app construction.
pub fn init(display: Option<RawDisplayHandle>) {
    CLIP.get_or_init(|| match display {
        // SAFETY: the pointer comes from the live winit/eframe Wayland display, which outlives the
        // clipboard (both die with the process), as smithay_clipboard::Clipboard::new requires.
        Some(RawDisplayHandle::Wayland(d)) => {
            Some(Mutex::new(unsafe { smithay_clipboard::Clipboard::new(d.display.as_ptr()) }))
        }
        _ => None,
    });
}

/// Read the system clipboard as text, or `None` when it is empty / holds no text.
pub fn read() -> Option<String> {
    let debug = std::env::var_os("CIDER_CLIP_DEBUG").is_some();
    if let Some(Some(clip)) = CLIP.get() {
        // A panic elsewhere while the lock was held poisons it; the clipboard state itself is
        // still sound, so recover the guard instead of cascading the failure.
        return match clip.lock().unwrap_or_else(|e| e.into_inner()).load() {
            Ok(text) => Some(text),
            Err(e) => {
                if debug {
                    eprintln!("[cider] wayland clipboard read: {e}");
                }
                None
            }
        };
    }
    // X11 / not yet initialised.
    match arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
        Ok(t) => Some(t),
        Err(e) => {
            if debug {
                eprintln!("[cider] arboard clipboard read: {e}");
            }
            None
        }
    }
}

/// Put text on the system clipboard (selection copy, OSC 52, the Copy menu item).
pub fn write(text: String) {
    if let Some(Some(clip)) = CLIP.get() {
        clip.lock().unwrap_or_else(|e| e.into_inner()).store(text);
        return;
    }
    if let Ok(mut c) = arboard::Clipboard::new() {
        let _ = c.set_text(text);
    }
}
