//! app.rs — the eframe application: lay out, render and drive input for the terminal grid.
//!
//! Each frame the app pumps the active [`Session`] (draining PTY output into the grid and acting on
//! the grid's events), recomputes the cell grid from the pixel area (resizing the PTY when it
//! changes so TUIs reflow), paints every renderable cell with [`egui::Painter`], draws the cursor,
//! and translates egui key / wheel / mouse events into PTY bytes.
//!
//! Sessions live in a `Vec` with an `active` index; the always-visible tab strip and the Tabby-style
//! keybinds in [`handle_tab_keys`] manage them. All Alacritty access goes through
//! [`crate::term::Terminal`].

use livewall_uikit::{chrome, theme};

use cider::config::Config;
use cider::emoji::Emoji;
use cider::session::Session;
use cider::widget::{color32, handle_keys_and_wheel, handle_selection, paste_clipboard, read_clipboard, render, BLINK_MS};

const PAD: f32 = 4.0;

pub struct Rterm {
    cfg: Config,
    sessions: Vec<Session>,
    active: usize,
    /// True while a left-drag text selection is in progress.
    dragging_sel: bool,
    /// Set when the shell could not be spawned; shown instead of a grid.
    spawn_error: Option<String>,
    /// Colour-emoji rasteriser + texture cache (see [`crate::emoji`]).
    emoji: Emoji,
}

impl Rterm {
    pub fn new(cc: &eframe::CreationContext<'_>, cfg: Config) -> Self {
        let ctx = cc.egui_ctx.clone();
        // Bind the clipboard to eframe's Wayland display before any paste can happen.
        cider::clip::init(
            raw_window_handle::HasDisplayHandle::display_handle(cc).ok().map(|d| d.as_raw()),
        );
        let mut sessions = Vec::new();
        let mut spawn_error = None;
        // Spawn at a conventional 80×24; the first frame resizes to the real window immediately.
        match Session::spawn(80, 24, cfg.scrollback, &ctx, None) {
            Ok(s) => sessions.push(s),
            Err(e) => spawn_error = Some(format!("{e:#}")),
        }
        Self { cfg, sessions, active: 0, dragging_sel: false, spawn_error, emoji: Emoji::load_system() }
    }
}

impl eframe::App for Rterm {
    // Clear to the (optionally translucent) terminal background so rounded corners / transparency
    // show the right colour behind the grid.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        color32(self.cfg.palette.bg, self.cfg.opacity).to_normalized_gamma_f32()
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        theme::apply(ctx);

        // DIAGNOSTIC (temporary): if the compositor asked us to close, record it BEFORE eframe acts
        // on it — this is the one vanish cause that goes through neither close path below, so it's
        // the prime suspect for the "vanishes on a titlebar drag" reports.
        if ctx.input(|i| i.viewport().close_requested()) {
            cider::diag::note(
                "compositor sent CloseRequested (WM close / surface destroyed / interactive-move race)",
            );
        }

        // Debug: once the window actually holds keyboard focus (the clipboard is focus-gated, so a
        // read before that always fails), copy a canary and read it back — a full round-trip over
        // the same path Ctrl+C / Ctrl+V use. NOTE: this overwrites the real clipboard.
        if std::env::var_os("CIDER_CLIP_SELFTEST").is_some() && ctx.input(|i| i.focused) {
            static PROBED: std::sync::Once = std::sync::Once::new();
            PROBED.call_once(|| {
                cider::clip::write("CIDER-CANARY-42".to_owned());
                match read_clipboard() {
                    Some(t) => eprintln!("[cider] SELFTEST round-trip OK: {t:?}"),
                    None => eprintln!("[cider] SELFTEST round-trip FAILED"),
                }
            });
        }

        // Split borrows so the render/input closures can touch fields independently.
        let Self { cfg, sessions, active, dragging_sel, spawn_error, emoji } = self;

