//! Cauldron's IDE visual system — layered ON TOP of the shared Rune theme.
//!
//! Call order per frame (or once at startup + on theme change):
//!   1. `livewall_uikit::theme::apply(ctx)`   — the Rune family base
//!   2. `cauldron_style::apply_ide_style(ctx)` — this module's IDE overrides
//!
//! The design: JetBrains New UI structure, Rune autumn skin. Flat surfaces, ONE accent
//! (rust orange), low-contrast chrome vs high-contrast content, depth via fill deltas
//! (never strokes), borderless widgets that only reveal themselves on hover.
//!
//! Everything the app paints by hand should take its colors from [`colors`] and its
//! metrics from [`sizes`] — no inline `Color32::from_rgb` in app code.

#![allow(dead_code)]

use egui::text::LayoutJob;
use egui::{
    Color32, FontId, Pos2, Rect, Rounding, Sense, Stroke, TextFormat, Vec2,
};

// =================================================================================================
// colors — the complete Cauldron palette. Autumn brand: ORANGE is THE accent; amber/moss/plum are
// STATUS colors only (dirty, git, severity) and never used for interaction states.
// =================================================================================================
/// The shipped themes. Dark is the original hand-tuned palette (default). Runtime-selectable —
/// [`colors`] reads [`ACTIVE_THEME`] on every access (a lock-free atomic branch, no locking).
/// `System` is not a palette: the app resolves it to a concrete theme from the OS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Theme {
    Dark,
    Light,
    Midnight,
    Amber,
}

/// Every field the hand-painted UI needs. One instance per theme.
#[derive(Clone, Copy)]
pub struct Palette {
    pub bg_editor: egui::Color32,
    pub bg_panel: egui::Color32,
    pub bg_overlay: egui::Color32,
    pub bg_raised: egui::Color32,
    pub bg_active: egui::Color32,
    pub bg_input: egui::Color32,
    pub accent: egui::Color32,
    pub accent_hi: egui::Color32,
    pub accent_row: egui::Color32,
    pub accent_wash: egui::Color32,
    pub accent_selection: egui::Color32,
    pub text: egui::Color32,
    pub text_muted: egui::Color32,
    pub text_faint: egui::Color32,
    pub amber: egui::Color32,
    pub moss: egui::Color32,
    pub plum: egui::Color32,
    pub error: egui::Color32,
    pub warn: egui::Color32,
    pub hairline: egui::Color32,
    pub border: egui::Color32,
    pub indent_guide: egui::Color32,
    pub hover_wash: egui::Color32,
    pub hover_lift: egui::Color32,
}

use egui::Color32 as C;

/// The original near-black autumn palette.
const DARK: Palette = Palette {
    bg_editor: C::from_rgb(14, 13, 17),
    bg_panel: C::from_rgb(28, 27, 31),
    bg_overlay: C::from_rgb(33, 32, 38),
    bg_raised: C::from_rgb(38, 37, 43),
    bg_active: C::from_rgb(48, 46, 55),
    bg_input: C::from_rgb(17, 16, 20),
    accent: C::from_rgb(233, 110, 44),
    accent_hi: C::from_rgb(248, 140, 74),
    accent_row: C::from_rgba_premultiplied(27, 13, 5, 30),
    accent_wash: C::from_rgba_premultiplied(37, 17, 7, 40),
    accent_selection: C::from_rgba_premultiplied(53, 25, 10, 58),
    text: C::from_rgb(238, 235, 232),
    text_muted: C::from_rgb(176, 172, 168),
    text_faint: C::from_rgb(128, 124, 122),
    amber: C::from_rgb(217, 164, 65),
    moss: C::from_rgb(163, 190, 140),
    plum: C::from_rgb(150, 110, 184),
    error: C::from_rgb(224, 82, 60),
    warn: C::from_rgb(230, 180, 60),
    hairline: C::from_rgba_premultiplied(18, 18, 18, 18),
    border: C::from_rgba_premultiplied(38, 38, 38, 38),
    indent_guide: C::from_rgba_premultiplied(40, 40, 40, 40),
    hover_wash: C::from_rgba_premultiplied(14, 14, 14, 14),
    hover_lift: C::from_rgba_premultiplied(28, 28, 32, 30),
};

