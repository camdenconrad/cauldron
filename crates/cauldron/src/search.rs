//! Repo-wide search (Ctrl+Shift+F): Text / Regex / Symbol modes over every workspace file,
//! on a background thread, results streamed into an overlay — click/Enter jumps to file:line.
//! Extras: file mask ("*.c;*.h" glob semantics) and a case-sensitivity toggle. Symbol mode
//! delegates to the project symbol index (symbols.rs) and lists matching definitions.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::mpsc;

use egui::Key;
use regex::RegexBuilder;

use crate::style::{colors, sizes};
use crate::symbols::SymbolIndex;

const MAX_RESULTS: usize = 400;
/// Files larger than this are skipped (generated blobs, not code).
const MAX_FILE_BYTES: u64 = 2_000_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SearchMode {
    Text,
    Regex,
    Symbol,
}

pub struct Hit {
    pub path: PathBuf,
    /// 0-based line.
    pub line: usize,
    pub preview: String,
}

enum Msg {
    Hit { seq: u64, hit: Hit },
    Done { seq: u64, searched: usize },
    /// A computed (not yet applied) Replace-in-Files plan: `(path, new_content, occurrences)`
    /// per file with at least one match. The UI shows a confirm step before releasing it.
    ReplacePlan { seq: u64, plan: Vec<(PathBuf, String, usize)> },
}

pub struct RepoSearch {
    open: bool,
    query: String,
    mode: SearchMode,
    /// File mask, e.g. `*.c;*.h` (empty = all files). `;` or `,` separated globs.
    mask: String,
    case_sensitive: bool,
    /// Bad-regex message shown under the input (cleared per launch).
    error: Option<String>,
    results: Vec<Hit>,
    selected: usize,
    running: bool,
    searched: usize,
    just_opened: bool,
    rx: Receiver<Msg>,
    tx: Sender<Msg>,
    /// Bumped per launched search; stale threads' messages are ignored.
    seq: u64,
    live_seq: u64,
    /// Replace-in-Files replacement text (Text/Regex modes; `$1` groups in Regex mode).
    replace: String,
    /// A replace-plan worker is out.
    replacing: bool,
    /// Computed plan awaiting the user's confirm click.
    pending_plan: Option<Vec<(PathBuf, String, usize)>>,
    /// Confirmed plan the integrator applies (open buffers undo-safely, others to disk).
    confirmed_plan: Option<Vec<(PathBuf, String)>>,
}

impl Default for RepoSearch {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            open: false,
            query: String::new(),
            mode: SearchMode::Text,
            mask: String::new(),
            case_sensitive: false,
            error: None,
            results: Vec::new(),
            selected: 0,
            running: false,
            searched: 0,
            just_opened: false,
            rx,
            tx,
            seq: 0,
            live_seq: 0,
            replace: String::new(),
            replacing: false,
            pending_plan: None,
            confirmed_plan: None,
        }
    }
}