        if let Some(err) = spawn_error {
            chrome::title_bar(ctx, "cider");
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.label(egui::RichText::new(format!("Could not start a shell:\n{err}")).monospace());
                });
            });
            return;
        }

        // App-level tab keybinds (Tabby-style), consumed so they never reach the shell.
        handle_tab_keys(ctx, sessions, active, cfg);

        // Pump EVERY session each frame so background tabs keep making progress, then close any
        // whose shell has exited (`exit`/Ctrl+D). If the last one goes, close the window.
        for s in sessions.iter_mut() {
            s.pump(ctx);
        }
        reap_and_clamp(ctx, sessions, active);
        if sessions.is_empty() {
            return;
        }

        let title = sessions[*active].title.as_str();
        chrome::title_bar(ctx, &format!("cider  \u{2014}  {title}"));

        // Tab strip — always visible (Tabby-style) so tabs are discoverable: the "+" is the only
        // mouse affordance for a second shell, and hiding it left the keybinds unfindable.
        if let Some(action) = tab_bar(ctx, sessions, *active) {
            apply_tab_action(ctx, sessions, active, cfg, action);
            if sessions.is_empty() {
                return;
            }
        }

        // Monospace cell metrics for this frame.
        let font_id = egui::FontId::monospace(cfg.font_size);
        let (cw, ch) = ctx.fonts(|f| (f.glyph_width(&font_id, 'M'), f.row_height(&font_id)));
        let cell_w = cw.max(1.0);
        let cell_h = ch.max(1.0);

        let panel_fill = color32(cfg.palette.bg, cfg.opacity);
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(panel_fill).inner_margin(egui::Margin::same(PAD)))
            .show(ctx, |ui| {
                let area = ui.max_rect();
                // Clamp against a degenerate first-frame cell size (font not yet loaded) so we never
                // ask Alacritty for an absurd grid.
                let cols = ((area.width() / cell_w).floor() as usize).clamp(2, 1000);
                let rows = ((area.height() / cell_h).floor() as usize).clamp(1, 1000);

                // Input + resize mutate the session; render only reads it — keep the borrows disjoint.
                {
                    let s = &mut sessions[*active];
                    // Debounced: a raw resize-on-every-delta storms the shell with SIGWINCH while the
                    // fonts are still loading and floods scrollback with prompt fragments.
                    let now = ctx.input(|i| i.time);
                    if let Some(wait) = s.resize_settled(cols, rows, now) {
                        ctx.request_repaint_after(std::time::Duration::from_secs_f64(wait));
                    }
                    let response =
                        ui.interact(area, egui::Id::new("cider-grid"), egui::Sense::click_and_drag());
                    handle_selection(s, &response, area, cell_w, cell_h, cols, rows, dragging_sel, ctx);
                    handle_keys_and_wheel(s, ctx, area, cell_w, cell_h, cols, rows, *dragging_sel);

                    // Middle-click pastes (the X11 convention).
                    if response.middle_clicked() {
                        paste_clipboard(s);
                    }
                    // Right-click menu: the discoverable copy / paste / select-all controls.
                    response.context_menu(|ui| {
                        let has_sel = s.terminal.selection_text().is_some();
                        if ui.add_enabled(has_sel, egui::Button::new("Copy")).clicked() {
                            if let Some(sel) = s.terminal.selection_text() {
                                ui.ctx().copy_text(sel);
                                s.terminal.clear_selection();
                            }
                            ui.close_menu();
                        }
                        if ui.button("Paste").clicked() {
                            paste_clipboard(s);
                            ui.close_menu();
                        }
                        ui.separator();
                        if ui.button("Select All").clicked() {
                            s.terminal.select_all();
                            ui.close_menu();
                        }
                    });
                }
                render(ui, &sessions[*active], cfg, &font_id, area, cell_w, cell_h, cols, rows, ctx, emoji);
            });

        // Keep the cursor blinking while idle; PTY output drives its own repaints from the reader.
        ctx.request_repaint_after(std::time::Duration::from_millis(BLINK_MS));
    }
}