/// Paper-and-ink counterpart, same rust accent.
const LIGHT: Palette = Palette {
    bg_editor: C::from_rgb(253, 252, 250),
    bg_panel: C::from_rgb(240, 238, 234),
    bg_overlay: C::from_rgb(248, 246, 242),
    bg_raised: C::from_rgb(232, 229, 224),
    bg_active: C::from_rgb(219, 215, 209),
    bg_input: C::from_rgb(255, 255, 255),
    accent: C::from_rgb(214, 92, 26),
    accent_hi: C::from_rgb(196, 78, 16),
    accent_row: C::from_rgba_premultiplied(214, 92, 26, 26),
    accent_wash: C::from_rgba_premultiplied(214, 92, 26, 34),
    accent_selection: C::from_rgba_premultiplied(214, 92, 26, 50),
    text: C::from_rgb(30, 28, 26),
    text_muted: C::from_rgb(90, 86, 82),
    text_faint: C::from_rgb(140, 135, 130),
    amber: C::from_rgb(166, 118, 20),
    moss: C::from_rgb(78, 121, 48),
    plum: C::from_rgb(120, 74, 160),
    error: C::from_rgb(198, 40, 22),
    warn: C::from_rgb(176, 120, 8),
    hairline: C::from_rgba_premultiplied(0, 0, 0, 20),
    border: C::from_rgba_premultiplied(0, 0, 0, 40),
    indent_guide: C::from_rgba_premultiplied(0, 0, 0, 28),
    hover_wash: C::from_rgba_premultiplied(0, 0, 0, 14),
    hover_lift: C::from_rgba_premultiplied(0, 0, 0, 20),
};

/// Cool blue-black — deeper and calmer than DARK; a slate-blue tint on every surface.
const MIDNIGHT: Palette = Palette {
    bg_editor: C::from_rgb(12, 14, 20),
    bg_panel: C::from_rgb(22, 26, 34),
    bg_overlay: C::from_rgb(27, 31, 40),
    bg_raised: C::from_rgb(32, 37, 48),
    bg_active: C::from_rgb(42, 48, 62),
    bg_input: C::from_rgb(15, 18, 24),
    accent: C::from_rgb(233, 110, 44),
    accent_hi: C::from_rgb(248, 140, 74),
    accent_row: C::from_rgba_premultiplied(27, 13, 5, 30),
    accent_wash: C::from_rgba_premultiplied(37, 17, 7, 40),
    accent_selection: C::from_rgba_premultiplied(40, 60, 90, 60),
    text: C::from_rgb(226, 230, 238),
    text_muted: C::from_rgb(160, 168, 182),
    text_faint: C::from_rgb(112, 120, 134),
    amber: C::from_rgb(217, 164, 65),
    moss: C::from_rgb(150, 190, 160),
    plum: C::from_rgb(150, 130, 200),
    error: C::from_rgb(230, 90, 80),
    warn: C::from_rgb(230, 180, 60),
    hairline: C::from_rgba_premultiplied(180, 200, 230, 16),
    border: C::from_rgba_premultiplied(180, 200, 230, 34),
    indent_guide: C::from_rgba_premultiplied(180, 200, 230, 28),
    hover_wash: C::from_rgba_premultiplied(120, 150, 200, 16),
    hover_lift: C::from_rgba_premultiplied(150, 175, 220, 26),
};

