//! widget.rs — the embeddable terminal widget: input translation + grid painting for one
//! [`Session`], shared by the standalone cider app and hosts that embed a terminal pane
//! (the Cauldron IDE). [`terminal_ui`] is the embed entry point: it pumps the session, sizes the
//! grid to the available rect, gates KEYBOARD input on egui focus (click the pane to focus — an
//! embedded terminal must not eat the host's keystrokes), and paints. The standalone app calls
//! the same internals with its window-wide layout.

use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{point_to_viewport, viewport_to_point, TermMode};
use alacritty_terminal::index::{Column, Point, Side};
use alacritty_terminal::vte::ansi::{CursorShape, Rgb};

use crate::config::Config;
use crate::emoji::{self, Emoji};
use crate::session::Session;

/// Cursor blink half-period (ms) — hosts use it as their idle repaint cadence too.
pub const BLINK_MS: u64 = 530;
/// Selection highlight: a muted slate that keeps most foreground colours readable.
const SEL_BG: egui::Color32 = egui::Color32::from_rgb(56, 66, 88);

/// Embed a terminal into any egui container. Pumps + resizes + paints `s` in the available rect;
/// keyboard goes to the shell ONLY while the pane has focus (click it to focus). Returns the
/// pane's Response so hosts can style focus/hover.
#[allow(clippy::too_many_arguments)]
pub fn terminal_ui(
    ui: &mut egui::Ui,
    s: &mut Session,
    cfg: &Config,
    emoji: &mut Emoji,
    dragging_sel: &mut bool,
) -> egui::Response {
    let ctx = ui.ctx().clone();
    s.pump(&ctx);

    let font_id = egui::FontId::monospace(cfg.font_size);
    let (cw, ch) = ctx.fonts(|f| (f.glyph_width(&font_id, 'M'), f.row_height(&font_id)));
    let cell_w = cw.max(1.0);
    let cell_h = ch.max(1.0);

    let area = ui.available_rect_before_wrap();
    ui.painter().rect_filled(area, egui::Rounding::ZERO, color32(cfg.palette.bg, 1.0));

    let cols = ((area.width() / cell_w).floor() as usize).clamp(2, 1000);
    let rows = ((area.height() / cell_h).floor() as usize).clamp(1, 1000);
    // Debounced: a raw resize-on-every-delta storms the shell with SIGWINCH (fonts still loading, a
    // pane being dragged) and floods scrollback with half-drawn prompts. See `Session::resize_settled`.
    let now = ctx.input(|i| i.time);
    if let Some(wait) = s.resize_settled(cols, rows, now) {
        ctx.request_repaint_after(std::time::Duration::from_secs_f64(wait));
    }

    let response = ui.interact(area, ui.id().with("cider-embed"), egui::Sense::click_and_drag());
    if response.clicked() || response.drag_started() {
        response.request_focus();
    }
    let focused = response.has_focus();

    // While the grid has focus, every key belongs to the SHELL — egui's built-in widget focus
    // traversal must not eat any of them. Without this lock, Tab moves focus to the next widget
    // instead of completing a path (you "tab out of the shell"), and the arrows/Escape that
    // `key_to_bytes` already forwards to the PTY get fought over by the same traversal.
    if focused {
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                response.id,
                egui::EventFilter {
                    tab: true,
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    escape: true,
                },
            )
        });
    }

    handle_selection(s, &response, area, cell_w, cell_h, cols, rows, dragging_sel, &ctx);
    if focused {
        handle_keys_and_wheel(s, &ctx, area, cell_w, cell_h, cols, rows, *dragging_sel);
    }
    if response.middle_clicked() {
        paste_clipboard(s);
    }
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

    render(ui, s, cfg, &font_id, area, cell_w, cell_h, cols, rows, &ctx, emoji);
    ui.allocate_rect(area, egui::Sense::hover());
    // Keep the cursor blinking while the pane is visible.
    ctx.request_repaint_after(std::time::Duration::from_millis(BLINK_MS));
    response
}

