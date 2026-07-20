//! Ctrl+P quick-open: a centered fuzzy file picker over the workspace's flat file list.
//!
//! Matching is `nucleo-matcher` (the Helix/Zed fuzzy engine): each frame the query is scored
//! against every workspace-relative path and the top results are shown, best-first. Results are
//! recomputed only when the query changes (idle frames are free). Keyboard: ↑/↓ move, Enter opens
//! the highlighted row, Esc dismisses.

use std::path::{Path, PathBuf};

use egui::{Color32, Key};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

/// Max rows shown in the results list.
const MAX_RESULTS: usize = 50;
const ORANGE: Color32 = Color32::from_rgb(233, 110, 44);
const TEXT: Color32 = Color32::from_rgb(238, 235, 232);
const DIM: Color32 = Color32::from_rgb(150, 145, 140);

/// One candidate: workspace-relative display string + absolute path to open.
struct Entry {
    rel: String,
    abs: PathBuf,
}

pub struct QuickOpen {
    open: bool,
    query: String,
    /// True the frame the overlay opens — used to focus the text field once.
    just_opened: bool,
    entries: Vec<Entry>,
    matcher: Matcher,
    /// Indices into `entries`, best-first, for the current query.
    results: Vec<usize>,
    /// Query the `results` were computed for (recompute only on change).
    results_for: Option<String>,
    /// Selected row within `results`.
    selected: usize,
}

impl Default for QuickOpen {
    fn default() -> Self {
        Self {
            open: false,
            query: String::new(),
            just_opened: false,
            entries: Vec::new(),
            matcher: Matcher::new(Config::DEFAULT.match_paths()),
            results: Vec::new(),
            results_for: None,
            selected: 0,
        }
    }
}

impl QuickOpen {
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Open the picker over `files` (absolute paths), showing them relative to `root`.
    pub fn open(&mut self, files: &[PathBuf], root: &Path) {
        self.entries = files
            .iter()
            .map(|abs| {
                let rel = abs.strip_prefix(root).unwrap_or(abs).to_string_lossy().into_owned();
                Entry { rel, abs: abs.clone() }
            })
            .collect();
        self.open = true;
        self.just_opened = true;
        self.query.clear();
        self.results_for = None;
        self.selected = 0;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.entries.clear();
        self.results.clear();
        self.results_for = None;
    }

    /// Draw the overlay if open. Returns `Some(path)` the frame a file is chosen (which also
    /// closes the picker). Esc / clicking away closes it, returning `None`.
    /// The 1-based line the current query asks for, if it carries a `:42` suffix.
    pub fn requested_line(&self) -> Option<usize> {
        split_line_suffix(&self.query).1
    }

    pub fn ui(&mut self, ctx: &egui::Context) -> Option<PathBuf> {
        if !self.open {
            return None;
        }

        // Global keys handled before the text field consumes them.
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.close();
            return None;
        }
        let mut chosen: Option<PathBuf> = None;

        egui::Area::new("quickopen".into())
            .anchor(egui::Align2::CENTER_TOP, [0.0, 80.0])
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .inner_margin(egui::Margin::same(crate::style::sizes::OVERLAY_PAD))
                    .show(ui, |ui| {
                        ui.set_width(560.0);

                        let edit = egui::TextEdit::singleline(&mut self.query)
                            .hint_text("Go to file…")
                            .desired_width(f32::INFINITY)
                            .font(egui::TextStyle::Monospace);
                        let resp = ui.add(edit);
                        if self.just_opened {
                            resp.request_focus();
                            self.just_opened = false;
                        }

                        self.recompute_if_changed();

                        // Arrow navigation (clamped to the visible result window).
                        let shown = self.results.len().min(MAX_RESULTS);
                        if shown > 0 {
                            if ui.input(|i| i.key_pressed(Key::ArrowDown)) {
                                self.selected = (self.selected + 1) % shown;
                            }
                            if ui.input(|i| i.key_pressed(Key::ArrowUp)) {
                                self.selected = (self.selected + shown - 1) % shown;
                            }
                            if ui.input(|i| i.key_pressed(Key::Enter)) {
                                chosen = Some(self.entries[self.results[self.selected]].abs.clone());
                            }
                        }

                        ui.separator();
                        egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                            for (row, &idx) in self.results.iter().take(MAX_RESULTS).enumerate() {
                                let entry = &self.entries[idx];
                                let selected = row == self.selected;
                                let (name, dir) = split_tail(&entry.rel);
                                let mut job = egui::text::LayoutJob::default();
                                let font = egui::TextStyle::Monospace.resolve(ui.style());
                                job.append(
                                    name,
                                    0.0,
                                    egui::TextFormat {
                                        font_id: font.clone(),
                                        color: if selected { ORANGE } else { TEXT },
                                        ..Default::default()
                                    },
                                );
                                if !dir.is_empty() {
                                    job.append(
                                        &format!("  {dir}"),
                                        0.0,
                                        egui::TextFormat { font_id: font, color: DIM, ..Default::default() },
                                    );
                                }
                                if ui.selectable_label(selected, job).clicked_by(egui::PointerButton::Primary) {
                                    chosen = Some(entry.abs.clone());
                                }
                            }
                            if self.results.is_empty() && !self.query.is_empty() {
                                ui.colored_label(DIM, "no matches");
                            }
                        });
                    });
            });

        if chosen.is_some() {
            self.close();
        }
        chosen
    }

    /// Rescore all entries against the current query, best-first, unless the query is unchanged.
    fn recompute_if_changed(&mut self) {
        if self.results_for.as_deref() == Some(self.query.as_str()) {
            return;
        }
        self.results_for = Some(self.query.clone());
        self.selected = 0;
        // `main.c:42` means "open main.c at line 42": only the name half is matched. Pasting a
        // compiler/grep location straight into the picker is the whole point, so the suffix must
        // not poison the score.
        let (name_part, _) = split_line_suffix(&self.query);
        let query = name_part.to_string();

        if query.is_empty() {
            // Empty query: show everything in the workspace's natural (sorted) order.
            self.results = (0..self.entries.len()).collect();
            self.results.truncate(MAX_RESULTS * 4);
            return;
        }

        let pattern = Pattern::parse(&query, CaseMatching::Smart, Normalization::Smart);
        let mut buf = Vec::new();
        let mut scored: Vec<(u32, usize)> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                let hay = Utf32Str::new(&e.rel, &mut buf);
                pattern.score(hay, &mut self.matcher).map(|s| (s, i))
            })
            .collect();
        // Higher score first; ties broken by the workspace's stable sort order (lower index).
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        self.results = scored.into_iter().map(|(_, i)| i).collect();
    }
}