/// Warm high-contrast dark — leans all the way into the rust/amber autumn, brighter text.
const AMBER_THEME: Palette = Palette {
    bg_editor: C::from_rgb(20, 15, 11),
    bg_panel: C::from_rgb(32, 25, 19),
    bg_overlay: C::from_rgb(38, 30, 23),
    bg_raised: C::from_rgb(46, 36, 27),
    bg_active: C::from_rgb(58, 45, 33),
    bg_input: C::from_rgb(24, 18, 13),
    accent: C::from_rgb(245, 140, 50),
    accent_hi: C::from_rgb(255, 168, 88),
    accent_row: C::from_rgba_premultiplied(50, 26, 8, 40),
    accent_wash: C::from_rgba_premultiplied(60, 32, 10, 50),
    accent_selection: C::from_rgba_premultiplied(70, 38, 12, 66),
    text: C::from_rgb(245, 236, 224),
    text_muted: C::from_rgb(200, 184, 164),
    text_faint: C::from_rgb(150, 134, 116),
    amber: C::from_rgb(232, 180, 90),
    moss: C::from_rgb(178, 196, 130),
    plum: C::from_rgb(190, 140, 200),
    error: C::from_rgb(232, 96, 72),
    warn: C::from_rgb(240, 190, 80),
    hairline: C::from_rgba_premultiplied(255, 220, 170, 16),
    border: C::from_rgba_premultiplied(255, 220, 170, 34),
    indent_guide: C::from_rgba_premultiplied(255, 220, 170, 26),
    hover_wash: C::from_rgba_premultiplied(255, 200, 140, 16),
    hover_lift: C::from_rgba_premultiplied(255, 210, 150, 26),
};

static ACTIVE_THEME: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Set the active theme. Call before [`apply_ide_style`], which reads the palette to build
/// egui's Visuals.
pub fn set_theme(theme: Theme) {
    let v = match theme {
        Theme::Dark => 0,
        Theme::Light => 1,
        Theme::Midnight => 2,
        Theme::Amber => 3,
    };
    ACTIVE_THEME.store(v, std::sync::atomic::Ordering::Relaxed);
}

pub fn active_theme() -> Theme {
    match ACTIVE_THEME.load(std::sync::atomic::Ordering::Relaxed) {
        1 => Theme::Light,
        2 => Theme::Midnight,
        3 => Theme::Amber,
        _ => Theme::Dark,
    }
}

/// The active palette (lock-free).
#[inline]
pub fn palette() -> &'static Palette {
    match ACTIVE_THEME.load(std::sync::atomic::Ordering::Relaxed) {
        1 => &LIGHT,
        2 => &MIDNIGHT,
        3 => &AMBER_THEME,
        _ => &DARK,
    }
}

/// Whether the active theme is a LIGHT one (drives the editor crate's palette + egui's base
/// light/dark visuals). Only [`Theme::Light`] is light today.
#[inline]
pub fn is_light_theme() -> bool {
    ACTIVE_THEME.load(std::sync::atomic::Ordering::Relaxed) == 1
}

/// Runtime color accessors. Each is a `fn() -> Color32` reading the active [`Palette`], so the
/// whole UI re-themes the instant [`set_theme`] flips the atomic — every call site kept its
/// `colors::NAME()` spelling.
pub mod colors {
    use egui::Color32;

    macro_rules! field {
        ($name:ident, $field:ident) => {
            #[inline]
            pub fn $name() -> Color32 {
                super::palette().$field
            }
        };
    }

    field!(BG_EDITOR, bg_editor);
    field!(BG_PANEL, bg_panel);
    field!(BG_OVERLAY, bg_overlay);
    field!(BG_RAISED, bg_raised);
    field!(BG_ACTIVE, bg_active);
    field!(BG_INPUT, bg_input);
    field!(ACCENT, accent);
    field!(ACCENT_HI, accent_hi);
    field!(ACCENT_ROW, accent_row);
    field!(ACCENT_WASH, accent_wash);
    field!(ACCENT_SELECTION, accent_selection);
    field!(TEXT, text);
    field!(TEXT_MUTED, text_muted);
    field!(TEXT_FAINT, text_faint);
    field!(AMBER, amber);
    field!(MOSS, moss);
    field!(PLUM, plum);
    field!(ERROR, error);
    field!(WARN, warn);
    field!(HAIRLINE, hairline);
    field!(BORDER, border);
    field!(INDENT_GUIDE, indent_guide);
    field!(HOVER_WASH, hover_wash);
    field!(HOVER_LIFT, hover_lift);
}