// --- input ---------------------------------------------------------------------------------------

/// Mouse text selection: left-drag selects, release copies (X11 primary-selection style), a plain
/// click clears.
#[allow(clippy::too_many_arguments)]
pub fn handle_selection(
    s: &mut Session,
    response: &egui::Response,
    area: egui::Rect,
    cell_w: f32,
    cell_h: f32,
    cols: usize,
    rows: usize,
    dragging: &mut bool,
    ctx: &egui::Context,
) {
    if response.drag_started() {
        if let Some(p) = response.interact_pointer_pos() {
            let (point, side) = grid_point(s, p, area, cell_w, cell_h, cols, rows);
            s.terminal.begin_selection(point, side);
            *dragging = true;
            ctx.request_repaint();
        }
    }
    if *dragging && response.dragged() {
        if let Some(p) = response.interact_pointer_pos() {
            // Dragging past the top/bottom edge auto-scrolls the view in that direction (speed
            // proportional to the overshoot, capped), extending the selection as it goes. The
            // repaint below keeps this repeating every frame while the button is held.
            let overshoot = if p.y < area.top() {
                area.top() - p.y // above → positive → scroll up into history
            } else if p.y > area.bottom() {
                -(p.y - area.bottom()) // below → negative → scroll down toward the prompt
            } else {
                0.0
            };
            if overshoot != 0.0 {
                let lines = (1 + (overshoot.abs() / cell_h) as i32).min(5);
                s.terminal.scroll_delta(if overshoot > 0.0 { lines } else { -lines });
            }
            // Compute the grid point AFTER any scroll, so the selection lands on the cell now
            // under the pointer.
            let (point, side) = grid_point(s, p, area, cell_w, cell_h, cols, rows);
            s.terminal.update_selection(point, side);
            ctx.request_repaint();
        }
    }
    if response.drag_stopped() {
        *dragging = false;
        if let Some(text) = s.terminal.selection_text() {
            ctx.copy_text(text);
        }
    }
    if response.clicked() {
        s.terminal.clear_selection();
    }
}

/// Pixel position → (grid point, side of the cell the pointer is on).
fn grid_point(
    s: &Session,
    pos: egui::Pos2,
    area: egui::Rect,
    cell_w: f32,
    cell_h: f32,
    cols: usize,
    rows: usize,
) -> (Point, Side) {
    let relx = (pos.x - area.left()).max(0.0);
    let rely = (pos.y - area.top()).max(0.0);
    let col = ((relx / cell_w) as usize).min(cols - 1);
    let line = ((rely / cell_h) as usize).min(rows - 1);
    let point = viewport_to_point(s.terminal.display_offset(), Point::new(line, Column(col)));
    let side = if relx - col as f32 * cell_w < cell_w * 0.5 { Side::Left } else { Side::Right };
    (point, side)
}