/// Split a workspace-relative path into (file name, parent dir) for two-tone display.
fn split_tail(rel: &str) -> (&str, &str) {
    match rel.rfind('/') {
        Some(slash) => (&rel[slash + 1..], &rel[..slash]),
        None => (rel, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_tail_separates_name_and_dir() {
        assert_eq!(split_tail("src/main.rs"), ("main.rs", "src"));
        assert_eq!(split_tail("README.md"), ("README.md", ""));
        assert_eq!(split_tail("a/b/c.rs"), ("c.rs", "a/b"));
    }

    /// A picker preloaded with `rels` as its entries (abs = "/r/<rel>"), ready to score `query`.
    fn with_entries(rels: impl IntoIterator<Item = &'static str>, query: &str) -> QuickOpen {
        let entries = rels
            .into_iter()
            .map(|p| Entry { rel: p.to_string(), abs: PathBuf::from(format!("/r/{p}")) })
            .collect();
        QuickOpen { entries, query: query.to_string(), ..Default::default() }
    }

    #[test]
    fn empty_query_lists_all_capped() {
        let rels: Vec<String> = (0..500).map(|i| format!("f{i}.rs")).collect();
        let entries = rels.iter().map(|r| Entry { rel: r.clone(), abs: PathBuf::from(r) }).collect();
        let mut qo = QuickOpen { entries, ..Default::default() };
        qo.recompute_if_changed();
        assert_eq!(qo.results.len(), MAX_RESULTS * 4);
    }

    #[test]
    fn fuzzy_ranks_subsequence_matches() {
        let mut qo = with_entries(["src/workspace.rs", "src/main.rs", "docs/scope.md"], "wksp");
        qo.recompute_if_changed();
        assert!(!qo.results.is_empty());
        assert_eq!(qo.entries[qo.results[0]].rel, "src/workspace.rs");
    }
}

/// Split a picker query into its name part and an optional 1-based line number.
///
/// `main.c:42` -> ("main.c", Some(42)); `main.c:42:9` -> ("main.c", Some(42)) (a column follows
/// the same convention as compiler output and is ignored — the picker positions by line).
/// A trailing bare `:` is treated as "still typing" and matched as a plain name, so results do
/// not flicker away between the colon and the digits.
fn split_line_suffix(query: &str) -> (&str, Option<usize>) {
    let Some((name, rest)) = query.rsplit_once(':') else { return (query, None) };
    // `main.c:42:9` — take the LINE, drop the column.
    let (name, rest) = match name.rsplit_once(':') {
        Some((n, mid)) if mid.chars().all(|c| c.is_ascii_digit()) && !mid.is_empty() => (n, mid),
        _ => (name, rest),
    };
    if rest.is_empty() || !rest.chars().all(|c| c.is_ascii_digit()) {
        return (query, None);
    }
    match rest.parse::<usize>() {
        Ok(n) if n > 0 => (name, Some(n)),
        _ => (query, None),
    }
}

#[cfg(test)]
mod line_suffix_tests {
    use super::split_line_suffix;

    #[test]
    fn plain_names_are_untouched() {
        assert_eq!(split_line_suffix("main.c"), ("main.c", None));
        assert_eq!(split_line_suffix(""), ("", None));
    }

    #[test]
    fn line_suffix_is_split_off() {
        assert_eq!(split_line_suffix("main.c:42"), ("main.c", Some(42)));
    }

    #[test]
    fn compiler_style_line_and_column_keeps_the_line() {
        assert_eq!(split_line_suffix("src/main.c:42:9"), ("src/main.c", Some(42)));
    }

    #[test]
    fn a_half_typed_suffix_still_matches_by_name() {
        // Between the colon and the digits the results must not vanish.
        assert_eq!(split_line_suffix("main.c:"), ("main.c:", None));
    }

    #[test]
    fn non_numeric_after_a_colon_is_part_of_the_name() {
        // Windows drive letters and namespaced names must not be mangled.
        assert_eq!(split_line_suffix("C:foo"), ("C:foo", None));
        assert_eq!(split_line_suffix("std::vector"), ("std::vector", None));
    }

    #[test]
    fn line_zero_is_not_a_line() {
        assert_eq!(split_line_suffix("main.c:0"), ("main.c:0", None));
    }
}