// =================================================================================================
// sizes — the metric system. 4px base grid; panel heights land on the 8px rhythm where possible.
// =================================================================================================
pub mod sizes {
    /// Menu bar strip height.
    pub const MENU_BAR_H: f32 = 28.0;
    /// Editor tab strip height (tabs fill it completely — flat, no gaps).
    pub const TAB_H: f32 = 32.0;
    /// Active-tab accent underline thickness.
    pub const TAB_UNDERLINE: f32 = 2.0;
    /// Project-tree row height.
    pub const TREE_ROW_H: f32 = 20.0;
    /// Status bar height.
    pub const STATUS_H: f32 = 24.0;
    /// Bottom tool-window switcher strip height.
    pub const TOOLBAR_H: f32 = 26.0;
    /// Tool-button height inside the switcher strip.
    pub const TOOL_BTN_H: f32 = 20.0;
    /// Scrollbar width.
    pub const SCROLLBAR_W: f32 = 6.0;
    /// Overlay (quick-open / open-project) corner radius.
    pub const OVERLAY_ROUNDING: f32 = 8.0;
    /// Overlay inner padding.
    pub const OVERLAY_PAD: f32 = 12.0;

    // ---- type scale ----
    pub const FONT_MENU: f32 = 13.0;
    pub const FONT_TREE: f32 = 13.0;
    pub const FONT_TAB: f32 = 13.0;
    pub const FONT_EDITOR: f32 = 14.0; // monospace — the editor sets this itself
    pub const FONT_STATUS: f32 = 12.0;
    pub const FONT_PANEL_HEADER: f32 = 11.0;
}

