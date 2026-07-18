//! Shared look-and-feel for Rune's native egui apps (redit, rmon, rpower, rark, lull).
//!
//! One place defines the "Rune" identity — a warm charcoal + burnt-orange autumn theme with modern
//! rounding, spacing, shadows, and typography — plus the decorationless window chrome (title bar with
//! minimize / maximize / close), since Rune advertises no server-side decorations. Apps call
//! [`theme::apply`] once per frame and [`chrome::title_bar`] as their top panel.

pub mod theme {
    use egui::{Color32, FontFamily, FontId, Margin, Rounding, Shadow, Stroke, TextStyle};

    /// Burnt orange — the accent (selection, active widgets, focus). Vivid, modern pumpkin.
    pub const ORANGE: Color32 = Color32::from_rgb(233, 110, 44);
    /// Brighter orange for hover strokes / links.
    pub const ORANGE_HI: Color32 = Color32::from_rgb(248, 140, 74);
    /// Clean warm off-white body text.
    pub const TEXT: Color32 = Color32::from_rgb(238, 235, 232);
    /// A translucent orange for row/selection highlights painted over content.
    pub const SEL_HL: Color32 = Color32::from_rgba_premultiplied(132, 60, 26, 96);
    /// Chrome color — the title bar + the compositor's window "container" frame (kept in sync with
    /// the hardcoded value in crates/rune-compositor/src/udev.rs).
    pub const CHROME: Color32 = Color32::from_rgb(19, 18, 22);

    /// Apply the Rune theme to a context. Call once per frame (cheap; idempotent).
    pub fn apply(ctx: &egui::Context) {
        let mut style = (*ctx.style()).clone();

        // ---- typography: a clear modern scale ----
        style.text_styles = [
            (TextStyle::Heading, FontId::new(19.0, FontFamily::Proportional)),
            (TextStyle::Body, FontId::new(14.0, FontFamily::Proportional)),
            (TextStyle::Button, FontId::new(14.0, FontFamily::Proportional)),
            (TextStyle::Small, FontId::new(11.5, FontFamily::Proportional)),
            (TextStyle::Monospace, FontId::new(13.0, FontFamily::Monospace)),
        ]
        .into();

        // ---- spacing: generous, modern ----
        let s = &mut style.spacing;
        s.item_spacing = egui::vec2(8.0, 8.0);
        s.button_padding = egui::vec2(11.0, 6.0);
        s.interact_size.y = 28.0;
        s.menu_margin = Margin::same(6.0);
        s.window_margin = Margin::same(10.0);
        s.indent = 18.0;
        s.scroll.bar_width = 9.0;
        s.scroll.floating = true;

        // ---- visuals: clean neutral-dark + vivid orange, soft corners, subtle depth ----
        // (Cleaner near-neutral darks — not the old muddy warm-brown that read "dated".)
        let v = &mut style.visuals;
        v.dark_mode = true;
        v.override_text_color = Some(TEXT);
        v.panel_fill = Color32::from_rgb(28, 27, 31);
        v.window_fill = Color32::from_rgb(24, 23, 27);
        v.faint_bg_color = Color32::from_rgb(38, 37, 43);
        v.extreme_bg_color = Color32::from_rgb(17, 16, 20);
        v.window_stroke = Stroke::new(1.0, Color32::from_rgb(54, 52, 60));

        let r = Rounding::same(7.0);
        v.widgets.noninteractive.rounding = r;
        v.widgets.inactive.rounding = r;
        v.widgets.hovered.rounding = r;
        v.widgets.active.rounding = r;
        v.widgets.open.rounding = r;
        v.window_rounding = Rounding::same(11.0);
        v.menu_rounding = Rounding::same(8.0);

        // flat widgets that lift subtly on hover, fill orange when active
        v.widgets.noninteractive.bg_fill = Color32::from_rgb(28, 27, 31);
        v.widgets.noninteractive.weak_bg_fill = Color32::from_rgb(28, 27, 31);
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(50, 48, 56));
        v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);

        v.widgets.inactive.bg_fill = Color32::from_rgb(44, 43, 50);
        v.widgets.inactive.weak_bg_fill = Color32::from_rgb(44, 43, 50);
        v.widgets.inactive.bg_stroke = Stroke::NONE;
        v.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);

        v.widgets.hovered.bg_fill = Color32::from_rgb(58, 56, 65);
        v.widgets.hovered.weak_bg_fill = Color32::from_rgb(58, 56, 65);
        v.widgets.hovered.bg_stroke = Stroke::new(1.0, ORANGE_HI);
        v.widgets.hovered.fg_stroke = Stroke::new(1.0, Color32::WHITE);
        v.widgets.hovered.expansion = 1.0;

        v.widgets.active.bg_fill = ORANGE;
        v.widgets.active.weak_bg_fill = ORANGE;
        v.widgets.active.bg_stroke = Stroke::new(1.0, ORANGE_HI);
        v.widgets.active.fg_stroke = Stroke::new(1.0, Color32::WHITE);

        v.widgets.open.bg_fill = Color32::from_rgb(58, 56, 65);
        v.widgets.open.weak_bg_fill = Color32::from_rgb(58, 56, 65);

        v.selection.bg_fill = ORANGE.gamma_multiply(0.42);
        v.selection.stroke = Stroke::new(1.0, ORANGE_HI);
        v.hyperlink_color = ORANGE_HI;

        v.popup_shadow = Shadow {
            offset: egui::vec2(0.0, 4.0),
            blur: 16.0,
            spread: 0.0,
            color: Color32::from_black_alpha(96),
        };
        v.window_shadow = Shadow {
            offset: egui::vec2(0.0, 9.0),
            blur: 26.0,
            spread: 0.0,
            color: Color32::from_black_alpha(115),
        };

        ctx.set_style(style);
    }
}