/// Does `name` match one glob pattern (`*` = any run, `?` = any one char)? Case-insensitive.
///
/// Iterative two-pointer matcher with single-star backtracking — O(pattern × name) worst
/// case and zero recursion. The old recursive form was exponential on stacked stars
/// (`****a` against a long name), and a hostile mask string must not hang the UI thread.
fn glob_match(pat: &str, name: &str) -> bool {
    let p: Vec<char> = pat.to_lowercase().chars().collect();
    let n: Vec<char> = name.to_lowercase().chars().collect();
    let (mut pi, mut ni) = (0usize, 0usize);
    // Last `*` seen and the name position its current guess resumes from.
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            mark = ni;
            pi += 1;
        } else if star != usize::MAX {
            // Grow the last star's span by one and retry from after it.
            pi = star + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Does the file name of `path` pass `mask` ("*.c;*.h")? Empty mask passes everything.
fn mask_matches(mask: &str, path: &std::path::Path) -> bool {
    let mask = mask.trim();
    if mask.is_empty() {
        return true;
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else { return false };
    mask.split([';', ','])
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .any(|pat| glob_match(pat, name))
}

/// Unsaved-edit shadowing: dirty buffer contents captured on the UI thread at launch time,
/// keyed by absolute path. A path present here is searched from the overlay INSTEAD of disk,
/// so hits and line numbers reflect what the user actually sees in the editor.
pub type DirtyOverlay = Arc<HashMap<PathBuf, String>>;

/// Worker-side content fetch: the dirty-buffer overlay shadows disk. Returns `None` when the
/// file is oversized (either form) or unreadable and absent from the overlay.
fn content_for(path: &Path, overlay: &HashMap<PathBuf, String>) -> Option<String> {
    if let Some(text) = overlay.get(path) {
        if text.len() as u64 > MAX_FILE_BYTES {
            return None;
        }
        return Some(text.clone());
    }
    if std::fs::metadata(path).map(|m| m.len() > MAX_FILE_BYTES).unwrap_or(true) {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// The per-line predicate a text/regex search thread runs. Built once per launch.
enum LineMatcher {
    /// Needle pre-lowercased when insensitive.
    Text { needle: String, case_sensitive: bool },
    Regex(regex::Regex),
}

impl LineMatcher {
    fn matches(&self, line: &str) -> bool {
        match self {
            LineMatcher::Text { needle, case_sensitive } => {
                if *case_sensitive {
                    line.contains(needle.as_str())
                } else {
                    line.to_lowercase().contains(needle.as_str())
                }
            }
            LineMatcher::Regex(re) => re.is_match(line),
        }
    }
}

impl RepoSearch {
    pub fn open(&mut self, seed: &str) {
        self.open = true;
        self.just_opened = true;
        if !seed.is_empty() {
            self.query = seed.to_string();
        }
        self.selected = 0;
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    /// Whether the overlay is showing.
    #[allow(dead_code)] // public panel state; the lazy-overlay rework removed the last caller
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Build the line matcher for the current mode/toggles, or record an error.
    fn build_matcher(&mut self) -> Option<LineMatcher> {
        self.error = None;
        let q = self.query.trim();
        if q.is_empty() {
            return None;
        }
        match self.mode {
            SearchMode::Text => Some(LineMatcher::Text {
                needle: if self.case_sensitive { q.to_string() } else { q.to_lowercase() },
                case_sensitive: self.case_sensitive,
            }),
            SearchMode::Regex => match RegexBuilder::new(q)
                .case_insensitive(!self.case_sensitive)
                .build()
            {
                Ok(re) => Some(LineMatcher::Regex(re)),
                Err(e) => {
                    self.error = Some(format!("bad regex: {e}"));
                    None
                }
            },
            SearchMode::Symbol => None, // handled synchronously in launch()
        }
    }

    /// One regex over both modes: Text escapes the needle (literal), Regex takes it raw —
    /// so replacement, counting, and case handling share a single engine. None + error set
    /// on a bad user regex; None silently on an empty query.
    fn build_replace_regex(&mut self) -> Option<regex::Regex> {
        self.error = None;
        let q = self.query.trim();
        if q.is_empty() || self.mode == SearchMode::Symbol {
            return None;
        }
        let pattern = match self.mode {
            SearchMode::Text => regex::escape(q),
            _ => q.to_string(),
        };
        match RegexBuilder::new(&pattern).case_insensitive(!self.case_sensitive).build() {
            Ok(re) => Some(re),
            Err(e) => {
                self.error = Some(format!("bad regex: {e}"));
                None
            }
        }
    }

    /// Compute a Replace-in-Files plan on a background thread. Nothing is modified here —
    /// the plan comes back as [`Msg::ReplacePlan`] and waits for the confirm click.
    fn launch_replace(&mut self, files: Vec<PathBuf>, overlay: &dyn Fn() -> DirtyOverlay, ctx: &egui::Context) {
        let Some(re) = self.build_replace_regex() else { return };
        // Take over the live seq: hits still streaming from a running search are stale next
        // to the replace pass, and a later relaunch invalidates this plan the same way.
        self.seq += 1;
        self.live_seq = self.seq;
        self.replacing = true;
        self.pending_plan = None;
        let seq = self.seq;
        let mask = self.mask.clone();
        let replacement = self.replace.clone();
        let literal = self.mode == SearchMode::Text;
        let overlay = overlay();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("cauldron-repo-replace".into())
            .spawn(move || {
                let mut plan = Vec::new();
                for path in files {
                    if !mask_matches(&mask, &path) {
                        continue;
                    }
                    let Some(text) = content_for(&path, &overlay) else { continue };
                    let count = re.find_iter(&text).count();
                    if count == 0 {
                        continue;
                    }
                    let new = if literal {
                        re.replace_all(&text, regex::NoExpand(&replacement)).into_owned()
                    } else {
                        re.replace_all(&text, replacement.as_str()).into_owned()
                    };
                    plan.push((path, new, count));
                }
                let _ = tx.send(Msg::ReplacePlan { seq, plan });
                ctx.request_repaint();
            })
            .ok();
    }

    /// The confirmed plan, once, for the integrator to apply.
    pub fn take_confirmed_replacements(&mut self) -> Option<Vec<(PathBuf, String)>> {
        self.confirmed_plan.take()
    }

    fn launch(
        &mut self,
        files: Vec<PathBuf>,
        overlay: &dyn Fn() -> DirtyOverlay,
        ctx: &egui::Context,
        symbols: Option<&SymbolIndex>,
    ) {
        self.seq += 1;
        self.live_seq = self.seq;
        self.results.clear();
        self.selected = 0;
        self.searched = 0;
        // A new search supersedes any computed-but-unconfirmed replace plan (it was built
        // against the previous query).
        self.pending_plan = None;
        self.replacing = false;

        if self.mode == SearchMode::Symbol {
            // Symbol mode is synchronous: query the in-memory index, honoring the mask.
            self.running = false;
            self.error = None;
            let q = self.query.trim();
            if q.is_empty() {
                return;
            }
            let Some(index) = symbols else {
                self.error = Some("symbol index not available".into());
                return;
            };
            let mask = self.mask.clone();
            self.results = index
                .query(q, MAX_RESULTS * 4)
                .into_iter()
                .filter(|e| mask_matches(&mask, &e.path))
                .take(MAX_RESULTS)
                .map(|e| Hit {
                    path: e.path.clone(),
                    line: e.line,
                    preview: format!("{} {}", e.kind.glyph(), e.name),
                })
                .collect();
            self.searched = index.len();
            return;
        }

        let Some(matcher) = self.build_matcher() else { return };
        // Materialize the dirty-buffer overlay ONLY here, on an actual (re)launch — building
        // it per frame while the panel is open would flatten every dirty rope at repaint rate.
        let overlay = overlay();
        self.running = true;
        let seq = self.seq;
        let mask = self.mask.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("cauldron-repo-search".into())
            .spawn(move || {
                let mut sent = 0usize;
                let mut searched = 0usize;
                'files: for path in files {
                    if !mask_matches(&mask, &path) {
                        continue;
                    }
                    // Dirty-buffer overlay shadows disk: unsaved edits are searched as-is.
                    let Some(text) = content_for(&path, &overlay) else { continue };
                    searched += 1;
                    for (ln, line) in text.lines().enumerate() {
                        if matcher.matches(line) {
                            let preview = line.trim().chars().take(160).collect::<String>();
                            if tx
                                .send(Msg::Hit {
                                    seq,
                                    hit: Hit { path: path.clone(), line: ln, preview },
                                })
                                .is_err()
                            {
                                return;
                            }
                            sent += 1;
                            if sent >= MAX_RESULTS {
                                break 'files;
                            }
                        }
                    }
                    if searched.is_multiple_of(64) {
                        ctx.request_repaint();
                    }
                }
                let _ = tx.send(Msg::Done { seq, searched });
                ctx.request_repaint();
            })
            .ok();
    }

    /// Draw the overlay when open. `files` = the workspace's flat file list (excludes applied).
    /// Returns `Some((path, line))` when a hit is chosen.
    ///
    /// Back-compat wrapper: Symbol mode shows "symbol index not available" and no dirty-buffer
    /// overlay is applied until the caller switches to [`Self::ui_with_symbols`].
    #[allow(dead_code)] // kept: symbol-less call path for embedders
    pub fn ui(
        &mut self,
        ctx: &egui::Context,
        files: &[PathBuf],
        root: &std::path::Path,
    ) -> Option<(PathBuf, usize)> {
        self.ui_with_symbols(ctx, files, root, None, &|| Arc::new(HashMap::new()))
    }

    /// Same as [`Self::ui`], with the project symbol index wired for Symbol mode and a LAZY
    /// dirty-buffer overlay producer (unsaved editor contents keyed by absolute path, shadowing
    /// disk). `overlay` is only invoked on the frames a search actually (re)launches — never
    /// per repaint.
    #[allow(dead_code)] // awaiting the main.rs integrator (swap the `ui` call site)
    pub fn ui_with_symbols(
        &mut self,
        ctx: &egui::Context,
        files: &[PathBuf],
        root: &std::path::Path,
        symbols: Option<&SymbolIndex>,
        overlay: &dyn Fn() -> DirtyOverlay,
    ) -> Option<(PathBuf, usize)> {
        // Drain stream even while closed so a stale thread never backs up the channel.
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                // Only the live search's messages count — a superseded thread may still
                // be streaming hits for the previous query.
                Msg::Hit { seq, hit } => {
                    if seq == self.live_seq && self.results.len() < MAX_RESULTS {
                        self.results.push(hit);
                    }
                }
                Msg::Done { seq, searched } => {
                    if seq == self.live_seq {
                        self.running = false;
                        self.searched = searched;
                    }
                }
                Msg::ReplacePlan { seq, plan } => {
                    if seq == self.live_seq {
                        self.replacing = false;
                        self.pending_plan = Some(plan);
                    }
                }
            }
        }
        if !self.open {
            return None;
        }
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.close();
            return None;
        }
        let mut chosen: Option<(PathBuf, usize)> = None;
        let mut relaunch = false;

        egui::Area::new("reposearch".into())
            .anchor(egui::Align2::CENTER_TOP, [0.0, 64.0])
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .inner_margin(egui::Margin::same(sizes::OVERLAY_PAD))
                    .show(ui, |ui| {
                        ui.set_width(720.0);
                        ui.horizontal(|ui| {
                            crate::style::panel_header_inline(ui, "Find in Files");
                            // Mode toggle: Text / Regex / Symbol.
                            for (label, mode) in [
                                ("Text", SearchMode::Text),
                                ("Regex", SearchMode::Regex),
                                ("Symbol", SearchMode::Symbol),
                            ] {
                                if crate::style::tool_button(ui, label, self.mode == mode).clicked_by(egui::PointerButton::Primary)
                                    && self.mode != mode
                                {
                                    self.mode = mode;
                                    relaunch = true;
                                }
                            }
                            // Case-sensitivity toggle (RustRover's "Aa").
                            if crate::style::tool_button(ui, "Aa", self.case_sensitive)
                                .on_hover_text("match case")
                                .clicked_by(egui::PointerButton::Primary)
                            {
                                self.case_sensitive = !self.case_sensitive;
                                relaunch = true;
                            }
                            if self.running {
                                ui.spinner();
                            } else if !self.results.is_empty() || !self.query.is_empty() {
                                ui.colored_label(
                                    colors::TEXT_FAINT(),
                                    format!("{} hits", self.results.len()),
                                );
                            }
                        });
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut self.query)
                                .hint_text(match self.mode {
                                    SearchMode::Text => "search the whole repo (Enter)",
                                    SearchMode::Regex => "regex over the whole repo (Enter)",
                                    SearchMode::Symbol => "symbol name (Enter)",
                                })
                                .desired_width(f32::INFINITY)
                                .font(egui::TextStyle::Monospace),
                        );
                        if self.just_opened {
                            resp.request_focus();
                            self.just_opened = false;
                        }
                        // File mask row.
                        let mut mask_focused = false;
                        ui.horizontal(|ui| {
                            ui.colored_label(colors::TEXT_FAINT(), "mask");
                            let mask_resp = ui.add(
                                egui::TextEdit::singleline(&mut self.mask)
                                    .hint_text("*.c;*.h (empty = all files)")
                                    .desired_width(220.0)
                                    .font(egui::TextStyle::Monospace),
                            );
                            mask_focused = mask_resp.has_focus();
                            // lost_focus, not has_focus: Enter in a singleline TextEdit SURRENDERS
                            // focus before this runs, so has_focus was false on exactly the Enter
                            // frame — the launch never fired and the Enter fell through to "pick",
                            // jumping to a stale result. Keep focus so the query can be refined.
                            if mask_resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                                relaunch = true;
                                mask_resp.request_focus();
                                mask_focused = true;
                            }
                        });
                        // Replace row (Text/Regex only): replacement text + the two-step apply.
                        let mut replace_focused = false;
                        if self.mode != SearchMode::Symbol {
                            ui.horizontal(|ui| {
                                ui.colored_label(colors::TEXT_FAINT(), "replace");
                                let rep = ui.add(
                                    egui::TextEdit::singleline(&mut self.replace)
                                        .hint_text(if self.mode == SearchMode::Regex {
                                            "replacement ($1 = group)"
                                        } else {
                                            "replacement"
                                        })
                                        .desired_width(220.0)
                                        .font(egui::TextStyle::Monospace),
                                );
                                replace_focused = rep.has_focus() || rep.lost_focus();
                                let can = !self.replacing && !self.query.trim().is_empty();
                                if ui
                                    .add_enabled(can, egui::Button::new("Replace All…").small())
                                    .on_hover_text("Compute matches, then confirm before anything changes")
                                    .clicked_by(egui::PointerButton::Primary)
                                {
                                    self.launch_replace(files.to_vec(), overlay, ui.ctx());
                                }
                                if self.replacing {
                                    ui.spinner();
                                }
                            });
                            if let Some(plan) = &self.pending_plan {
                                let occurrences: usize = plan.iter().map(|(_, _, c)| c).sum();
                                let n_files = plan.len();
                                ui.horizontal(|ui| {
                                    if n_files == 0 {
                                        ui.colored_label(colors::TEXT_FAINT(), "nothing to replace");
                                        if ui.button("OK").clicked_by(egui::PointerButton::Primary) {
                                            self.pending_plan = None;
                                        }
                                    } else {
                                        ui.colored_label(
                                            colors::AMBER(),
                                            format!(
                                                "Replace {occurrences} occurrence{} in {n_files} file{}?",
                                                if occurrences == 1 { "" } else { "s" },
                                                if n_files == 1 { "" } else { "s" },
                                            ),
                                        );
                                        if ui.button("Replace").clicked_by(egui::PointerButton::Primary) {
                                            let plan = self.pending_plan.take().unwrap();
                                            self.confirmed_plan = Some(
                                                plan.into_iter().map(|(p, t, _)| (p, t)).collect(),
                                            );
                                        }
                                        if ui.button("Cancel").clicked_by(egui::PointerButton::Primary) {
                                            self.pending_plan = None;
                                        }
                                    }
                                });
                            }
                        }
                        if let Some(err) = &self.error {
                            ui.colored_label(colors::ERROR(), err);
                        }
                        // lost_focus for the same reason as the mask field above.
                        if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                            relaunch = true;
                            resp.request_focus();
                        }
                        if relaunch {
                            self.launch(files.to_vec(), overlay, ui.ctx(), symbols);
                        }

                        let shown = self.results.len();
                        if shown > 0 {
                            if ui.input(|i| i.key_pressed(Key::ArrowDown)) {
                                self.selected = (self.selected + 1) % shown;
                            }
                            if ui.input(|i| i.key_pressed(Key::ArrowUp)) {
                                self.selected = (self.selected + shown - 1) % shown;
                            }
                            // Enter picks only when neither text field owns it and this frame
                            // didn't just relaunch (a relaunch Enter must never also pick).
                            if !resp.has_focus()
                                && !mask_focused
                                && !replace_focused
                                && !relaunch
                                && ui.input(|i| i.key_pressed(Key::Enter))
                            {
                                let h = &self.results[self.selected];
                                chosen = Some((h.path.clone(), h.line));
                            }
                        }

                        ui.add_space(4.0);
                        crate::style::hairline(ui);
                        egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                            for (i, h) in self.results.iter().enumerate() {
                                let sel = i == self.selected;
                                let rel = h.path.strip_prefix(root).unwrap_or(&h.path);
                                let mut job = egui::text::LayoutJob::default();
                                let font = egui::TextStyle::Monospace.resolve(ui.style());
                                job.append(
                                    &format!("{}:{}  ", rel.display(), h.line + 1),
                                    0.0,
                                    egui::TextFormat {
                                        font_id: font.clone(),
                                        color: if sel { colors::ACCENT_HI() } else { colors::AMBER() },
                                        ..Default::default()
                                    },
                                );
                                job.append(
                                    &h.preview,
                                    0.0,
                                    egui::TextFormat {
                                        font_id: font,
                                        color: colors::TEXT_MUTED(),
                                        ..Default::default()
                                    },
                                );
                                if ui.selectable_label(sel, job).clicked_by(egui::PointerButton::Primary) {
                                    chosen = Some((h.path.clone(), h.line));
                                }
                            }
                            if !self.running && self.results.is_empty() && !self.query.is_empty() {
                                ui.colored_label(colors::TEXT_FAINT(), "no matches");
                            }
                        });
                    });
            });

        if chosen.is_some() {
            self.close();
        }
        chosen
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn glob_mask_matching() {
        assert!(glob_match("*.c", "main.c"));
        assert!(!glob_match("*.c", "main.cpp"));
        assert!(glob_match("*.C", "MAIN.c")); // case-insensitive
        assert!(glob_match("te?t.rs", "test.rs"));
        assert!(!glob_match("te?t.rs", "teest.rs"));
        assert!(glob_match("*", "anything.xyz"));

        assert!(mask_matches("", Path::new("/r/src/a.py"))); // empty = all
        assert!(mask_matches("*.c;*.h", Path::new("/r/inc/x.h")));
        assert!(mask_matches("*.c, *.h", Path::new("/r/src/y.c")));
        assert!(!mask_matches("*.c;*.h", Path::new("/r/src/z.rs")));
    }

    #[test]
    fn text_matcher_respects_case_toggle() {
        let mut s = RepoSearch::default();
        s.query = "Needle".into();
        s.mode = SearchMode::Text;

        s.case_sensitive = false;
        let m = s.build_matcher().unwrap();
        assert!(m.matches("a nEEdle here"));

        s.case_sensitive = true;
        let m = s.build_matcher().unwrap();
        assert!(m.matches("a Needle here"));
        assert!(!m.matches("a needle here"));
    }

    #[test]
    fn regex_matcher_and_error_plumbing() {
        let mut s = RepoSearch::default();
        s.mode = SearchMode::Regex;
        s.query = r"fn\s+ma.n".into();
        let m = s.build_matcher().unwrap();
        assert!(m.matches("pub fn main() {"));
        assert!(!m.matches("pub fn helper() {"));

        // Case-insensitive by default, sensitive when toggled.
        s.query = "TODO".into();
        assert!(s.build_matcher().unwrap().matches("// todo: later"));
        s.case_sensitive = true;
        assert!(!s.build_matcher().unwrap().matches("// todo: later"));

        // Bad regex surfaces an error instead of a matcher.
        s.query = "([".into();
        assert!(s.build_matcher().is_none());
        assert!(s.error.as_deref().unwrap_or("").starts_with("bad regex"));
    }

    #[test]
    fn stale_thread_messages_are_ignored() {
        let mut s = RepoSearch::default();
        s.live_seq = 2; // pretend search #2 is live
        s.running = true;
        // Messages from a superseded search #1 must not land.
        s.tx.send(Msg::Hit {
            seq: 1,
            hit: Hit { path: PathBuf::from("/r/a.rs"), line: 0, preview: "old".into() },
        })
        .unwrap();
        s.tx.send(Msg::Done { seq: 1, searched: 7 }).unwrap();
        // A live hit + done must land.
        s.tx.send(Msg::Hit {
            seq: 2,
            hit: Hit { path: PathBuf::from("/r/b.rs"), line: 3, preview: "new".into() },
        })
        .unwrap();
        s.tx.send(Msg::Done { seq: 2, searched: 42 }).unwrap();

        // ui() drains the channel before touching any egui state; overlay is closed.
        let ctx = egui::Context::default();
        assert!(s.ui(&ctx, &[], Path::new("/r")).is_none());
        assert_eq!(s.results.len(), 1);
        assert_eq!(s.results[0].preview, "new");
        assert!(!s.running);
        assert_eq!(s.searched, 42);
    }

    #[test]
    fn overlay_shadows_disk_and_falls_back() {
        let dir = std::env::temp_dir().join(format!("cauldron-overlay-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let on_disk = dir.join("shadowed.rs");
        let disk_only = dir.join("disk_only.rs");
        std::fs::write(&on_disk, "stale disk content\n").unwrap();
        std::fs::write(&disk_only, "fresh from disk\n").unwrap();

        let mut overlay = HashMap::new();
        overlay.insert(on_disk.clone(), "unsaved buffer content\n".to_string());
        // Overlay also covers files that don't exist on disk yet (never-saved buffers).
        let unsaved_new = dir.join("never_saved.rs");
        overlay.insert(unsaved_new.clone(), "brand new\n".to_string());

        // Path in the overlay: overlay text wins over disk.
        assert_eq!(content_for(&on_disk, &overlay).as_deref(), Some("unsaved buffer content\n"));
        // Path not in the overlay: falls back to disk.
        assert_eq!(content_for(&disk_only, &overlay).as_deref(), Some("fresh from disk\n"));
        // Overlay entry without a disk file still searches.
        assert_eq!(content_for(&unsaved_new, &overlay).as_deref(), Some("brand new\n"));
        // Missing everywhere: skipped.
        assert!(content_for(&dir.join("nope.rs"), &overlay).is_none());
        // Oversized overlay buffers are skipped like oversized files.
        let big = dir.join("big.rs");
        overlay.insert(big.clone(), "x".repeat(MAX_FILE_BYTES as usize + 1));
        assert!(content_for(&big, &overlay).is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Replace regex: Text mode is literal (metachars escaped), Regex mode expands groups,
    /// Symbol mode never builds one.
    #[test]
    fn replace_regex_modes() {
        let mut s = RepoSearch::default();
        s.mode = SearchMode::Text;
        s.query = "a.b(".into(); // metachars must be literal in Text mode
        let re = s.build_replace_regex().unwrap();
        assert!(re.is_match("call a.b( now"));
        assert!(!re.is_match("axbc"));

        s.mode = SearchMode::Regex;
        s.query = r"fn (\w+)".into();
        let re = s.build_replace_regex().unwrap();
        assert_eq!(re.replace_all("fn foo()", "fn renamed_$1"), "fn renamed_foo()");

        // Text-mode replacement must NOT expand $1 (NoExpand at the call site).
        s.mode = SearchMode::Text;
        s.query = "x".into();
        let re = s.build_replace_regex().unwrap();
        assert_eq!(re.replace_all("x", regex::NoExpand("$1")), "$1");

        s.mode = SearchMode::Symbol;
        assert!(s.build_replace_regex().is_none());

        s.mode = SearchMode::Regex;
        s.query = "([".into();
        assert!(s.build_replace_regex().is_none());
        assert!(s.error.as_deref().unwrap_or("").starts_with("bad regex"));
    }

    /// A stale replace plan (superseded seq) is dropped; a live one lands as pending and a
    /// relaunch clears it.
    #[test]
    fn replace_plan_seq_guard_and_invalidations() {
        let mut s = RepoSearch::default();
        s.live_seq = 2;
        s.replacing = true;
        s.tx.send(Msg::ReplacePlan { seq: 1, plan: vec![(PathBuf::from("/r/old.rs"), "x".into(), 1)] })
            .unwrap();
        s.tx.send(Msg::ReplacePlan { seq: 2, plan: vec![(PathBuf::from("/r/new.rs"), "y".into(), 3)] })
            .unwrap();
        let ctx = egui::Context::default();
        assert!(s.ui(&ctx, &[], Path::new("/r")).is_none());
        assert!(!s.replacing);
        let plan = s.pending_plan.as_ref().unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].0, PathBuf::from("/r/new.rs"));

        // A new search invalidates the unconfirmed plan.
        s.query = "q".into();
        s.launch(vec![], &|| Arc::new(HashMap::new()), &ctx, None);
        assert!(s.pending_plan.is_none());
    }

    #[test]
    fn symbol_mode_builds_no_line_matcher() {
        let mut s = RepoSearch::default();
        s.mode = SearchMode::Symbol;
        s.query = "anything".into();
        assert!(s.build_matcher().is_none());
        assert!(s.error.is_none());
    }
}