// =================================================================================================
// apply_ide_style — call AFTER livewall_uikit::theme::apply. Overrides only what the IDE needs;
// everything untouched stays Rune.
// =================================================================================================
pub fn apply_ide_style(ctx: &egui::Context) {
    use egui::{FontFamily, TextStyle};

    let mut style = (*ctx.style()).clone();

    // ---- typography: one notch denser than the uikit default (IDE, not settings app) ----------
    style.text_styles = [
        (TextStyle::Heading, FontId::new(17.0, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(sizes::FONT_MENU, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(sizes::FONT_MENU, FontFamily::Proportional)),
        (TextStyle::Small, FontId::new(sizes::FONT_PANEL_HEADER, FontFamily::Proportional)),
        // Monospace stays 13 for OUTPUT/Problems; the editor drives its own 14px FontId.
        (TextStyle::Monospace, FontId::new(13.0, FontFamily::Monospace)),
    ]
    .into();

    // ---- spacing: tight vertical rhythm, 8px horizontal grid ----------------------------------
    let s = &mut style.spacing;
    s.item_spacing = egui::vec2(8.0, 4.0);
    s.button_padding = egui::vec2(10.0, 4.0);
    s.interact_size.y = 24.0;
    s.menu_margin = egui::Margin::same(6.0);
    s.window_margin = egui::Margin::same(sizes::OVERLAY_PAD);
    s.indent = 16.0;

    // ---- scrollbars: thin floating rails, dim until touched -----------------------------------
    s.scroll = egui::style::ScrollStyle {
        floating: true,
        bar_width: sizes::SCROLLBAR_W,
        floating_width: sizes::SCROLLBAR_W,
        floating_allocated_width: 0.0,
        handle_min_length: 24.0,
        foreground_color: false, // handle color = widgets.*.bg_fill (set below)
        dormant_background_opacity: 0.0,
        active_background_opacity: 0.2,
        interact_background_opacity: 0.35,
        dormant_handle_opacity: 0.4,
        active_handle_opacity: 0.7,
        interact_handle_opacity: 1.0,
        ..Default::default()
    };

    // ---- visuals ------------------------------------------------------------------------------
    let v = &mut style.visuals;
    v.panel_fill = colors::BG_PANEL();
    // Frame::popup / menus / windows pick these up → overlays restyle for free.
    v.window_fill = colors::BG_OVERLAY();
    v.window_stroke = Stroke::new(1.0, colors::BORDER());
    v.window_rounding = Rounding::same(sizes::OVERLAY_ROUNDING);
    v.menu_rounding = Rounding::same(sizes::OVERLAY_ROUNDING);
    v.faint_bg_color = colors::BG_RAISED();
    v.extreme_bg_color = colors::BG_INPUT();
    v.code_bg_color = colors::BG_EDITOR();
    v.override_text_color = Some(colors::TEXT());
    v.warn_fg_color = colors::WARN();
    v.error_fg_color = colors::ERROR();
    v.hyperlink_color = colors::ACCENT_HI();
    v.striped = false;
    v.indent_has_left_vline = false; // the tree paints its own guides (colors::INDENT_GUIDE())

    // Crisper corners than the uikit default (7): IDE chrome rounds at 4.
    let r = Rounding::same(4.0);
    v.widgets.noninteractive.rounding = r;
    v.widgets.inactive.rounding = r;
    v.widgets.hovered.rounding = r;
    v.widgets.active.rounding = r;
    v.widgets.open.rounding = r;

    // Borderless-until-hover: buttons/selectables are INVISIBLE at rest, lift by fill on hover,
    // deepen when pressed. No strokes anywhere; no expansion jitter.
    v.widgets.noninteractive.bg_fill = colors::BG_PANEL();
    v.widgets.noninteractive.weak_bg_fill = colors::BG_PANEL();
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, colors::HAIRLINE()); // ui.separator()
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, colors::TEXT_MUTED());

    v.widgets.inactive.bg_fill = Color32::from_rgb(64, 62, 72); // scroll handle @ rest (dimmed by opacity)
    v.widgets.inactive.weak_bg_fill = Color32::TRANSPARENT; // flat buttons — the JetBrains move
    v.widgets.inactive.bg_stroke = Stroke::NONE;
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, colors::TEXT_MUTED());

    v.widgets.hovered.bg_fill = Color32::from_rgb(84, 82, 94); // scroll handle hover
    v.widgets.hovered.weak_bg_fill = colors::BG_RAISED();
    v.widgets.hovered.bg_stroke = Stroke::NONE; // kill the uikit orange hover ring in the IDE
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, colors::TEXT());
    v.widgets.hovered.expansion = 0.0;

    v.widgets.active.bg_fill = Color32::from_rgb(96, 93, 107); // scroll handle drag
    v.widgets.active.weak_bg_fill = colors::BG_ACTIVE(); // pressed = deeper fill, NOT orange
    v.widgets.active.bg_stroke = Stroke::NONE;
    v.widgets.active.fg_stroke = Stroke::new(1.0, Color32::WHITE);
    v.widgets.active.expansion = 0.0;

    v.widgets.open.bg_fill = colors::BG_ACTIVE(); // an open menu button stays lifted
    v.widgets.open.weak_bg_fill = colors::BG_ACTIVE();
    v.widgets.open.bg_stroke = Stroke::NONE;
    v.widgets.open.fg_stroke = Stroke::new(1.0, colors::TEXT());

    // Selection: one orange, everywhere. Rows read as a tint, text stays legible under it.
    v.selection.bg_fill = colors::ACCENT_SELECTION();
    v.selection.stroke = Stroke::new(1.0, colors::ACCENT_HI());
    v.text_cursor.stroke = Stroke::new(2.0, colors::ACCENT());

    // Depth comes from shadow + fill step, never heavy borders.
    v.popup_shadow = egui::Shadow {
        offset: egui::vec2(0.0, 6.0),
        blur: 18.0,
        spread: 0.0,
        color: Color32::from_black_alpha(110),
    };
    v.window_shadow = egui::Shadow {
        offset: egui::vec2(0.0, 8.0),
        blur: 24.0,
        spread: 0.0,
        color: Color32::from_black_alpha(120),
    };

    ctx.set_style(style);
}

// =================================================================================================
// helpers — the widgets the spec standardizes. App code uses these instead of ad-hoc labels.
// =================================================================================================

/// The 11px UPPERCASE tracking-wide panel header — PROBLEMS / OUTPUT / TERMINAL / project name.
/// One look for every tool-window title.
pub fn panel_header(ui: &mut egui::Ui, label: &str) {
    let mut job = LayoutJob::default();
    job.append(
        &label.to_uppercase(),
        0.0,
        TextFormat {
            font_id: FontId::proportional(sizes::FONT_PANEL_HEADER),
            color: colors::TEXT_FAINT(),
            extra_letter_spacing: 1.2,
            ..Default::default()
        },
    );
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.add_space(8.0);
        ui.label(job);
    });
    ui.add_space(4.0);
}

