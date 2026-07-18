//! Double-Shift Search Everywhere — JetBrains' one box over everything: files, project
//! symbols, and actions, fuzzy-matched together and shown in sections.
//!
//! Composes the existing engines rather than inventing new ones: `nucleo-matcher` scores
//! files (quick-open's corpus) and actions (the palette's [`crate::palette::COMMANDS`]);
//! symbols come pre-ranked from [`crate::symbols::SymbolIndex::query`]. The overlay only
//! CHOOSES a [`Hit`]; the app performs it (open file / jump to symbol / run_command), same
//! split as the palette.

use std::path::{Path, PathBuf};

use egui::{Color32, Key};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use crate::palette;
use crate::symbols::SymbolIndex;

const ORANGE: Color32 = Color32::from_rgb(233, 110, 44);
const TEXT: Color32 = Color32::from_rgb(238, 235, 232);
const DIM: Color32 = Color32::from_rgb(150, 145, 140);

const MAX_FILES: usize = 12;
const MAX_SYMBOLS: usize = 12;
const MAX_ACTIONS: usize = 8;
/// Two clean Shift taps at most this far apart open the overlay.
pub const DOUBLE_SHIFT_WINDOW: f64 = 0.4;

/// What the user picked.
pub enum Hit {
    File(PathBuf),
    /// `(path, 0-based line)`.
    Symbol(PathBuf, usize),
    Command(palette::Command),
}

/// One visible row: section-tagged, pre-rendered label parts.
struct Row {
    /// Main text (file name / symbol name / action label).
    head: String,
    /// Dimmed context (dir / file:line / keybind).
    tail: String,
    /// Symbol glyph + color; None for files/actions.
    glyph: Option<(&'static str, Color32)>,
    hit: Hit,
}

pub struct SearchEverywhere {
    open: bool,
    query: String,
    just_opened: bool,
    /// `(workspace-relative, absolute)` file corpus, loaded at open().
    files: Vec<(String, PathBuf)>,
    matcher: Matcher,
    rows: Vec<Row>,
    /// `(query, symbol-index generation)` the rows were computed for — symbols stream in
    /// (regex build, PSI/LSP tiers), so a generation bump recomputes even with a still query.
    rows_for: Option<(String, u64)>,
    selected: usize,
}

impl Default for SearchEverywhere {
    fn default() -> Self {
        Self {
            open: false,
            query: String::new(),
            just_opened: false,
            files: Vec::new(),
            matcher: Matcher::new(Config::DEFAULT.match_paths()),
            rows: Vec::new(),
            rows_for: None,
            selected: 0,
        }
    }
}

impl SearchEverywhere {
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Open over `files` (absolute), displayed relative to `root`.
    pub fn open(&mut self, files: &[PathBuf], root: &Path) {
        self.files = files
            .iter()
            .map(|abs| {
                let rel = abs.strip_prefix(root).unwrap_or(abs).to_string_lossy().into_owned();
                (rel, abs.clone())
            })
            .collect();
        self.open = true;
        self.just_opened = true;
        self.query.clear();
        self.rows_for = None;
        self.selected = 0;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.files.clear();
        self.rows.clear();
        self.rows_for = None;
    }

    /// Draw the overlay if open. Returns the chosen [`Hit`] (which also closes it).
    pub fn ui(&mut self, ctx: &egui::Context, symbols: &SymbolIndex) -> Option<Hit> {
        if !self.open {
            return None;
        }
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.close();
            return None;
        }
        let mut chosen: Option<usize> = None;

