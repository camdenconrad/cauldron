//! cider — a native Rust/wgpu terminal emulator for the Rune desktop.
//!
//! A single-window terminal: `$SHELL` on a real PTY (portable-pty), parsed by Alacritty's VTE
//! engine (alacritty_terminal), rendered with egui's painter over the shared Rune chrome/theme
//! (livewall-uikit). Truecolor + 256 + 16 ANSI, bold/italic/underline/reverse, block/bar/underline
//! cursor with blink, full keyboard → PTY (control/alt/function keys, bracketed paste), scrollback
//! with the wheel, mouse-selection → clipboard, and resize → PTY winsize. One window, one shell for
//! v1; each shell is modelled as a self-contained `Session` held in a `Vec` so tabs can be added
//! later. Implements GitHub issue #7.

#![windows_subsystem = "windows"]

mod app;

fn main() -> eframe::Result {
    env_logger::init();

    let cfg = cider::config::Config::load();

    // DEFAULT VULKAN, matching rmon: wgpu 22's GL backend enumerates no adapter on this box
    // (NVIDIA/EGL), and eframe has no fallback, so a GL default aborts. RTERM_BACKEND=gl re-tests GL.
    let backends = match std::env::var("RTERM_BACKEND").ok().as_deref() {
        Some("gl") | Some("gles") => eframe::wgpu::Backends::GL,
        _ => eframe::wgpu::Backends::VULKAN,
    };
    let transparent = cfg.opacity < 1.0;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("cider")
            .with_app_id("com.coffee.cider") // Wayland app_id → dock icon match / per-app scale
            .with_decorations(false) // we draw our own title bar — Rune has no SSD
            .with_transparent(transparent)
            .with_inner_size([750.0, 467.0]) // the size the user settled on
            .with_min_inner_size([340.0, 220.0])
            .with_resizable(true),
        wgpu_options: eframe::egui_wgpu::WgpuConfiguration {
            supported_backends: backends,
            ..Default::default()
        },
        ..Default::default()
    };
    eframe::run_native(
        "cider",
        options,
        Box::new(|cc| {
            // Noto fallback chain first, so the very first frame already has full-script glyphs.
            cider::fonts::install(&cc.egui_ctx);
            Ok(Box::new(app::Rterm::new(cc, cfg)))
        }),
    )
}