// --- tabs ----------------------------------------------------------------------------------------

/// What a tab-bar interaction or keybind asks the app to do.
enum TabAction {
    Switch(usize),
    Close(usize),
    New,
}

/// Tabby-style tab keybinds, consumed via `consume_key` so they never reach the shell:
/// Ctrl+Shift+T new, Ctrl+Shift+W close, Ctrl+Tab / Ctrl+Shift+Tab cycle, Ctrl+1..9 jump.
fn handle_tab_keys(
    ctx: &egui::Context,
    sessions: &mut Vec<Session>,
    active: &mut usize,
    cfg: &Config,
) {
    use egui::{Key, Modifiers};
    let cs = Modifiers::CTRL | Modifiers::SHIFT;
    if ctx.input_mut(|i| i.consume_key(cs, Key::T)) {
        spawn_tab(ctx, sessions, active, cfg);
    }
    if ctx.input_mut(|i| i.consume_key(cs, Key::W)) {
        close_at(ctx, sessions, active, *active);
    }
    let n = sessions.len();
    if n > 1 && ctx.input_mut(|i| i.consume_key(cs, Key::Tab)) {
        *active = (*active + n - 1) % n; // Ctrl+Shift+Tab → previous
    }
    if n > 1 && ctx.input_mut(|i| i.consume_key(Modifiers::CTRL, Key::Tab)) {
        *active = (*active + 1) % n; // Ctrl+Tab → next
    }
    for (idx, key) in [
        Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8,
        Key::Num9,
    ]
    .into_iter()
    .enumerate()
    {
        if ctx.input_mut(|i| i.consume_key(Modifiers::CTRL, key)) && idx < sessions.len() {
            *active = idx;
        }
    }
}

/// Open a new tab at the active tab's current grid size (so it starts correctly sized) and focus it.
fn spawn_tab(ctx: &egui::Context, sessions: &mut Vec<Session>, active: &mut usize, cfg: &Config) {
    let (cols, lines) = sessions
        .get(*active)
        .map(|s| (s.terminal.size.cols, s.terminal.size.lines))
        .unwrap_or((80, 24));
    match Session::spawn(cols.max(2), lines.max(1), cfg.scrollback, ctx, None) {
        Ok(s) => {
            sessions.push(s);
            *active = sessions.len() - 1;
        }
        Err(e) => log::warn!("cider: could not open a new tab: {e:#}"),
    }
}

/// Close tab `i` (its `Pty` drops → SIGHUP → shell exits); keep `active` pointing sensibly, and if
/// the last tab closed, close the window.
fn close_at(ctx: &egui::Context, sessions: &mut Vec<Session>, active: &mut usize, i: usize) {
    if i >= sessions.len() {
        return;
    }
    sessions.remove(i);
    if *active > i {
        *active -= 1;
    }
    if sessions.is_empty() {
        cider::diag::note("close_at: last tab closed (Ctrl+Shift+W or × click) → Close");
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    } else if *active >= sessions.len() {
        *active = sessions.len() - 1;
    }
}

/// Drop any tabs whose shell has exited, clamp `active`, and close the window if none remain.
fn reap_and_clamp(ctx: &egui::Context, sessions: &mut Vec<Session>, active: &mut usize) {
    let mut i = 0;
    while i < sessions.len() {
        if sessions[i].exited() {
            sessions.remove(i);
            if *active > i {
                *active -= 1;
            }
        } else {
            i += 1;
        }
    }
    if sessions.is_empty() {
        cider::diag::note("reap_and_clamp: all shells reported exited (EOF/error on every PTY) → Close");
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    } else if *active >= sessions.len() {
        *active = sessions.len() - 1;
    }
}