        egui::Area::new("search-everywhere".into())
            .anchor(egui::Align2::CENTER_TOP, [0.0, 80.0])
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .inner_margin(egui::Margin::same(crate::style::sizes::OVERLAY_PAD))
                    .show(ui, |ui| {
                        ui.set_width(640.0);

                        let edit = egui::TextEdit::singleline(&mut self.query)
                            .hint_text("Search everywhere: files, symbols, actions…")
                            .desired_width(f32::INFINITY)
                            .font(egui::TextStyle::Monospace);
                        let resp = ui.add(edit);
                        if self.just_opened {
                            resp.request_focus();
                            self.just_opened = false;
                        }

                        self.recompute_if_changed(symbols);

                        let shown = self.rows.len();
                        if shown > 0 {
                            if ui.input(|i| i.key_pressed(Key::ArrowDown)) {
                                self.selected = (self.selected + 1) % shown;
                            }
                            if ui.input(|i| i.key_pressed(Key::ArrowUp)) {
                                self.selected = (self.selected + shown - 1) % shown;
                            }
                            if ui.input(|i| i.key_pressed(Key::Enter)) {
                                chosen = Some(self.selected);
                            }
                        }

                        ui.separator();
                        egui::ScrollArea::vertical().max_height(400.0).show(ui, |ui| {
                            let font = egui::TextStyle::Monospace.resolve(ui.style());
                            let mut last_section: Option<&'static str> = None;
                            for (row_i, row) in self.rows.iter().enumerate() {
                                let section = match row.hit {
                                    Hit::File(_) => "Files",
                                    Hit::Symbol(..) => "Symbols",
                                    Hit::Command(_) => "Actions",
                                };
                                if last_section != Some(section) {
                                    last_section = Some(section);
                                    ui.add_space(2.0);
                                    ui.colored_label(DIM, egui::RichText::new(section).size(11.0));
                                }
                                let selected = row_i == self.selected;
                                let mut job = egui::text::LayoutJob::default();
                                if let Some((glyph, color)) = row.glyph {
                                    job.append(
                                        glyph,
                                        0.0,
                                        egui::TextFormat { font_id: font.clone(), color, ..Default::default() },
                                    );
                                    job.append(" ", 0.0, egui::TextFormat::default());
                                }
                                job.append(
                                    &row.head,
                                    0.0,
                                    egui::TextFormat {
                                        font_id: font.clone(),
                                        color: if selected { ORANGE } else { TEXT },
                                        ..Default::default()
                                    },
                                );
                                if !row.tail.is_empty() {
                                    job.append(
                                        &format!("  {}", row.tail),
                                        0.0,
                                        egui::TextFormat { font_id: font.clone(), color: DIM, ..Default::default() },
                                    );
                                }
                                if ui.selectable_label(selected, job).clicked_by(egui::PointerButton::Primary) {
                                    chosen = Some(row_i);
                                }
                            }
                            if self.rows.is_empty() && !self.query.is_empty() {
                                ui.colored_label(DIM, "no matches");
                            }
                        });
                    });
            });

        if let Some(i) = chosen {
            let hit = self.rows.swap_remove(i).hit;
            self.close();
            return Some(hit);
        }
        None
    }

    /// Rebuild the mixed result rows when the query (or the streaming symbol index) changed.
    fn recompute_if_changed(&mut self, symbols: &SymbolIndex) {
        let key = (self.query.clone(), symbols.generation());
        if self.rows_for.as_ref() == Some(&key) {
            return;
        }
        self.rows_for = Some(key);
        self.selected = 0;
        self.rows.clear();

        if self.query.is_empty() {
            // Empty query: files in workspace order (the corpus arrives recent/sorted), then
            // every action — a browsable launcher, like the palette's default list.
            for (rel, abs) in self.files.iter().take(MAX_FILES) {
                self.rows.push(file_row(rel, abs));
            }
            for (cmd, label, bind) in palette::COMMANDS.iter().take(MAX_ACTIONS) {
                self.rows.push(Row {
                    head: (*label).to_string(),
                    tail: (*bind).to_string(),
                    glyph: None,
                    hit: Hit::Command(*cmd),
                });
            }
            return;
        }

        let pattern = Pattern::parse(&self.query, CaseMatching::Smart, Normalization::Smart);
        let mut buf = Vec::new();

        // Files.
        let mut scored: Vec<(u32, usize)> = self
            .files
            .iter()
            .enumerate()
            .filter_map(|(i, (rel, _))| {
                let hay = Utf32Str::new(rel, &mut buf);
                pattern.score(hay, &mut self.matcher).map(|s| (s, i))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        for &(_, i) in scored.iter().take(MAX_FILES) {
            let (rel, abs) = &self.files[i];
            self.rows.push(file_row(rel, abs));
        }

        // Symbols — the index ranks these itself (exact > prefix > substring > subsequence).
        for e in symbols.query(&self.query, MAX_SYMBOLS) {
            let file = e.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            self.rows.push(Row {
                head: e.name.clone(),
                tail: format!("{file}:{}", e.line + 1),
                glyph: Some((e.kind.glyph(), e.kind.color())),
                hit: Hit::Symbol(e.path.clone(), e.line),
            });
        }

        // Actions.
        let mut scored: Vec<(u32, usize)> = palette::COMMANDS
            .iter()
            .enumerate()
            .filter_map(|(i, (_, label, _))| {
                let hay = Utf32Str::new(label, &mut buf);
                pattern.score(hay, &mut self.matcher).map(|s| (s, i))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        for &(_, i) in scored.iter().take(MAX_ACTIONS) {
            let (cmd, label, bind) = palette::COMMANDS[i];
            self.rows.push(Row {
                head: label.to_string(),
                tail: bind.to_string(),
                glyph: None,
                hit: Hit::Command(cmd),
            });
        }
    }
}

fn file_row(rel: &str, abs: &Path) -> Row {
    let (name, dir) = match rel.rfind('/') {
        Some(s) => (&rel[s + 1..], &rel[..s]),
        None => (rel, ""),
    };
    Row {
        head: name.to_string(),
        tail: dir.to_string(),
        glyph: None,
        hit: Hit::File(abs.to_path_buf()),
    }
}

/// Double-Shift detector, fed once per frame from raw input state. A "clean tap" is Shift
/// going down and up with NO other key pressed while held (so Shift+F6 chords, shifted
/// typing, and Shift+arrows never count). Two clean taps within [`DOUBLE_SHIFT_WINDOW`]
/// seconds fire.
#[derive(Default)]
pub struct DoubleShift {
    shift_was_down: bool,
    /// A non-shift key was pressed while shift was held → this hold is a chord, not a tap.
    dirty: bool,
    last_clean_release: Option<f64>,
}

impl DoubleShift {
    /// `shift_down`: modifiers.shift this frame. `other_key_active`: any non-modifier key
    /// currently down. `now`: seconds. Returns true the frame a double-tap completes.
    pub fn update(&mut self, shift_down: bool, other_key_active: bool, now: f64) -> bool {
        if shift_down && other_key_active {
            self.dirty = true;
        }
        let released = self.shift_was_down && !shift_down;
        let mut fire = false;
        if released {
            if !self.dirty {
                if self.last_clean_release.is_some_and(|t| now - t <= DOUBLE_SHIFT_WINDOW) {
                    fire = true;
                    self.last_clean_release = None;
                } else {
                    self.last_clean_release = Some(now);
                }
            } else {
                self.last_clean_release = None;
            }
            self.dirty = false;
        }
        self.shift_was_down = shift_down;
        fire
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// tap-tap fires; chorded shift (Shift+X) never counts toward a double-tap.
    #[test]
    fn double_shift_taps_and_chords() {
        let mut d = DoubleShift::default();
        // First clean tap: down, up.
        assert!(!d.update(true, false, 0.0));
        assert!(!d.update(false, false, 0.05));
        // Second clean tap inside the window → fires on release.
        assert!(!d.update(true, false, 0.1));
        assert!(d.update(false, false, 0.15));
        // Fired state resets: a third tap alone does not fire.
        assert!(!d.update(true, false, 0.2));
        assert!(!d.update(false, false, 0.25));

        // Chord: shift down + letter → dirty, its release is not a tap.
        let mut d = DoubleShift::default();
        assert!(!d.update(true, false, 0.0));
        assert!(!d.update(true, true, 0.02)); // Shift+X pressed
        assert!(!d.update(false, false, 0.05));
        assert!(!d.update(true, false, 0.1));
        assert!(!d.update(false, false, 0.15), "chord + tap must not fire");
    }

    /// Taps outside the window don't fire.
    #[test]
    fn double_shift_window_expires() {
        let mut d = DoubleShift::default();
        assert!(!d.update(true, false, 0.0));
        assert!(!d.update(false, false, 0.05));
        assert!(!d.update(true, false, 1.0));
        assert!(!d.update(false, false, 1.05));
    }
}