/// Translate this frame's keyboard + wheel events into PTY bytes (and scrollback moves).
/// `cols`/`rows` and `dragging` let a wheel scroll mid-drag extend the active selection to the
/// cell under the pointer after the view moves.
#[allow(clippy::too_many_arguments)]
pub fn handle_keys_and_wheel(
    s: &mut Session,
    ctx: &egui::Context,
    area: egui::Rect,
    cell_w: f32,
    cell_h: f32,
    cols: usize,
    rows: usize,
    dragging: bool,
) {
    let (events, mods, hover) =
        ctx.input(|i| (i.events.clone(), i.modifiers, i.pointer.hover_pos()));
    let over_grid = hover.is_some_and(|p| area.contains(p));
    let mode = s.terminal.mode();

    let debug_clip = std::env::var_os("CIDER_CLIP_DEBUG").is_some();
    let mut out: Vec<u8> = Vec::new();
    let mut typed = false;
    for ev in &events {
        if debug_clip {
            if let egui::Event::Key { key, pressed, modifiers, .. } = ev {
                if *pressed && (*key == egui::Key::V || *key == egui::Key::Insert) {
                    eprintln!("[cider] Key {key:?} ctrl={} shift={}", modifiers.ctrl, modifiers.shift);
                }
            }
        }
        match ev {
            egui::Event::Text(t) => {
                // egui already suppresses Text under Ctrl; suppress under Alt too so we don't
                // double-send the meta form synthesised from the Key event.
                if !mods.alt && !t.is_empty() {
                    out.extend_from_slice(t.as_bytes());
                    typed = true;
                }
            }
            egui::Event::Key { key, pressed: true, modifiers, .. } => {
                if modifiers.shift && *key == egui::Key::PageUp {
                    s.terminal.scroll_page(true);
                } else if modifiers.shift && *key == egui::Key::PageDown {
                    s.terminal.scroll_page(false);
                } else if modifiers.shift && *key == egui::Key::Insert {
                    // Shift+Insert — the xterm paste chord. egui-winit only steals Ctrl+V/Ctrl+Shift+V
                    // (is_paste_command), so this reaches us as a plain Key event and we read the
                    // clipboard ourselves via arboard's data-control backend. A reliable paste path
                    // that doesn't depend on egui producing a Paste event at all.
                    if let Some(text) = read_clipboard() {
                        paste_into(&text, mode, &mut out);
                        typed = true;
                    }
                } else if *key == egui::Key::A
                    && modifiers.ctrl
                    && !modifiers.shift
                    && !modifiers.alt
                    && !mode.contains(TermMode::ALT_SCREEN)
                {
                    // Ctrl+A on the PRIMARY screen = select all (scrollback + visible screen).
                    // On the alternate screen (vim, htop, tmux prefix…) it falls through to
                    // `key_to_bytes` and reaches the app as 0x01, which those apps bind
                    // themselves. Trade-off: readline's beginning-of-line Ctrl+A at a shell
                    // prompt is shadowed (Home still works).
                    s.terminal.select_all();
                    ctx.request_repaint();
                } else if let Some(bytes) = key_to_bytes(*key, *modifiers, mode) {
                    out.extend_from_slice(&bytes);
                    typed = true;
                }
            }
            // Ctrl+C (egui emits Copy for it) copies the selection when there is one and clears it,
            // otherwise sends SIGINT — so a plain Ctrl+C still interrupts when nothing's selected.
            egui::Event::Copy => {
                if let Some(sel) = s.terminal.selection_text() {
                    ctx.copy_text(sel);
                    s.terminal.clear_selection();
                } else {
                    out.push(0x03);
                }
            }
            // Ctrl+V pastes the clipboard.
            egui::Event::Paste(text) => {
                if std::env::var_os("CIDER_CLIP_DEBUG").is_some() {
                    eprintln!("[cider] Event::Paste ({} bytes)", text.len());
                }
                paste_into(text, mode, &mut out);
                typed = true;
            }
            egui::Event::Cut => out.push(0x18), // Ctrl+X → the real control code (nano etc.)
            egui::Event::MouseWheel { unit, delta, .. } if over_grid => {
                // Convert the wheel delta to (fractional) grid lines and accumulate, so
                // pixel-precise / high-resolution wheels don't lose sub-line deltas to
                // rounding.
                let lines_f = match unit {
                    egui::MouseWheelUnit::Line => delta.y * 3.0,
                    egui::MouseWheelUnit::Point => delta.y / cell_h,
                    egui::MouseWheelUnit::Page => delta.y * s.terminal.size.lines as f32,
                };
                if !lines_f.is_finite() {
                    continue; // never let a degenerate delta poison the accumulator with NaN/inf
                }
                s.scroll_accum += lines_f;
                let whole = s.scroll_accum.trunc();
                if whole != 0.0 {
                    s.scroll_accum -= whole;
                    if mode.intersects(TermMode::MOUSE_MODE) {
                        // The program enabled mouse reporting (a TUI like htop/vim/a chat
                        // UI): forward the wheel as real mouse button events (64 = up,
                        // 65 = down) at the pointer cell, so it scrolls *itself* natively.
                        if let Some(p) = hover {
                            let col = (((p.x - area.left()) / cell_w).floor() as i64).max(0) as u32 + 1;
                            let row = (((p.y - area.top()) / cell_h).floor() as i64).max(0) as u32 + 1;
                            let btn: u32 = if whole > 0.0 { 64 } else { 65 };
                            let sgr = mode.contains(TermMode::SGR_MOUSE);
                            // Cap the report burst: a huge page-unit delta must not spin this
                            // loop for thousands of iterations in one frame.
                            for _ in 0..(whole.abs() as i32).min(200) {
                                if sgr {
                                    out.extend_from_slice(format!("\x1b[<{btn};{col};{row}M").as_bytes());
                                } else {
                                    // Legacy X10 encoding (byte = 32 + value), clamped to its 223 ceiling.
                                    let enc = |v: u32| 32u8.saturating_add(v.min(223) as u8);
                                    out.extend_from_slice(&[0x1b, b'[', b'M', enc(btn), enc(col), enc(row)]);
                                }
                            }
                        }
                    } else {
                        // No mouse reporting: scroll THIS terminal's scrollback history.
                        // Never translated to arrow keys (that walked the program's own
                        // history instead of scrolling — the surprise we're fixing).
                        s.terminal.scroll_delta(whole as i32);
                        // Wheel during a selection drag: the view moved under the pointer, so
                        // extend the selection to the cell now under it.
                        if dragging {
                            if let Some(p) = hover {
                                let (point, side) =
                                    grid_point(s, p, area, cell_w, cell_h, cols, rows);
                                s.terminal.update_selection(point, side);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Real input snaps the view back to the live prompt, like every terminal.
    if typed {
        s.terminal.scroll_to_bottom();
    }
    if !out.is_empty() {
        s.write(&out);
    }
}

/// Read the system clipboard and paste it into the session (for middle-click / menu paste — egui
/// only hands us clipboard text through its own Ctrl+V Paste event). Snaps to the prompt like typing.
pub fn paste_clipboard(s: &mut Session) {
    if let Some(text) = read_clipboard() {
        let mut out = Vec::new();
        paste_into(&text, s.terminal.mode(), &mut out);
        if !out.is_empty() {
            s.write(&out);
            s.terminal.scroll_to_bottom();
        }
    }
}

/// On-demand system-clipboard read (see [`crate::clip`] for why this can't just be arboard).
pub fn read_clipboard() -> Option<String> {
    crate::clip::read()
}

/// Wrap pasted text for bracketed-paste mode, else send it with newlines normalised to CR.
fn paste_into(text: &str, mode: TermMode, out: &mut Vec<u8>) {
    // Drop NULs; a stray one can confuse line editors.
    let clean: String = text.chars().filter(|&c| c != '\0').collect();
    if mode.contains(TermMode::BRACKETED_PASTE) {
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(clean.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
    } else {
        for b in clean.bytes() {
            out.push(if b == b'\n' { b'\r' } else { b });
        }
    }
}

/// Map an egui key press (with modifiers) to the bytes a terminal expects. Returns None for a plain
/// printable key — the accompanying `Text` event carries the layout-correct character instead.
fn key_to_bytes(key: egui::Key, mods: egui::Modifiers, mode: TermMode) -> Option<Vec<u8>> {
    use egui::Key;
    let app = mode.contains(TermMode::APP_CURSOR);
    // xterm modifier code: 1 + shift(1) + alt(2) + ctrl(4).
    let modn = 1 + (mods.shift as u8) + ((mods.alt as u8) << 1) + ((mods.ctrl as u8) << 2);

    // Cursor/navigation keys: ESC[1;<m><final> when modified, else app (SS3) or normal (CSI) form.
    let csi_final = |fin: char| -> Vec<u8> {
        if modn > 1 {
            format!("\x1b[1;{modn}{fin}").into_bytes()
        } else if app {
            format!("\x1bO{fin}").into_bytes()
        } else {
            format!("\x1b[{fin}").into_bytes()
        }
    };
    // Editing/paging keys: ESC[<n>~ (with an optional ;<m> modifier).
    let tilde = |n: u32| -> Vec<u8> {
        if modn > 1 {
            format!("\x1b[{n};{modn}~").into_bytes()
        } else {
            format!("\x1b[{n}~").into_bytes()
        }
    };
    // F1–F4 are SS3 (ESC O P..S), promoted to ESC[1;<m>P.. when modified.
    let fkey = |fin: char| -> Vec<u8> {
        if modn > 1 {
            format!("\x1b[1;{modn}{fin}").into_bytes()
        } else {
            format!("\x1bO{fin}").into_bytes()
        }
    };

    match key {
        Key::ArrowUp => Some(csi_final('A')),
        Key::ArrowDown => Some(csi_final('B')),
        Key::ArrowRight => Some(csi_final('C')),
        Key::ArrowLeft => Some(csi_final('D')),
        Key::Home => Some(csi_final('H')),
        Key::End => Some(csi_final('F')),
        Key::Insert => Some(tilde(2)),
        Key::Delete => Some(tilde(3)),
        Key::PageUp => Some(tilde(5)),
        Key::PageDown => Some(tilde(6)),
        Key::Enter => Some(if mods.alt { vec![0x1b, b'\r'] } else { vec![b'\r'] }),
        Key::Backspace => Some(if mods.alt { vec![0x1b, 0x7f] } else { vec![0x7f] }),
        Key::Tab => Some(if mods.shift { b"\x1b[Z".to_vec() } else { vec![b'\t'] }),
        Key::Escape => Some(if mods.alt { vec![0x1b, 0x1b] } else { vec![0x1b] }),
        Key::F1 => Some(fkey('P')),
        Key::F2 => Some(fkey('Q')),
        Key::F3 => Some(fkey('R')),
        Key::F4 => Some(fkey('S')),
        Key::F5 => Some(tilde(15)),
        Key::F6 => Some(tilde(17)),
        Key::F7 => Some(tilde(18)),
        Key::F8 => Some(tilde(19)),
        Key::F9 => Some(tilde(20)),
        Key::F10 => Some(tilde(21)),
        Key::F11 => Some(tilde(23)),
        Key::F12 => Some(tilde(24)),
        Key::Space if mods.ctrl => Some(vec![0]), // Ctrl+Space → NUL
        _ => {
            // Printable keys: only synthesise the Ctrl (control code) or Alt (ESC-prefixed) form; a
            // plain press falls through to None so the Text event handles it.
            let base = base_char(key)?;
            if mods.ctrl && !mods.alt {
                ctrl_byte(base).map(|b| vec![b])
            } else if mods.alt && !mods.ctrl {
                let ch = if mods.shift { base.to_ascii_uppercase() } else { base };
                Some(vec![0x1b, ch as u8])
            } else {
                None
            }
        }
    }
}

/// The base (unshifted) ASCII character an egui key produces, for control/meta synthesis.
fn base_char(key: egui::Key) -> Option<char> {
    use egui::Key::*;
    Some(match key {
        A => 'a', B => 'b', C => 'c', D => 'd', E => 'e', F => 'f', G => 'g', H => 'h', I => 'i',
        J => 'j', K => 'k', L => 'l', M => 'm', N => 'n', O => 'o', P => 'p', Q => 'q', R => 'r',
        S => 's', T => 't', U => 'u', V => 'v', W => 'w', X => 'x', Y => 'y', Z => 'z',
        Num0 => '0', Num1 => '1', Num2 => '2', Num3 => '3', Num4 => '4', Num5 => '5', Num6 => '6',
        Num7 => '7', Num8 => '8', Num9 => '9',
        Space => ' ',
        Backtick => '`', Minus => '-', Equals => '=', OpenBracket => '[', CloseBracket => ']',
        Backslash => '\\', Semicolon => ';', Quote => '\'', Comma => ',', Period => '.', Slash => '/',
        _ => return None,
    })
}

/// The control code for Ctrl + `c`. (Ctrl+C/V/X never reach here — egui converts them to
/// Copy/Paste/Cut events — but they map correctly anyway.)
fn ctrl_byte(c: char) -> Option<u8> {
    match c.to_ascii_uppercase() {
        up @ 'A'..='Z' => Some(up as u8 - b'A' + 1),
        ' ' | '@' => Some(0),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' | '/' => Some(0x1f),
        _ => None,
    }
}

// --- rendering -----------------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub fn render(
    ui: &egui::Ui,
    s: &Session,
    cfg: &Config,
    font_id: &egui::FontId,
    area: egui::Rect,
    cell_w: f32,
    cell_h: f32,
    cols: usize,
    rows: usize,
    ctx: &egui::Context,
    emojis: &mut Emoji,
) {
    let painter = ui.painter_at(area);
    let default_bg = cfg.palette.bg;

    let content = s.terminal.renderable();
    let display_offset = content.display_offset;
    let colors = content.colors;
    let selection = content.selection;

    for indexed in content.display_iter {
        let point = indexed.point;
        let cell = indexed.cell;
        let Some(vp) = point_to_viewport(display_offset, point) else { continue };
        if vp.line >= rows || vp.column.0 >= cols {
            continue;
        }
        let flags = cell.flags;
        // The trailing half of a wide char is drawn by the wide char itself.
        if flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        let width_cells = if flags.contains(Flags::WIDE_CHAR) { 2.0 } else { 1.0 };
        let x = area.left() + vp.column.0 as f32 * cell_w;
        let y = area.top() + vp.line as f32 * cell_h;

        let mut fg = cfg.palette.resolve(cell.fg, colors);
        let mut bg = cfg.palette.resolve(cell.bg, colors);
        if flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        if flags.contains(Flags::DIM) {
            fg = crate::config::dim(fg);
        }

        let cell_rect =
            egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(cell_w * width_cells, cell_h));
        let selected = selection.as_ref().is_some_and(|r| r.contains(point));
        if selected {
            painter.rect_filled(cell_rect, egui::Rounding::ZERO, SEL_BG);
        } else if bg != default_bg {
            painter.rect_filled(cell_rect, egui::Rounding::ZERO, color32(bg, 1.0));
        }

        let c = cell.c;
        if c != ' ' && c != '\0' && !flags.contains(Flags::HIDDEN) {
            // Colour emoji go around the text pipeline as textures. Gate: emoji block AND
            // (double-width cell OR an explicit VS16 in the zero-width extras) — see emoji.rs.
            let vs16 = cell.zerowidth().is_some_and(|zw| zw.contains(&'\u{FE0F}'));
            let wide = flags.contains(Flags::WIDE_CHAR);
            let mut drew_texture = false;
            if emoji::wants_texture(c, wide, vs16) {
                if let Some(tex) = emojis.texture(ctx, c) {
                    // Largest square that fits the cell's own footprint, centred — an emoji can
                    // never bleed into a neighbouring cell, even for narrow VS16 bases.
                    let side = (cell_w * width_cells).min(cell_h);
                    let img_rect = egui::Rect::from_center_size(cell_rect.center(), egui::vec2(side, side));
                    let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                    painter.image(tex.id(), img_rect, uv, egui::Color32::WHITE);
                    drew_texture = true;
                }
            }
            if !drew_texture {
                let col = color32(fg, 1.0);
                let gy = y + cell_h * 0.5;
                painter.text(egui::pos2(x, gy), egui::Align2::LEFT_CENTER, c, font_id.clone(), col);
                if flags.contains(Flags::BOLD) {
                    // Faux bold: a second pass a half pixel over (default fonts ship no bold monospace).
                    painter.text(egui::pos2(x + 0.5, gy), egui::Align2::LEFT_CENTER, c, font_id.clone(), col);
                }
            }
        }
        let line_w = cell_w * width_cells;
        if flags.intersects(Flags::ALL_UNDERLINES) {
            let uy = y + cell_h - 1.5;
            painter.line_segment(
                [egui::pos2(x, uy), egui::pos2(x + line_w, uy)],
                egui::Stroke::new(1.0, color32(fg, 1.0)),
            );
        }
        if flags.contains(Flags::STRIKEOUT) {
            let sy = y + cell_h * 0.5;
            painter.line_segment(
                [egui::pos2(x, sy), egui::pos2(x + line_w, sy)],
                egui::Stroke::new(1.0, color32(fg, 1.0)),
            );
        }
    }

    draw_cursor(&painter, s, cfg, font_id, content.cursor, display_offset, area, cell_w, cell_h, cols, rows, ctx);
}

#[allow(clippy::too_many_arguments)]
fn draw_cursor(
    painter: &egui::Painter,
    s: &Session,
    cfg: &Config,
    font_id: &egui::FontId,
    cursor: alacritty_terminal::term::RenderableCursor,
    display_offset: usize,
    area: egui::Rect,
    cell_w: f32,
    cell_h: f32,
    cols: usize,
    rows: usize,
    ctx: &egui::Context,
) {
    if cursor.shape == CursorShape::Hidden {
        return;
    }
    let Some(vp) = point_to_viewport(display_offset, cursor.point) else { return };
    if vp.line >= rows || vp.column.0 >= cols {
        return;
    }
    let x = area.left() + vp.column.0 as f32 * cell_w;
    let y = area.top() + vp.line as f32 * cell_h;
    let (focused, time) = ctx.input(|i| (i.viewport().focused.unwrap_or(true), i.time));
    let blink_on = ((time / 0.53) as i64) % 2 == 0;
    let cur = color32(cfg.palette.cursor, 1.0);
    let stroke = egui::Stroke::new(1.0, cur);
    let full = egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(cell_w, cell_h));
    let underscore = egui::Rect::from_min_size(egui::pos2(x, y + cell_h - 2.0), egui::vec2(cell_w, 2.0));

    // Unfocused panes show a blinking underscore rather than the shape's own idle form.
    if !focused {
        if blink_on {
            painter.rect_filled(underscore, egui::Rounding::ZERO, cur);
        }
        return;
    }

    match cursor.shape {
        CursorShape::Block => {
            if blink_on {
                painter.rect_filled(full, egui::Rounding::ZERO, cur);
                // Redraw the glyph under the block in the background colour so it stays legible.
                let under = s.terminal.char_at(cursor.point);
                if under != ' ' && under != '\0' {
                    painter.text(
                        egui::pos2(x, y + cell_h * 0.5),
                        egui::Align2::LEFT_CENTER,
                        under,
                        font_id.clone(),
                        color32(cfg.palette.bg, 1.0),
                    );
                }
            }
        }
        CursorShape::HollowBlock => {
            painter.rect_stroke(full, egui::Rounding::ZERO, stroke);
        }
        CursorShape::Beam => {
            if blink_on {
                let bar = egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(2.0, cell_h));
                painter.rect_filled(bar, egui::Rounding::ZERO, cur);
            }
        }
        CursorShape::Underline => {
            if blink_on {
                painter.rect_filled(underscore, egui::Rounding::ZERO, cur);
            }
        }
        CursorShape::Hidden => {}
    }
}

/// Alacritty RGB → egui colour, applying `alpha` (used for the translucent background).
pub fn color32(c: Rgb, alpha: f32) -> egui::Color32 {
    if alpha >= 1.0 {
        egui::Color32::from_rgb(c.r, c.g, c.b)
    } else {
        egui::Color32::from_rgba_unmultiplied(c.r, c.g, c.b, (alpha * 255.0) as u8)
    }
}