/// Inline variant of [`panel_header`] for headers that share a row with buttons (OUTPUT strip,
/// TERMINAL strip): just the 11px caps label, no vertical padding of its own.
pub fn panel_header_inline(ui: &mut egui::Ui, label: &str) {
    let mut job = LayoutJob::default();
    job.append(
        &label.to_uppercase(),
        0.0,
        TextFormat {
            font_id: FontId::proportional(sizes::FONT_PANEL_HEADER),
            color: colors::TEXT_FAINT(),
            extra_letter_spacing: 1.2,
            ..Default::default()
        },
    );
    ui.label(job);
}

/// A full-width 1px hairline (use instead of `ui.separator()` when you need edge-to-edge).
pub fn hairline(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, colors::HAIRLINE());
}

/// Tool-window toggle button for the bottom switcher strip: flat text at rest, hover wash,
/// active = orange-tinted fill + bright label. Returns the click response.
pub fn tool_button(ui: &mut egui::Ui, label: &str, active: bool) -> egui::Response {
    let font = FontId::proportional(12.0);
    let galley = ui.fonts(|f| f.layout_no_wrap(label.to_owned(), font, Color32::PLACEHOLDER));
    let gsize = galley.size();
    let desired = Vec2::new(gsize.x + 20.0, sizes::TOOL_BTN_H);
    let (rect, resp) = ui.allocate_exact_size(desired, Sense::click());
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        let rounding = Rounding::same(4.0);
        if active {
            p.rect_filled(rect, rounding, colors::ACCENT_WASH());
        } else if resp.hovered() {
            p.rect_filled(rect, rounding, colors::HOVER_WASH());
        }
        let color = if active || resp.hovered() { colors::TEXT() } else { colors::TEXT_MUTED() };
        p.galley(
            Pos2::new(rect.min.x + 10.0, rect.center().y - gsize.y * 0.5),
            galley,
            color,
        );
    }
    resp
}

/// What [`tab`] reports back to the app.
pub struct TabResponse {
    /// The tab body was clicked — activate this file.
    pub clicked: bool,
    /// The ✕ was clicked (or the tab middle-clicked) — close this file.
    pub closed: bool,
    /// The tab body's response (context menus, hover queries).
    pub response: egui::Response,
}