fn apply_tab_action(
    ctx: &egui::Context,
    sessions: &mut Vec<Session>,
    active: &mut usize,
    cfg: &Config,
    action: TabAction,
) {
    match action {
        TabAction::Switch(i) => {
            if i < sessions.len() {
                *active = i;
            }
        }
        TabAction::Close(i) => close_at(ctx, sessions, active, i),
        TabAction::New => spawn_tab(ctx, sessions, active, cfg),
    }
}

/// The tab strip (always visible): a chip per session with a per-tab close (×) and a trailing
/// "+" for a new tab. Painted in the uikit charcoal + burnt-orange palette. Returns the click action.
fn tab_bar(ctx: &egui::Context, sessions: &[Session], active: usize) -> Option<TabAction> {
    let mut action = None;
    egui::TopBottomPanel::top("cider-tabs")
        .frame(egui::Frame::none().fill(theme::CHROME).inner_margin(egui::Margin::symmetric(6.0, 4.0)))
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                for (i, s) in sessions.iter().enumerate() {
                    if let Some(a) = tab_chip(ui, i, &s.title, i == active) {
                        action = Some(a);
                    }
                }
                // trailing new-tab button
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(26.0, 26.0), egui::Sense::click());
                let fill = if resp.hovered() { theme::SEL_HL } else { egui::Color32::TRANSPARENT };
                ui.painter().rect_filled(rect, egui::Rounding::same(6.0), fill);
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "+",
                    egui::FontId::proportional(17.0),
                    theme::TEXT,
                );
                if resp.clicked() {
                    action = Some(TabAction::New);
                }
            });
        });
    action
}

/// One tab chip: `N title…` with an active accent + a right-edge × close hit-region.
fn tab_chip(ui: &mut egui::Ui, i: usize, title: &str, active: bool) -> Option<TabAction> {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(160.0, 26.0), egui::Sense::click());
    let fill = if active {
        theme::ORANGE.gamma_multiply(0.28)
    } else if resp.hovered() {
        theme::SEL_HL
    } else {
        egui::Color32::TRANSPARENT
    };
    let p = ui.painter();
    p.rect_filled(rect, egui::Rounding::same(6.0), fill);
    if active {
        // left accent bar
        p.rect_filled(
            egui::Rect::from_min_size(rect.left_top() + egui::vec2(0.0, 3.0), egui::vec2(2.5, rect.height() - 6.0)),
            egui::Rounding::same(1.0),
            theme::ORANGE,
        );
    }
    // close × hit region on the right
    let x_center = egui::pos2(rect.right() - 13.0, rect.center().y);
    let x_rect = egui::Rect::from_center_size(x_center, egui::vec2(18.0, 18.0));
    let x_hover = ui.rect_contains_pointer(x_rect);
    let text_col = if active { theme::TEXT } else { theme::TEXT.gamma_multiply(0.72) };
    // title (numbered, truncated to the space before the ×)
    let mut job = egui::text::LayoutJob::simple_singleline(
        format!("{}  {}", i + 1, title),
        egui::FontId::proportional(12.5),
        text_col,
    );
    job.wrap = egui::text::TextWrapping::truncate_at_width(x_rect.left() - rect.left() - 16.0);
    let galley = ui.painter().layout_job(job);
    ui.painter().galley(
        egui::pos2(rect.left() + 10.0, rect.center().y - galley.size().y / 2.0),
        galley,
        text_col,
    );
    ui.painter().text(
        x_center,
        egui::Align2::CENTER_CENTER,
        "\u{00d7}",
        egui::FontId::proportional(15.0),
        if x_hover { theme::ORANGE_HI } else { text_col },
    );
    if resp.clicked() {
        let on_x = resp.interact_pointer_pos().is_some_and(|pos| x_rect.contains(pos));
        return Some(if on_x { TabAction::Close(i) } else { TabAction::Switch(i) });
    }
    let _ = active;
    None
}