pub mod chrome {
    use super::theme;
    use egui::{Color32, Pos2, Rect, Stroke, Vec2};

    // Autumn traffic-light colors (the macOS 3-dot idea in fall tones).
    const MIN: Color32 = Color32::from_rgb(217, 164, 65); // golden amber
    const MAX: Color32 = Color32::from_rgb(150, 110, 184); // plum purple
    const CLOSE: Color32 = Color32::from_rgb(197, 82, 46); // burnt rust

    #[derive(Clone, Copy)]
    enum Light {
        Close,
        Min,
        Max,
    }

    /// The Rune title bar: a Plasma-style header strip (flat, subtle bottom separator, centered
    /// title) with macOS traffic-light dots on the left for close / minimize / maximize. For a
    /// decorationless (`with_decorations(false)`) window under Rune. Render as the top-most panel.
    pub fn title_bar(ctx: &egui::Context, title: &str) {
        let mut cmd: Option<egui::ViewportCommand> = None;
        egui::TopBottomPanel::top("rune-titlebar")
            .exact_height(HEIGHT)
            .frame(egui::Frame::none().fill(theme::CHROME))
            .show(ctx, |ui| {
                cmd = bar_ui(ui, title, 1.0);
            });
        if let Some(c) = cmd {
            ctx.send_viewport_cmd(c);
        }
    }