/// One JetBrains-New-UI editor tab. Flat (zero rounding); the active tab takes the EDITOR fill so
/// it visually merges with the editor below, plus a 2px orange underline pinned to the strip's
/// bottom edge. Inactive tabs are transparent over the panel with dim text; hover lifts the fill.
/// The right-hand 16px slot shows the amber dirty dot at rest and swaps to ✕ on hover.
///
/// Call inside a `TopBottomPanel` of `exact_height(sizes::TAB_H)` with a NO-margin frame
/// (`egui::Frame::none().fill(colors::BG_PANEL())`) so tabs bleed to both strip edges.
pub fn tab(ui: &mut egui::Ui, label: &str, active: bool, dirty: bool) -> TabResponse {
    let font = FontId::proportional(sizes::FONT_TAB);
    let galley = ui.fonts(|f| f.layout_no_wrap(label.to_owned(), font, Color32::PLACEHOLDER));
    let gsize = galley.size();
    const PAD_L: f32 = 12.0;
    const SLOT_W: f32 = 16.0;
    let w = PAD_L + gsize.x + 6.0 + SLOT_W + 6.0;
    let h = ui.available_height().max(sizes::TAB_H - 2.0);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, h), Sense::click_and_drag());
    let mut closed = false;

    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        // 1) fill — flat, square. Active merges with the editor; hover lifts; rest is invisible.
        if active {
            p.rect_filled(rect, 0.0, colors::BG_EDITOR());
        } else if resp.hovered() {
            p.rect_filled(rect, 0.0, colors::BG_RAISED());
        }

        // 2) the close/dirty slot on the right.
        let slot = Rect::from_center_size(
            Pos2::new(rect.right() - 6.0 - SLOT_W * 0.5, rect.center().y),
            Vec2::splat(SLOT_W),
        );
        let slot_resp = ui.interact(slot, resp.id.with("close"), Sense::click());
        let show_close = resp.hovered() || slot_resp.hovered() || (active && !dirty);
        if show_close {
            if slot_resp.hovered() {
                p.rect_filled(slot.shrink(1.0), Rounding::same(3.0), colors::HOVER_WASH());
            }
            let c = if slot_resp.hovered() { colors::TEXT() } else { colors::TEXT_FAINT() };
            let m = slot.center();
            let s2 = 3.5;
            let st = Stroke::new(1.2, c);
            p.line_segment([m + egui::vec2(-s2, -s2), m + egui::vec2(s2, s2)], st);
            p.line_segment([m + egui::vec2(s2, -s2), m + egui::vec2(-s2, s2)], st);
        } else if dirty {
            p.circle_filled(slot.center(), 3.0, colors::AMBER());
        }

        // 3) label — bone when active, muted otherwise.
        let tcol = if active { colors::TEXT() } else { colors::TEXT_MUTED() };
        p.galley(
            Pos2::new(rect.left() + PAD_L, rect.center().y - gsize.y * 0.5),
            galley,
            tcol,
        );

        // 4) separator: a 1px hairline on the right edge so adjacent tabs read as distinct.
        p.rect_filled(
            Rect::from_min_max(
                Pos2::new(rect.right() - 1.0, rect.top() + 7.0),
                Pos2::new(rect.right(), rect.bottom() - 7.0),
            ),
            0.0,
            colors::HAIRLINE(),
        );

        // 5) the accent underline — the single loudest element in the chrome.
        if active {
            p.rect_filled(
                Rect::from_min_max(
                    Pos2::new(rect.left(), rect.bottom() - sizes::TAB_UNDERLINE),
                    rect.right_bottom(),
                ),
                0.0,
                colors::ACCENT(),
            );
        }

        if slot_resp.clicked_by(egui::PointerButton::Primary) {
            closed = true;
        }
    }

    let closed = closed || resp.middle_clicked();
    TabResponse { clicked: resp.clicked_by(egui::PointerButton::Primary) && !closed, closed, response: resp }
}

/// The little "+" that rides at the end of a tab row — a tab-shaped stub itself: flat at rest,
/// hover lift, hairline right edge, same height as its siblings. It moves with the tabs.
pub fn tab_add_button(ui: &mut egui::Ui) -> egui::Response {
    let h = ui.available_height().max(sizes::TAB_H - 2.0);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(28.0, h), Sense::click());
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        if resp.hovered() {
            p.rect_filled(rect, 0.0, colors::BG_RAISED());
        }
        let c = rect.center();
        let col = if resp.hovered() { colors::TEXT() } else { colors::TEXT_FAINT() };
        let st = Stroke::new(1.6, col);
        p.line_segment([c + egui::vec2(-4.5, 0.0), c + egui::vec2(4.5, 0.0)], st);
        p.line_segment([c + egui::vec2(0.0, -4.5), c + egui::vec2(0.0, 4.5)], st);
        p.rect_filled(
            Rect::from_min_max(
                Pos2::new(rect.right() - 1.0, rect.top() + 7.0),
                Pos2::new(rect.right(), rect.bottom() - 7.0),
            ),
            0.0,
            colors::HAIRLINE(),
        );
    }
    resp
}

/// A status-bar count chip: `status_chip(ui, "3 ✕", colors::ERROR())`. Pill fill at 16% of the
/// status color, label in the full color. Returns the response (click → open Problems).
pub fn status_chip(ui: &mut egui::Ui, text: &str, color: Color32) -> egui::Response {
    let font = FontId::proportional(sizes::FONT_STATUS);
    let galley = ui.fonts(|f| f.layout_no_wrap(text.to_owned(), font, Color32::PLACEHOLDER));
    let gsize = galley.size();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(gsize.x + 14.0, 16.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        p.rect_filled(rect, Rounding::same(8.0), color.gamma_multiply(0.16));
        p.galley(
            Pos2::new(rect.center().x - gsize.x * 0.5, rect.center().y - gsize.y * 0.5),
            galley,
            color,
        );
    }
    resp
}