    /// The same bar, but chromeless: it floats OVER the content instead of taking a strip of
    /// layout, and it is invisible until the pointer comes near the top edge. For full-screen
    /// views that want the window to be nothing but content — the close/min/max dots and the
    /// drag region are still there the moment you reach for them.
    ///
    /// Draw it AFTER the content it floats over (an `Area` is painted in call order).
    pub fn title_bar_overlay(ctx: &egui::Context, title: &str) {
        // Reveal when the pointer is within the bar itself plus a little approach margin, so the
        // bar is already there by the time you arrive at the dots. No pointer (touch, or focus
        // elsewhere) = hidden.
        let near_top = ctx.input(|i| {
            i.pointer
                .latest_pos()
                .is_some_and(|p| p.y <= HEIGHT + REVEAL_MARGIN)
        });
        // ~120ms fade, so it arrives/leaves as a soft wipe rather than a pop.
        let alpha = ctx.animate_bool_with_time(egui::Id::new("rune-titlebar-reveal"), near_top, 0.12);
        if alpha <= 0.001 {
            return; // fully hidden: don't paint it, and don't let it eat clicks on the content.
        }

        let mut cmd: Option<egui::ViewportCommand> = None;
        egui::Area::new(egui::Id::new("rune-titlebar-overlay"))
            .fixed_pos(ctx.screen_rect().min)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                let w = ctx.screen_rect().width();
                let (rect, _) = ui.allocate_exact_size(egui::vec2(w, HEIGHT), egui::Sense::hover());
                // Scrim rather than the opaque CHROME fill — over album art a solid strip reads as
                // a bar that failed to leave, while a wash reads as part of the image.
                ui.painter()
                    .rect_filled(rect, 0.0, Color32::from_black_alpha(150).gamma_multiply(alpha));
                let mut ui = ui.child_ui(rect, egui::Layout::left_to_right(egui::Align::Center), None);
                cmd = bar_ui(&mut ui, title, alpha);
            });
        if let Some(c) = cmd {
            ctx.send_viewport_cmd(c);
        }
    }

    const HEIGHT: f32 = 36.0;
    /// How far above/below the bar the pointer counts as "reaching for" it.
    const REVEAL_MARGIN: f32 = 28.0;

    /// The bar's contents, drawn into whatever `ui` it's given (a panel, or a floating overlay).
    /// `alpha` fades every mark it makes, so the overlay can wipe the whole thing in and out.
    /// Returns the viewport command the click asked for, if any.
    fn bar_ui(ui: &mut egui::Ui, title: &str, alpha: f32) -> Option<egui::ViewportCommand> {
        let maximized = ui.input(|i| i.viewport().maximized).unwrap_or(false);
        let mut cmd: Option<egui::ViewportCommand> = None;
        {
                let full = ui.max_rect();
                let cy = full.center().y;

                // Autumn dots on the TOP-RIGHT (KDE/Windows placement), left→right order
                // minimize, maximize, close — so close is in the far corner.
                let r = 6.5;
                let gap = 20.0;
                let close_c = egui::pos2(full.right() - 18.0, cy);
                let max_c = egui::pos2(close_c.x - gap, cy);
                let min_c = egui::pos2(max_c.x - gap, cy);
                // reveal the glyphs when the pointer is over the light cluster.
                let cluster = Rect::from_min_max(
                    egui::pos2(min_c.x - r - 3.0, full.top()),
                    egui::pos2(close_c.x + r + 3.0, full.bottom()),
                );
                let show_glyphs = ui.rect_contains_pointer(cluster);

                if light(ui, close_c, r, CLOSE, Light::Close, show_glyphs, alpha) {
                    cmd = Some(egui::ViewportCommand::Close);
                }
                if light(ui, min_c, r, MIN, Light::Min, show_glyphs, alpha) {
                    cmd = Some(egui::ViewportCommand::Minimized(true));
                }
                if light(ui, max_c, r, MAX, Light::Max, show_glyphs, alpha) {
                    cmd = Some(egui::ViewportCommand::Maximized(!maximized));
                }

                // The rest of the bar (left of the dots) drags the window. Hand the drag to the
                // compositor on the PRESS itself — NOT egui's drag_started(), which waits for the
                // ~6px is_decidedly_dragging threshold (plus a frame of latency) before the xdg
                // move goes out, and rode winit's per-window pointers list (empty = StartDrag
                // silently dropped — the "fresh window won't drag" bug). Rune's SSD titlebars
                // grab on the press with zero threshold; this matches them exactly. A quick
                // click still just focuses: the compositor's move grab ends on release having
                // moved nothing. Double-click zoom CANNOT live here anymore (the move grab eats
                // the release, so egui never completes a click on the bar) — the compositor
                // detects the paired move_requests and toggles maximize (see rune-compositor
                // xdg_shell.rs move_request).
                let drag_rect = Rect::from_min_max(full.min, egui::pos2(min_c.x - r - 8.0, full.bottom()));
                let drag = ui.interact(drag_rect, ui.id().with("rune-title-drag"), egui::Sense::click_and_drag());
                if drag.is_pointer_button_down_on() && ui.input(|i| i.pointer.primary_pressed()) {
                    // Rare (once per titlebar press), goes to the app's own log file.
                    eprintln!("[uikit] StartDrag sent (titlebar press)");
                    cmd = Some(egui::ViewportCommand::StartDrag);
                }

                // centered title (muted, like a Plasma header)
                ui.painter().text(
                    egui::pos2(full.center().x, cy),
                    egui::Align2::CENTER_CENTER,
                    title,
                    egui::FontId::proportional(13.5),
                    theme::TEXT.gamma_multiply(0.82 * alpha),
                );

                // subtle Plasma-style separator under the bar
                ui.painter().line_segment(
                    [full.left_bottom(), full.right_bottom()],
                    Stroke::new(1.0, Color32::from_rgb(48, 46, 54).gamma_multiply(alpha)),
                );
        }
        cmd
    }

    /// One traffic-light dot: filled circle + definition ring; when the cluster is hovered it shows a
    /// crisp dark glyph (× / − / +). Dims slightly when the window is unfocused. Returns clicked.
    fn light(
        ui: &mut egui::Ui,
        center: Pos2,
        r: f32,
        color: Color32,
        kind: Light,
        show_glyph: bool,
        alpha: f32,
    ) -> bool {
        let hit = Rect::from_center_size(center, Vec2::splat(r * 2.0 + 4.0));
        let id = ui.id().with(match kind {
            Light::Close => "tl-close",
            Light::Min => "tl-min",
            Light::Max => "tl-max",
        });
        let resp = ui.interact(hit, id, egui::Sense::click());
        let focused = ui.input(|i| i.viewport().focused).unwrap_or(true);
        let fill = if !focused {
            Color32::from_gray(90) // unfocused: macOS greys the lights out
        } else if resp.hovered() {
            color.gamma_multiply(1.12)
        } else {
            color
        };
        let p = ui.painter();
        p.circle_filled(center, r, fill.gamma_multiply(alpha));
        p.circle_stroke(center, r, Stroke::new(1.0, color.gamma_multiply(0.55 * alpha)));
        if show_glyph && focused {
            let g = Color32::from_black_alpha(165).gamma_multiply(alpha);
            let s = r * 0.52;
            let st = Stroke::new(1.4, g);
            match kind {
                Light::Close => {
                    p.line_segment([center + Vec2::new(-s, -s), center + Vec2::new(s, s)], st);
                    p.line_segment([center + Vec2::new(s, -s), center + Vec2::new(-s, s)], st);
                }
                Light::Min => {
                    p.line_segment([center + Vec2::new(-s, 0.0), center + Vec2::new(s, 0.0)], st);
                }
                Light::Max => {
                    p.line_segment([center + Vec2::new(-s, 0.0), center + Vec2::new(s, 0.0)], st);
                    p.line_segment([center + Vec2::new(0.0, -s), center + Vec2::new(0.0, s)], st);
                }
            }
        }
        resp.clicked()
    }
}
