//! The interactive editor widget: viewport-virtualized painting + caret/selection + input.
//!
//! Design mirrors JetBrains editor behavior on top of the [`Buffer::apply`] chokepoint:
//! - **Virtualized paint.** Only the visible line range is laid out — one `LayoutJob`/galley per
//!   visible line (never the whole file), colored by [`highlight`].
//! - **Geometry from the galley, never arithmetic.** Caret x, selection rects, and mouse
//!   hit-testing all go through the per-line galley (`pos_from_ccursor` / `cursor_from_pos`), so
//!   non-ASCII and per-glyph fallback stay correct.
//! - **Every edit is a [`Transaction`].** Typing, delete, newline, paste, duplicate — all build one
//!   transaction (a `Change` per caret), route through `Buffer::apply`, feed the incremental
//!   tree-sitter reparse, and reposition every caret by the accumulated byte delta.
//! - **Rainbow brackets.** Nesting depth is document-global (everything above the viewport
//!   contributes), so per-line jobs read a whole-buffer [`BracketIndex`] rebuilt lazily per edit
//!   generation — an unchanged frame does zero bracket work.
//!
//! Multi-caret is first-class (the model is [`Selections`]); the add-caret gestures come next.

use std::ops::Range;
use std::sync::Arc;

use egui::text::{CCursor, LayoutJob};
use egui::{Color32, FontId, Key, Pos2, Rect, TextFormat, Vec2};
use ropey::Rope;

use crate::buffer::{Buffer, Change, EditKind, EditMeta, SelectionSnapshot, Transaction};
use crate::highlight::{bracket_color, color, BracketIndex, Highlighter, HighlightKind};
use crate::selection::{self, Motion, Selection, Selections};
use crate::syntax::{Lang, Syntax};

// Editor chrome colors — theme-aware (dark, light) via crate::theme. Functions, not consts,
// so a theme flip re-paints without touching any per-buffer state. Premultiplied colors need
// components ≤ alpha — out-of-gamut values render saturated (the "white bar" bug).
#[inline]
fn TEXT() -> Color32 {
    crate::theme::pick(Color32::from_rgb(238, 235, 232), Color32::from_rgb(30, 28, 26))
}
#[inline]
fn GUTTER() -> Color32 {
    crate::theme::pick(Color32::from_rgb(233, 110, 44), Color32::from_rgb(214, 92, 26))
}
#[inline]
fn CARET() -> Color32 {
    crate::theme::pick(Color32::from_rgb(233, 110, 44), Color32::from_rgb(214, 92, 26))
}
/// Selection fill: warm amber wash (dark bg) / soft orange tint (light bg).
#[inline]
fn SEL() -> Color32 {
    crate::theme::pick(
        Color32::from_rgba_premultiplied(50, 24, 9, 55),
        Color32::from_rgba_premultiplied(214, 92, 26, 40),
    )
}
/// Current-line band: barely-there lift (white on dark, black on light).
#[inline]
fn CURLINE() -> Color32 {
    crate::theme::pick(
        Color32::from_rgba_premultiplied(7, 7, 7, 7),
        Color32::from_rgba_premultiplied(0, 0, 0, 8),
    )
}
#[inline]
fn MATCH_BRACKET_FILL() -> Color32 {
    crate::theme::pick(
        Color32::from_rgba_premultiplied(30, 15, 5, 40),
        Color32::from_rgba_premultiplied(214, 92, 26, 30),
    )
}
#[inline]
fn MATCH_BRACKET_STROKE() -> Color32 {
    crate::theme::pick(Color32::from_rgb(120, 84, 52), Color32::from_rgb(196, 120, 70))
}
/// Inline debug-value annotation: a calm sage-green so it reads as data, distinct from the
/// orange gutter (LSP inlay hints) and the faint blame note.
fn DEBUG_VALUE() -> Color32 {
    crate::theme::pick(Color32::from_rgb(140, 176, 130), Color32::from_rgb(70, 120, 66))
}

/// Extra vertical padding added to the font row height.
const LINE_PAD: f32 = 2.0;
/// Left padding inside the text column, and the sliver drawn for a selected newline.
const TEXT_PAD: f32 = 6.0;
/// Spaces inserted for a Tab (soft tabs; cFS/Rust both indent with spaces).
const TAB: &str = "    ";

/// One visible line's paint geometry, retained for caret/selection drawing + hit-testing.
struct LineGeom {
    line: usize,
    top: f32,
    galley: Arc<egui::Galley>,
    line_char_start: usize,
    content_chars: usize,
}

/// An open buffer's editor: text-engine highlight pipeline + the live selection set.
/// One caret's Ctrl+W ladder.
#[derive(Debug, Clone)]
struct ExpandStack {
    ranges: Vec<Range<usize>>,
    at: usize,
}

/// One pending AI ghost completion.
struct Ghost {
    byte: usize,
    generation: u64,
    text: String,
}

pub struct EditorView {
    /// Grammar for this buffer — kept so undo/redo can rebuild the tree (see [`EditorView::undo`]).
    lang: Option<Lang>,
    syntax: Option<Syntax>,
    highlighter: Option<Highlighter>,
    selections: Selections,
    font_size: f32,
    /// Running max content width (grows as wide lines scroll into view) for the h-scroll extent.
    max_width: f32,
    /// Anchor byte of an in-progress Alt+drag column selection.
    column_anchor: Option<usize>,
    find: FindState,
    /// One-shot vertical scroll target (px) — set by find navigation, applied next frame.
    pending_scroll: Option<f32>,
    /// Row height of the last painted frame (for scroll-target math outside the paint pass).
    last_row_h: f32,
    /// Vertical scroll offset (px) observed on the last painted frame — test/debug introspection.
    last_scroll_y: f32,
    /// Whether the editor widget held egui keyboard focus on the last painted frame. The app
    /// gates global key reads (completion popup Tab/arrows) on this so keystrokes aimed at the
    /// terminal / find bar / overlays never act on the editor behind them.
    focused: bool,
    /// One-shot: grab keyboard focus on the next painted frame (set when a takeover like the
    /// diff viewer closes, so typing works immediately without a click).
    focus_pending: bool,
    /// Stable egui id for the text-area widget, unique per EditorView. egui's auto-ids bake in an
    /// allocation COUNTER from every widget emitted earlier in the frame — so a conditional row
    /// above the editor (breadcrumbs appearing/vanishing as the caret moves scope, the find bar)
    /// silently changed the editor's id, and egui's focus dead-man's-switch dropped keyboard focus
    /// because the previously-focused id was never re-registered ("click a lone } and the caret
    /// disappears"). Interacting with this fixed id makes focus immune to layout-order churn.
    widget_id: egui::Id,
    /// Byte offset under the mouse this frame (None when the pointer is off the text). The app
    /// uses it for LSP hover ("code lens" popups).
    hover_byte: Option<usize>,
    /// Screen rect of the TEXT ROW under the mouse this frame. The app anchors the hover popup
    /// below this rather than beside the pointer, so the tooltip clears the line it describes
    /// instead of sitting on top of it.
    hover_row_rect: Option<egui::Rect>,
    /// Minimap line model, rebuilt lazily per buffer generation.
    mini: MiniModel,
    /// Minimap width (drag its left edge to resize).
    mini_w: f32,
    /// Byte of a Ctrl+Click this frame (goto-definition), consumed by the app.
    ctrl_click: Option<usize>,
    /// Byte of a right-click this frame (the app opens the editor context menu there).
    context_click: Option<usize>,
    /// Primary caret's on-screen position last frame (completion popup anchor).
    caret_pos: Option<(f32, f32)>,
    /// Breakpoint lines (0-based) painted as red dots in the gutter.
    pub breakpoint_lines: Vec<usize>,
    /// Test-declaration lines (0-based, sorted): the gutter paints a run ▶ there, and a gutter
    /// click on one RUNS the test instead of toggling a breakpoint. Set by the app.
    pub test_lines: Vec<usize>,
    /// Bookmarked lines (0-based, sorted): a small accent flag in the gutter. Set by the app.
    pub bookmark_lines: Vec<usize>,
    /// Gutter click on a test line → the app runs that test (0-based line).
    test_click: Option<usize>,
    /// Line whose PAUSED debug frame is highlighted (0-based).
    pub debug_line: Option<usize>,
    /// Gutter click (0-based line) → the app toggles a breakpoint.
    gutter_click: Option<usize>,
    /// AI inline (ghost) completion pinned to a caret byte + buffer generation. Cleared the
    /// moment either drifts. Tab accepts, Esc dismisses.
    ghost: Option<Ghost>,
    /// Ctrl+Shift+F7: byte spans of the identifier being highlighted across this file, plus the
    /// generation they were computed against. Painted like find matches but in a distinct wash,
    /// and cleared by Escape or by any edit.
    usage_marks: Option<(u64, Vec<Range<usize>>)>,
    /// Ctrl+W history: `(buffer generation, one range stack per caret)`, innermost first with
    /// `at` marking the current rung. NOT invalidated from the many caret-moving paths — that
    /// would be a bug farm. Instead [`Self::expand_valid`] checks that every caret still sits on
    /// the rung it claims, which any other motion or edit breaks by construction.
    expand: Option<(u64, Vec<ExpandStack>)>,
    /// A ghost was accepted with Tab this frame; the app must not also let the completion popup
    /// consume that same Tab. Drained by [`Self::take_ghost_accepted`].
    ghost_accepted: bool,
    /// Caret blink: the time of the last caret movement/edit (blink restarts VISIBLE there,
    /// JetBrains-style) + the head position that stamp belongs to.
    blink_epoch: f64,
    blink_head: usize,
    /// Active snippet session: Tab-ordered ABSOLUTE byte ranges + current position. Ranges
    /// are remapped through every transaction; undo/redo ends the session.
    snippet: Option<(Vec<Range<usize>>, usize)>,
    /// A MODAL overlay (bookmarks list) owns Enter/arrows/Tab/Esc outright: the editor drops
    /// them entirely while set. Distinct from the completion-popup suppression flags — this one
    /// steals Enter too (the overlay's Enter-to-jump must never also insert a newline).
    pub modal_keys_stolen: bool,
    /// While the app's completion popup is open it owns ↑/↓/Enter/Tab/Esc.
    pub suppress_nav_keys: bool,
    /// Enter belongs to the completion popup ONLY when it will accept it (the user arrowed into the
    /// list). Distinct from [`Self::suppress_nav_keys`] so a bare Enter under an un-navigated
    /// auto-popup still makes a newline.
    pub suppress_enter: bool,
    /// Coverage marks: (0-based line, covered) painted as a thin bar beside the git bar.
    /// Set by the app after a coverage run; empty = no bar.
    coverage_marks: Vec<(usize, bool)>,
    /// Git gutter marks: (0-based line, kind) — 0 added · 1 modified · 2 deletion-below. Sorted.
    gutter_marks: Vec<(usize, u8)>,
    /// Applied edits queued for external consumers (the LSP client derives incremental didChange
    /// from these). Each entry: the rope BEFORE the transaction (O(1) Arc-shared clone) + the
    /// transaction as applied. The view stays LSP-ignorant — the app drains via [`Self::take_edits`].
    edits_out: Vec<(Rope, Transaction)>,
    /// Diagnostics to paint (byte ranges, sorted by start). Set by the app via
    /// [`Self::set_diagnostics`]; mapped/replaced externally as edits and publishes arrive.
    diagnostics: Vec<ViewDiag>,
    /// Whole-buffer rainbow-bracket index, rebuilt lazily per buffer generation (one linear rope
    /// pass, sub-ms at ~5k lines). Never touched by the apply/reparse keystroke path.
    brackets: BracketIndex,
    /// The `(bracket, its match)` byte offsets to emphasize this frame — the pair the caret sits
    /// against. Recomputed each paint from [`Self::brackets`]; `None` when the caret is not on a
    /// matched bracket. Also the jump target for [`Self::jump_to_matching_bracket`].
    match_pair: Option<(usize, usize)>,
    /// Collapsed regions (code folding). Empty = nothing folded.
    folds: Folds,
    /// Inline git blame: `(0-based line, annotation)` painted faintly after that line's text.
    /// Set by the app (the view is git-ignorant); the app keeps it on the caret line.
    inline_blame: Option<(usize, String)>,
    /// LSP inlay hints as `(0-based line, merged label)`, sorted by line — painted dimmed
    /// after each line's text (end-of-line presentation; the galley stays hint-free so all
    /// caret/click math is untouched). App-set; may be briefly stale across edits.
    inlay_hints: Vec<(usize, String)>,
    /// Inline debug values as `(0-based line, "name = value  …")`, sorted by line. Painted at
    /// end of line while stopped in the debugger (distinct from LSP inlay hints). App-set.
    debug_values: Vec<(usize, String)>,
    /// Soft wrap (Alt+Z): long lines flow onto multiple visual rows instead of scrolling right.
    wrap: bool,
    /// Cached visual-row index for wrap mode (one doc line → N rows). Rebuilt when the buffer,
    /// wrap width, or folds change. `None`/unused when `wrap` is off.
    row_index: Option<RowIndex>,
    /// Validity key for `row_index`: (generation, wrap_cols, folds regions).
    row_index_key: Option<(u64, usize, Vec<(usize, usize)>)>,
}

/// One diagnostic squiggle: byte range + severity + message. Severity follows LSP (1 error,
/// 2 warning, 3 info, 4 hint) plus Cauldron's own: 5 = NASA/Power-of-Ten finding (the reserved
/// ORANGE, error weight) and 6 = GUARDED recursion (orchid — a cycle bounded by a recognized
/// re-entry guard: worth knowing, not alarming).
#[derive(Debug, Clone)]
pub struct ViewDiag {
    pub range: Range<usize>,
    pub severity: u8,
    pub message: String,
}

impl ViewDiag {
    fn color(&self) -> Color32 {
        match self.severity {
            1 => Color32::from_rgb(224, 82, 60),  // error — ember red
            2 => Color32::from_rgb(230, 180, 60), // warning — amber
            5 => Color32::from_rgb(233, 110, 44), // NASA/PoT — the reserved rust orange
            6 => Color32::from_rgb(180, 142, 173), // guarded recursion — orchid
            7 => Color32::from_rgb(191, 115, 100), // tooling recursion — muted salmon
            _ => TEXT().gamma_multiply(0.5),        // info/hint — quiet
        }
    }

    /// Severity for ORDERING (gutter dot, worst-first ties): NASA findings (5) carry ERROR
    /// weight — a Power-of-Ten violation outranks a warning on the same line. Lower = worse.
    pub fn rank(&self) -> u8 {
        match self.severity {
            5 => 1, // PoT violation carries error weight
            6 | 7 => 2, // guarded / tooling recursion rank with warnings
            s => s,
        }
    }
}

/// Ctrl+F / Ctrl+R find-and-replace bar state. Matches are recomputed lazily whenever
/// (buffer generation, query, case flag) changes.
#[derive(Default)]
struct FindState {
    open: bool,
    replace_open: bool,
    query: String,
    replacement: String,
    case_sensitive: bool,
    /// Treat `query` as a regular expression (the `.*` toggle) rather than a literal.
    regex: bool,
    /// The query is a regex but didn't compile — the match count area shows "bad pattern".
    bad_regex: bool,
    /// Sorted, non-overlapping byte ranges of every match.
    matches: Vec<Range<usize>>,
    /// Index into `matches` of the current (navigated-to) match, if any.
    current: Option<usize>,
    computed_for: Option<(u64, String, bool, bool)>,
    /// Focus the query field on the next frame (just opened / after Enter).
    focus_pending: bool,
}

impl FindState {
    /// Recompute `matches` if the buffer, query, or case flag changed since last time.
    fn refresh(&mut self, rope: &Rope, generation: u64) {
        let key = (generation, self.query.clone(), self.case_sensitive, self.regex);
        if self.computed_for.as_ref() == Some(&key) {
            return;
        }
        self.computed_for = Some(key);
        self.matches.clear();
        self.current = None;
        self.bad_regex = false;
        if self.query.is_empty() {
            return;
        }
        let text = rope.to_string();
        if self.regex {
            // regex crate handles case-insensitivity and Unicode correctly (no ASCII lowercase
            // hack) and yields byte offsets directly. A non-compiling pattern is not an error to
            // shout about — it's a half-typed regex; just show no matches until it parses.
            let re = match regex::RegexBuilder::new(&self.query)
                .case_insensitive(!self.case_sensitive)
                .build()
            {
                Ok(re) => re,
                Err(_) => {
                    self.bad_regex = true;
                    return;
                }
            };
            let mut idx = 0;
            while let Some(m) = re.find_at(&text, idx) {
                let (s, e) = (m.start(), m.end());
                self.matches.push(s..e);
                // Zero-width matches (e.g. `^`, `\b`, `a*`) must still advance, past the current
                // char boundary, or the loop spins forever.
                idx = if e > s { e } else { next_char_boundary(&text, e) };
                if idx > text.len() {
                    break;
                }
            }
            return;
        }
        // Literal search. Case-insensitive lowercases both sides — byte-offset-safe only for
        // ASCII, so fall back to case-sensitive when either side is non-ASCII (documented limit).
        let insensitive = !self.case_sensitive && text.is_ascii() && self.query.is_ascii();
        let (hay, needle) = if insensitive {
            (text.to_ascii_lowercase(), self.query.to_ascii_lowercase())
        } else {
            (text, self.query.clone())
        };
        let mut idx = 0;
        while let Some(rel) = hay.get(idx..).and_then(|s| s.find(&needle)) {
            let b = idx + rel;
            self.matches.push(b..b + needle.len());
            idx = b + needle.len().max(1);
        }
    }
}

/// Folded regions. Each `(header, end)` hides doc lines `header+1..=end`; the header line stays
/// visible (with a chevron). Kept sorted by `header` and strictly non-overlapping, which is what
/// makes the row<->line mapping a simple linear walk.
#[derive(Default, Clone)]
struct Folds {
    regions: Vec<(usize, usize)>,
}

impl Folds {
    fn is_header(&self, line: usize) -> bool {
        self.regions.iter().any(|(h, _)| *h == line)
    }

    /// A hidden line: strictly inside some fold's collapsed range (never the header itself).
    fn is_hidden(&self, line: usize) -> bool {
        self.regions.iter().any(|(h, e)| line > *h && line <= *e)
    }

    /// Count of hidden lines strictly before `line`.
    fn hidden_before(&self, line: usize) -> usize {
        self.regions
            .iter()
            .map(|(h, e)| {
                if *h >= line {
                    0
                } else {
                    (*e).min(line.saturating_sub(1)).saturating_sub(*h)
                }
            })
            .sum()
    }

    /// Visible row a doc line paints on (a hidden line collapses onto its header's row).
    fn line_to_row(&self, line: usize) -> usize {
        line - self.hidden_before(line)
    }

    /// Total visible rows for a `total`-line document.
    fn total_rows(&self, total: usize) -> usize {
        total.saturating_sub(self.regions.iter().map(|(h, e)| e - h).sum::<usize>())
    }

    /// The doc line shown at visible `row`. Inverse of [`Self::line_to_row`] over visible lines;
    /// the regions being sorted + disjoint makes this an ascending walk.
    fn row_to_line(&self, row: usize, total: usize) -> usize {
        let mut line = row;
        for (h, e) in &self.regions {
            if *h < line {
                line += e - h;
            } else {
                break;
            }
        }
        line.min(total.saturating_sub(1))
    }

    /// Toggle a fold anchored at `header` spanning through `end`. Unfolds if `header` is already a
    /// fold header; ignores a header that sits hidden inside another fold; otherwise adds it and
    /// drops any now-subsumed inner folds so regions stay disjoint.
    fn toggle(&mut self, header: usize, end: usize) {
        if let Some(i) = self.regions.iter().position(|(h, _)| *h == header) {
            self.regions.remove(i);
            return;
        }
        if end <= header || self.is_hidden(header) {
            return;
        }
        self.regions.retain(|(h, _)| !(*h > header && *h <= end));
        self.regions.push((header, end));
        self.regions.sort_by_key(|(h, _)| *h);
    }

    /// Drop folds invalidated by an edit that changed the line count (ranges past the end, or a
    /// header that no longer opens a multi-line scope is simply cleared — conservative but safe).
    fn clamp(&mut self, total: usize) {
        self.regions.retain(|(h, e)| *h < total && *e < total && h < e);
    }

    /// Shift fold line numbers through an edit so a fold keeps hiding the SAME lines after inserts/
    /// deletes above it (folds are anchored to absolute line numbers). `pre` is the rope BEFORE the
    /// transaction. Rule per fold: an edit strictly above the header slides the whole fold by the
    /// net line delta; an edit inside/at the fold is ambiguous, so the fold is dropped (it just
    /// opens); an edit below leaves it untouched.
    fn remap(&mut self, pre: &Rope, tx: &Transaction) {
        if self.regions.is_empty() || tx.changes.is_empty() {
            return;
        }
        let len = pre.len_bytes();
        let edit_line = tx
            .changes
            .iter()
            .map(|c| pre.byte_to_line(c.start.min(len)))
            .min()
            .unwrap_or(0);
        let delta: isize = tx
            .changes
            .iter()
            .map(|c| {
                let removed = pre
                    .byte_slice(c.start.min(len)..c.end.min(len))
                    .chars()
                    .filter(|ch| *ch == '\n')
                    .count() as isize;
                let added = c.text.chars().filter(|ch| *ch == '\n').count() as isize;
                added - removed
            })
            .sum();
        if delta == 0 {
            return;
        }
        self.regions.retain_mut(|(h, e)| {
            if edit_line < *h {
                *h = (*h as isize + delta).max(0) as usize;
                *e = (*e as isize + delta).max(0) as usize;
                true
            } else if edit_line <= *e {
                false // edit touches the fold — drop it (opens)
            } else {
                true // edit below the fold — unchanged
            }
        });
        self.regions.sort_by_key(|(h, _)| *h);
    }
}

/// Visual-row index for soft wrap: a prefix sum of each doc line's wrapped-row count (0 for a
/// folded-hidden line). `starts[line]` is the first visual row of `line`; `starts[total]` is the
/// grand total. This is what makes wrap-mode virtualization + caret/click math a lookup, not a
/// per-frame layout of the whole file.
struct RowIndex {
    starts: Vec<usize>,
}

impl RowIndex {
    fn build(rope: &Rope, folds: &Folds, wrap_cols: usize) -> Self {
        let total = rope.len_lines();
        let mut starts = Vec::with_capacity(total + 1);
        let mut acc = 0;
        for line in 0..total {
            starts.push(acc);
            acc += rows_of(rope, folds, wrap_cols, line);
        }
        starts.push(acc);
        RowIndex { starts }
    }

    fn total_rows(&self) -> usize {
        *self.starts.last().unwrap_or(&0)
    }

    fn line_to_row(&self, line: usize) -> usize {
        self.starts[line.min(self.starts.len().saturating_sub(1))]
    }

    /// Doc line owning visual `row` (the last line whose first row is <= row).
    fn row_to_line(&self, row: usize) -> usize {
        // starts is non-decreasing; hidden lines repeat a value (0 width) — partition_point lands
        // on the line that actually paints at/after this row's start.
        self.starts.partition_point(|&s| s <= row).saturating_sub(1)
    }
}

/// Wrapped-row count for one doc line at `wrap_cols` columns: 0 if folded away, else at least 1.
fn rows_of(rope: &Rope, folds: &Folds, wrap_cols: usize, line: usize) -> usize {
    if folds.is_hidden(line) {
        return 0;
    }
    let w = wrap_cols.max(1);
    // ceil(cols / w), but never below 1 — an empty line still owns one row.
    ((display_cols(rope, line) + w - 1) / w).max(1)
}

/// Monospace column advance of ONE char, matching how egui/epaint advances glyphs so our
/// wrap-row count and caret-follow column line up with the galley: a flat 4 for tab (epaint
/// gives every tab a 4-space advance, NOT tab-stop snapping), 0 for line breaks and combining
/// marks, and the Unicode terminal width (1 or 2) otherwise — so CJK/wide glyphs count as the
/// 2 cells a monospace font actually advances them, instead of the old flat 1 that desynced
/// wrap rows on any line with wide characters.
pub(crate) fn char_cols(ch: char) -> usize {
    use unicode_width::UnicodeWidthChar as _;
    match ch {
        '\n' | '\r' => 0,
        '\t' => 4,
        _ => ch.width().unwrap_or(1),
    }
}

/// Advance width of a line in monospace columns, used ONLY to size wrapped rows (rows_of/RowIndex).
fn display_cols(rope: &Rope, line: usize) -> usize {
    rope.line(line).chars().map(char_cols).sum()
}

impl EditorView {
    /// Build a view for `buffer`, guessing the grammar from `path`. Unknown extensions still edit
    /// fine — they just paint uncolored.
    pub fn new(buffer: &Buffer, path: &str) -> Self {
        let lang = Lang::from_path(path);
        let syntax = lang.and_then(|l| Syntax::new(l, buffer.rope()));
        let highlighter = syntax.as_ref().and(lang).and_then(Highlighter::new);
        Self {
            lang,
            syntax,
            highlighter,
            selections: Selections::single(0),
            font_size: 14.0,
            max_width: 0.0,
            column_anchor: None,
            find: FindState::default(),
            pending_scroll: None,
            last_row_h: 18.0,
            last_scroll_y: 0.0,
            focused: false,
            focus_pending: false,
            widget_id: {
                static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                egui::Id::new((
                    "cauldron-editor-view",
                    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                ))
            },
            hover_byte: None,
            hover_row_rect: None,
            mini: MiniModel::default(),
            mini_w: 84.0,
            ctrl_click: None,
            context_click: None,
            caret_pos: None,
            ghost: None,
            expand: None,
            usage_marks: None,
            ghost_accepted: false,
            blink_epoch: 0.0,
            blink_head: 0,
            snippet: None,
            breakpoint_lines: Vec::new(),
            test_lines: Vec::new(),
            test_click: None,
            bookmark_lines: Vec::new(),
            debug_line: None,
            gutter_click: None,
            modal_keys_stolen: false,
            suppress_nav_keys: false,
            suppress_enter: false,
            gutter_marks: Vec::new(),
            coverage_marks: Vec::new(),
            edits_out: Vec::new(),
            diagnostics: Vec::new(),
            brackets: BracketIndex::default(),
            match_pair: None,
            folds: Folds::default(),
            inline_blame: None,
            inlay_hints: Vec::new(),
            debug_values: Vec::new(),
            wrap: false,
            row_index: None,
            row_index_key: None,
        }
    }

    /// Paint + drive the editor for one frame.
    pub fn ui(&mut self, ui: &mut egui::Ui, buffer: &mut Buffer) {
        // Captured before the painting closure borrows `self`.
        let buf_generation = buffer.generation;
        let font = FontId::monospace(self.font_size);
        let row_h = ui.fonts(|f| f.row_height(&font)) + LINE_PAD;
        self.last_row_h = row_h;

        if self.find.open {
            self.find_bar_ui(ui, buffer);
        }

        // Ghost survives only while the caret sits exactly where it was offered.
        if let Some(g) = &self.ghost {
            let sel = self.selections.primary();
            if g.generation != buffer.generation
                || sel.head != g.byte
                || !sel.is_empty()
                || self.selections.ranges.len() != 1
            {
                self.ghost = None;
            }
        }

        // --- breadcrumbs: scope chain at the caret (clickable) ---------------------------------
        if let Some(syn) = self.syntax.as_ref() {
            let caret = self.selections.primary().head.min(buffer.rope().len_bytes());
            let crumbs = syn.scopes_at(buffer.rope(), caret);
            // The row is ALWAYS emitted (empty placeholder at top level) so its height is
            // constant: a row that appears/vanishes as the caret crosses scope depths shifted the
            // whole editor by one row height on click — perceived as an unprovoked auto-scroll.
            {
                let mut jump: Option<usize> = None;
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    ui.add_space(6.0);
                    if crumbs.is_empty() {
                        // Same font size as a crumb name → identical row height.
                        ui.label(egui::RichText::new(" ").size(11.5));
                    }
                    for (i, c) in crumbs.iter().enumerate() {
                        if i > 0 {
                            ui.label(
                                egui::RichText::new("›").size(11.0).color(Color32::from_gray(95)),
                            );
                        }
                        let resp = ui.add(
                            egui::Label::new(
                                egui::RichText::new(&c.name)
                                    .size(11.5)
                                    .color(Color32::from_gray(150)),
                            )
                            .sense(egui::Sense::click()),
                        );
                        if resp.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        // Real pointer clicks only: Sense::click widgets are focusable, and egui
                        // fakes a primary click on the focused widget when Space/Enter is pressed —
                        // a crumb holding focus would turn Space into a goto-definition jump.
                        if resp.clicked_by(egui::PointerButton::Primary) {
                            jump = Some(c.def_byte);
                        }
                    }
                });
                if let Some(b) = jump {
                    let rope = buffer.rope().clone();
                    self.selections.set_single(b);
                    buffer.seal();
                    self.jump_to(b, &rope);
                }
            }
        }

        // Soft wrap flows lines to the viewport width, so there's nothing to scroll horizontally.
        let mut scroll = if self.wrap {
            egui::ScrollArea::vertical()
        } else {
            egui::ScrollArea::both()
        }
        .auto_shrink([false, false])
        // The editor owns click+drag (caret placement, text selection). Without this, egui's
        // drag-to-scroll claims the press and the view lurches when you click.
        .drag_to_scroll(false);
        if let Some(y) = self.pending_scroll.take() {
            scroll = scroll.vertical_scroll_offset(y);
        }
        let char_w = ui.fonts(|f| f.glyph_width(&font, 'm')).max(1.0);
        scroll.show_viewport(ui, |ui, viewport| {
            self.last_scroll_y = viewport.min.y;
            let total = buffer.rope().len_lines();
            // Folds can be invalidated by edits that changed the line count — drop stale ones
            // before any geometry depends on them.
            self.folds.clamp(total);
            let digits = ((total.max(1)) as f64).log10().floor() as usize + 1;
            // The gutter ends in a narrow fold strip (chevrons); the line number sits left of it.
            let fold_w = self.font_size;
            let gutter = (digits.max(3) as f32) * self.font_size * 0.62 + 14.0 + fold_w;

            // Wrap column count from the width left for text (gutter + pads + minimap removed).
            // wrap_px is an EXACT multiple of char_w so egui's break boundary matches our column
            // math — the caret/click galley coordinates then line up per visual sub-row.
            let text_avail = ui.available_width() - gutter - TEXT_PAD * 2.0 - self.mini_w;
            let wrap_cols = (text_avail / char_w).floor().max(1.0) as usize;
            let wrap_px = wrap_cols as f32 * char_w;
            self.refresh_row_index(buffer.rope(), buffer.generation, wrap_cols);
            let total_rows = self.total_rows(total);

            // Reserve the full virtual content rect; only the visible slice is painted below.
            // Height is in VISIBLE rows (folds collapse to 0, wrapped lines expand to N). One row
            // of slack past the end: the rect is allocated BEFORE this frame's edit runs, so on the
            // frame you press Enter the line count is still the old one — without the slack, the
            // caret's brand-new bottom line falls outside the content and scroll_to_rect can't
            // reveal it (and next frame the caret hasn't moved, so the follow never re-fires and the
            // view looks stuck). The slack also just lets you scroll one line past EOF, like VS Code.
            let content = Vec2::new(
                if self.wrap {
                    wrap_px + gutter + TEXT_PAD
                } else {
                    (self.max_width + gutter + TEXT_PAD).max(ui.available_width())
                },
                (total_rows + 1) as f32 * row_h,
            );
            // Allocate space senselessly, then interact under our STABLE id (see `widget_id`):
            // the auto-id a sensed allocation would get here shifts whenever a conditional row
            // above (breadcrumbs, find bar) appears or vanishes, and a shifted id silently drops
            // egui keyboard focus — the invisible-caret-after-click bug.
            let (rect, _) = ui.allocate_exact_size(content, egui::Sense::hover());
            let resp = ui.interact(rect, self.widget_id, egui::Sense::click_and_drag());
            let origin = rect.min;
            // Pointer clicks only (clicked() would also fire on egui's Space/Enter fake click —
            // harmless here since that requires focus, but keep the gate unambiguous).
            if resp.clicked_by(egui::PointerButton::Primary)
                || resp.drag_started()
                || resp.secondary_clicked()
            {
                resp.request_focus();
                // request_focus takes effect NEXT frame, and the caret only paints while focused —
                // but egui is reactive and won't render that frame on its own, so the caret would
                // stay invisible until the next mouse move. Force the follow-up frame so the caret
                // (and this click's new position) shows immediately.
                ui.ctx().request_repaint();
            }
            if std::mem::take(&mut self.focus_pending) {
                resp.request_focus();
                ui.ctx().request_repaint(); // focus lands next frame — paint it
            }
            let focused = resp.has_focus();
            self.focused = focused;
            if focused {
                // Tab belongs to the EDITOR (indent / ghost-completion accept), never to egui's
                // widget focus traversal; same for arrows and Esc.
                ui.memory_mut(|m| {
                    m.set_focus_lock_filter(
                        resp.id,
                        egui::EventFilter {
                            tab: true,
                            horizontal_arrows: true,
                            vertical_arrows: true,
                            escape: true,
                        },
                    )
                });
            }

            // 1) Input that only touches text/selection (no geometry needed) — before paint so the
            //    galleys we lay out reflect this frame's edits.
            let head_before = self.selections.primary().head;
            if focused {
                self.handle_keys(ui, buffer);
            }
            // Keyboard input that moved the caret must bring it back into view — typing, Enter,
            // arrows, paste. egui's ScrollArea only follows real widgets, not our painted caret, so
            // this is what makes "press Enter → the caret is on the new line, on screen" actually
            // happen. scroll_to_rect with no alignment scrolls the MINIMUM needed (a no-op when the
            // caret is already visible), so it never fights manual scrolling.
            if focused {
                let rope = buffer.rope();
                let head = self.selections.primary().head.min(rope.len_bytes());
                if head != head_before.min(rope.len_bytes()) {
                    let cline = rope.byte_to_line(head);
                    // Moving the caret into a folded region reveals it (JetBrains behaviour).
                    if self.folds.is_hidden(cline) {
                        self.folds.regions.retain(|(h, e)| !(cline > *h && cline <= *e));
                    }
                    // Horizontal follow needs the caret's x. The paint pass (which builds exact
                    // galley geometry) hasn't run yet, and the caret's line may not even be painted
                    // this frame (it just moved off-screen). But the font is monospace, so estimate
                    // x from the column: tab counts as one advance — a small skew on tab-indented
                    // lines, never a jump. Crucially, we target the caret's REAL x, not origin.x —
                    // pinning the target to the left edge is what yanked the view left every
                    // keystroke on any line wider than the viewport.
                    let line_start = rope.line_to_byte(cline);
                    // char_cols matches display_cols / the galley advance (tab=4, CJK/wide=2), so
                    // the wrap sub-row (col / wrap_cols) lines up with where the text wrapped —
                    // even on lines with wide characters.
                    let col = rope
                        .byte_slice(line_start..head)
                        .chars()
                        .map(char_cols)
                        .sum::<usize>();
                    let text_left = origin.x + gutter + TEXT_PAD;
                    // Under wrap, the caret's column splits into a sub-row + in-row column.
                    let (crow, ccol) = if self.wrap {
                        (self.line_to_row(cline) + col / wrap_cols, col % wrap_cols)
                    } else {
                        (self.line_to_row(cline), col)
                    };
                    let cy = origin.y + crow as f32 * row_h;
                    let cx = text_left + ccol as f32 * char_w;
                    // Only scroll if the caret's cell is ACTUALLY outside the viewport. `viewport`
                    // is in content coords; convert the caret to the same space. Deciding
                    // visibility ourselves (instead of trusting scroll_to_rect(None) to no-op)
                    // guarantees typing/clicking on an already-visible caret NEVER nudges the view.
                    let cy_c = crow as f32 * row_h;
                    let cx_c = (gutter + TEXT_PAD) + ccol as f32 * char_w;
                    let v_visible = cy_c >= viewport.min.y && cy_c + row_h <= viewport.max.y;
                    // The minimap occupies the rightmost `mini_w` px (the text layout reserves it
                    // above) — so the usable right edge is `max.x - mini_w`, not `max.x`. Without
                    // this a caret on a long line reads as "visible" while sitting UNDER the
                    // minimap, and horizontal-follow never scrolls it into the clear.
                    let mini_reserve = if self.wrap { 0.0 } else { self.mini_w };
                    let right_bound = viewport.max.x - mini_reserve;
                    // Wrap has no horizontal scroll, so the caret is always horizontally in view.
                    let h_visible =
                        self.wrap || (cx_c >= viewport.min.x && cx_c + char_w <= right_bound);
                    if !(v_visible && h_visible) {
                        let hmargin = char_w; // a column of slack so the caret isn't flush to the edge
                        ui.scroll_to_rect(
                            egui::Rect::from_min_max(
                                egui::pos2(cx - hmargin, cy),
                                // Reserve the minimap width on the right so the revealed caret
                                // lands clear of it, not tucked behind.
                                egui::pos2(cx + hmargin + mini_reserve, cy + row_h),
                            ),
                            None,
                        );
                    }
                }
            }

            // 2) Paint the visible line range and retain per-line geometry.
            let rope = buffer.rope();
            let total = rope.len_lines(); // may have changed under an edit
            self.folds.clamp(total);
            // handle_keys may have edited (bumping the generation) — rebuild the wrap index so this
            // frame's geometry matches the current text, not last frame's.
            self.refresh_row_index(rope, buffer.generation, wrap_cols);
            let total_rows = self.total_rows(total);
            // Viewport bounds are in visible-ROW space; widen to the doc-line window they cover so
            // the highlighter's contiguous batch and the paint loop below both have what they need.
            let first_row = ((viewport.min.y / row_h).floor() as usize).min(total_rows);
            let last_row = ((viewport.max.y / row_h).ceil() as usize + 1).min(total_rows);
            let first = self.row_to_line(first_row, total);
            let last = (self.row_to_line(last_row, total) + 1).min(total);

            // Rainbow brackets: refresh the whole-buffer depth index (no-op when the generation
            // is unchanged — this is NOT per-frame document work).
            self.brackets.refresh(rope, buffer.generation);
            self.match_pair = self.compute_match_pair(rope);

            let spans = match (self.syntax.as_ref(), self.highlighter.as_mut()) {
                (Some(syn), Some(hl)) if first < last => {
                    Some(hl.line_spans(syn, rope, first..last, buffer.generation))
                }
                _ => None,
            };

            // Gutter hover: ghost breakpoint dot + hand cursor (the affordance that says
            // "click here to set a breakpoint").
            let gutter_hover_line = ui
                .input(|i| i.pointer.hover_pos())
                .filter(|p| {
                    ui.clip_rect().contains(*p)
                        && p.x >= origin.x
                        && p.x < origin.x + gutter - 4.0
                })
                .map(|p| {
                    let row = ((p.y - origin.y) / row_h).floor() as usize;
                    self.row_to_line(row, total)
                });
            if gutter_hover_line.is_some() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            // Which hovered gutter line can be folded (one scope query — drives the ▾ affordance).
            let hover_foldable: Option<usize> = gutter_hover_line
                .filter(|l| !self.folds.is_header(*l))
                .and_then(|l| self.foldable_end(rope, l).map(|_| l));
            // The breakpoint ghost dot belongs to the breakpoint area only — the fold strip (the
            // rightmost `fold_w` of the gutter) folds on click, so showing the "set breakpoint"
            // dot there misled (dead-click / folded instead). Restrict it to gutter minus strip.
            let breakpoint_hover_line = ui
                .input(|i| i.pointer.hover_pos())
                .filter(|p| {
                    ui.clip_rect().contains(*p)
                        && p.x >= origin.x
                        && p.x < origin.x + gutter - fold_w
                })
                .map(|p| {
                    let row = ((p.y - origin.y) / row_h).floor() as usize;
                    self.row_to_line(row, total)
                });

            let painter = ui.painter();
            let mut geoms: Vec<LineGeom> = Vec::with_capacity(last.saturating_sub(first));
            for (i, line) in (first..last).enumerate() {
                if self.folds.is_hidden(line) {
                    continue; // collapsed into its fold header — no row, no paint
                }
                let top = origin.y + self.line_to_row(line) as f32 * row_h;
                // coverage bar: 3px, one step left of the git bar (moss covered / ember not).
                if !self.coverage_marks.is_empty() {
                    if let Ok(idx) =
                        self.coverage_marks.binary_search_by_key(&line, |(l, _)| *l)
                    {
                        let covered = self.coverage_marks[idx].1;
                        let x = origin.x + gutter - 7.0;
                        painter.rect_filled(
                            Rect::from_min_max(
                                Pos2::new(x, top + 1.0),
                                Pos2::new(x + 3.0, top + row_h - 1.0),
                            ),
                            1.0,
                            if covered {
                                Color32::from_rgba_premultiplied(50, 60, 42, 110)
                            } else {
                                Color32::from_rgba_premultiplied(84, 30, 22, 120)
                            },
                        );
                    }
                }
                // git gutter mark: 3px bar (added moss / modified amber) or deletion caret (red)
                let gm = self
                    .gutter_marks
                    .binary_search_by_key(&line, |(l, _)| *l)
                    .ok()
                    .map(|idx| self.gutter_marks[idx].1);
                if let Some(kind) = gm {
                    let x = origin.x + gutter - 2.0;
                    match kind {
                        2 => {
                            // deletion below this line: small red triangle at the boundary
                            painter.add(egui::Shape::convex_polygon(
                                vec![
                                    Pos2::new(x - 4.0, top + row_h - 1.0),
                                    Pos2::new(x + 3.0, top + row_h + 2.5),
                                    Pos2::new(x - 4.0, top + row_h + 6.0),
                                ],
                                Color32::from_rgb(224, 82, 60),
                                egui::Stroke::NONE,
                            ));
                        }
                        k => {
                            let color = if k == 0 {
                                Color32::from_rgb(163, 190, 140) // added — moss
                            } else {
                                Color32::from_rgb(217, 164, 65) // modified — amber
                            };
                            painter.rect_filled(
                                Rect::from_min_max(
                                    Pos2::new(x, top + 1.0),
                                    Pos2::new(x + 3.0, top + row_h - 1.0),
                                ),
                                1.0,
                                color,
                            );
                        }
                    }
                }
                if self.debug_line == Some(line) {
                    // paused frame: warm amber band under the whole line
                    painter.rect_filled(
                        Rect::from_min_max(
                            Pos2::new(origin.x, top),
                            Pos2::new(origin.x + rect.width(), top + row_h),
                        ),
                        0.0,
                        Color32::from_rgba_premultiplied(38, 26, 6, 46),
                    );
                }
                if self.bookmark_lines.binary_search(&line).is_ok() {
                    // Bookmark flag: an accent pennant right of the breakpoint slot (coexists).
                    let fx = origin.x + 14.0;
                    let fy = top + row_h * 0.5;
                    painter.add(egui::Shape::convex_polygon(
                        vec![
                            Pos2::new(fx, fy - 4.5),
                            Pos2::new(fx + 6.0, fy - 2.0),
                            Pos2::new(fx, fy + 0.5),
                        ],
                        Color32::from_rgb(233, 110, 44),
                        egui::Stroke::NONE,
                    ));
                    painter.line_segment(
                        [Pos2::new(fx, fy - 4.5), Pos2::new(fx, fy + 4.5)],
                        egui::Stroke::new(1.2, Color32::from_rgb(233, 110, 44)),
                    );
                }
                if self.breakpoint_lines.binary_search(&line).is_ok() {
                    painter.circle_filled(
                        Pos2::new(origin.x + 7.0, top + row_h * 0.5),
                        4.0,
                        Color32::from_rgb(224, 82, 60),
                    );
                } else if self.test_lines.binary_search(&line).is_ok() {
                    // Run-test affordance: a small moss ▶ (clicking the gutter here RUNS it).
                    let cx = origin.x + 7.0;
                    let cy = top + row_h * 0.5;
                    painter.add(egui::Shape::convex_polygon(
                        vec![
                            Pos2::new(cx - 3.5, cy - 4.5),
                            Pos2::new(cx - 3.5, cy + 4.5),
                            Pos2::new(cx + 4.5, cy),
                        ],
                        Color32::from_rgb(163, 190, 140),
                        egui::Stroke::NONE,
                    ));
                } else if breakpoint_hover_line == Some(line) {
                    // ghost dot: where the breakpoint would land
                    painter.circle_filled(
                        Pos2::new(origin.x + 7.0, top + row_h * 0.5),
                        4.0,
                        Color32::from_rgba_premultiplied(70, 26, 19, 80),
                    );
                }
                // gutter number (left of the fold strip)
                painter.text(
                    Pos2::new(origin.x + gutter - fold_w - 4.0, top),
                    egui::Align2::RIGHT_TOP,
                    line + 1,
                    font.clone(),
                    GUTTER().gamma_multiply(0.55),
                );
                // fold chevron in the strip, drawn as a vector triangle (no glyph-coverage risk):
                // ► collapsed header (always), ▼ when hovering a foldable line.
                let collapsed = self.folds.is_header(line);
                if collapsed || hover_foldable == Some(line) {
                    let cx = origin.x + gutter - fold_w * 0.5;
                    let cy = top + row_h * 0.5;
                    let s = (fold_w * 0.24).clamp(3.0, 6.0);
                    let pts = if collapsed {
                        vec![Pos2::new(cx - s, cy - s), Pos2::new(cx - s, cy + s), Pos2::new(cx + s, cy)]
                    } else {
                        vec![Pos2::new(cx - s, cy - s), Pos2::new(cx + s, cy - s), Pos2::new(cx, cy + s)]
                    };
                    painter.add(egui::Shape::convex_polygon(
                        pts,
                        GUTTER().gamma_multiply(0.9),
                        egui::Stroke::NONE,
                    ));
                }
                let slice = rope.line(line);
                let text = slice.to_string();
                let text = text.trim_end_matches(['\n', '\r']);
                // Brackets on this line's content bytes (absolute offsets, rebased in line_job).
                let line_start_byte = rope.line_to_byte(line);
                let line_brackets =
                    self.brackets.in_range(line_start_byte..line_start_byte + text.len());
                let mut job = match spans.as_ref() {
                    Some(batch) => line_job(text, &batch[i], line_brackets, line_start_byte, &font),
                    None => line_job(text, &[], line_brackets, line_start_byte, &font),
                };
                if self.wrap {
                    // break_anywhere (character wrap) so the row count matches display_cols/wrap_cols
                    // exactly — a word-boundary wrap would desync the caret/click sub-row math.
                    job.wrap.max_width = wrap_px;
                    job.wrap.break_anywhere = true;
                }
                let galley = ui.fonts(|f| f.layout_job(job));
                self.max_width = self.max_width.max(galley.size().x);
                let text_pos = Pos2::new(origin.x + gutter + TEXT_PAD, top);
                painter.galley(text_pos, galley.clone(), TEXT());

                // After-line annotations chain left→right: inlay hints, then blame. Both are
                // suppressed under soft wrap: the galley is multi-row there and they would
                // land mid-text (or off-screen) instead of after the line.
                let mut after_x = text_pos.x + galley.size().x;
                // Collapsed-region marker trailing the header line (ASCII — always renders).
                // Advances the chain: hints/blame used to paint OVER it.
                if self.folds.is_header(line) {
                    let r = painter.text(
                        Pos2::new(after_x + 8.0, top),
                        egui::Align2::LEFT_TOP,
                        "…",
                        font.clone(),
                        GUTTER(),
                    );
                    after_x = r.max.x;
                }
                // Inline debug values (while stopped): paint FIRST in the after-line chain —
                // they're the most relevant thing on screen during a debug stop.
                if !self.wrap {
                    let lo = self.debug_values.partition_point(|(l, _)| *l < line);
                    if let Some((l, label)) = self.debug_values.get(lo) {
                        if *l == line {
                            let r = painter.text(
                                Pos2::new(after_x + 16.0, top),
                                egui::Align2::LEFT_TOP,
                                label,
                                font.clone(),
                                DEBUG_VALUE(),
                            );
                            after_x = r.max.x;
                        }
                    }
                }
                if !self.wrap {
                    let lo = self.inlay_hints.partition_point(|(l, _)| *l < line);
                    if let Some((l, label)) = self.inlay_hints.get(lo) {
                        if *l == line {
                            let r = painter.text(
                                Pos2::new(after_x + 12.0, top),
                                egui::Align2::LEFT_TOP,
                                label,
                                font.clone(),
                                GUTTER().gamma_multiply(0.85),
                            );
                            after_x = r.max.x;
                        }
                    }
                }
                if let Some((bl, note)) = &self.inline_blame {
                    // A pending AI ghost paints at this line's EOL too — it wins (it is
                    // actionable); the note returns when the ghost resolves.
                    let ghost_here = self.ghost.as_ref().is_some_and(|g| rope.byte_to_line(g.byte) == *bl);
                    if *bl == line && !self.wrap && !ghost_here {
                        painter.text(
                            Pos2::new(after_x + 32.0, top),
                            egui::Align2::LEFT_TOP,
                            note,
                            font.clone(),
                            GUTTER().gamma_multiply(0.62),
                        );
                    }
                }
                geoms.push(LineGeom {
                    line,
                    top,
                    galley,
                    line_char_start: rope.line_to_char(line),
                    content_chars: text.chars().count(),
                });
            }

            let text_left = origin.x + gutter + TEXT_PAD;

            // --- sticky function header: when the enclosing scope's definition scrolled off the
            // top, pin its signature line over the viewport (click = jump). ------------------------
            if first > 0 && first < total {
                if let Some(syn) = self.syntax.as_ref() {
                    let first_byte = rope.line_to_byte(first.min(total - 1));
                    let sticky = syn
                        .scopes_at(rope, first_byte.min(rope.len_bytes().saturating_sub(1)))
                        .into_iter().rfind(|c| rope.byte_to_line(c.def_byte) < first && c.end_byte > first_byte);
                    if let Some(c) = sticky {
                        let def_line = rope.byte_to_line(c.def_byte);
                        let text = rope.line(def_line).to_string();
                        let text = text.trim_end().trim_start_matches([' ', '\t']).to_string();
                        let top_y = origin.y + viewport.min.y;
                        let bar = Rect::from_min_max(
                            Pos2::new(origin.x + viewport.min.x, top_y),
                            Pos2::new(origin.x + viewport.min.x + ui.clip_rect().width(), top_y + row_h + 2.0),
                        );
                        painter.rect_filled(bar, 0.0, Color32::from_rgb(22, 20, 26));
                        painter.line_segment(
                            [bar.left_bottom(), bar.right_bottom()],
                            egui::Stroke::new(1.0, Color32::from_gray(45)),
                        );
                        painter.text(
                            Pos2::new(origin.x + viewport.min.x + gutter - 6.0, top_y + 1.0),
                            egui::Align2::RIGHT_TOP,
                            def_line + 1,
                            font.clone(),
                            GUTTER().gamma_multiply(0.75),
                        );
                        painter.text(
                            Pos2::new(origin.x + viewport.min.x + gutter + TEXT_PAD, top_y + 1.0),
                            egui::Align2::LEFT_TOP,
                            &text,
                            font.clone(),
                            Color32::from_gray(175),
                        );
                        let hit = ui.interact(bar, ui.id().with("sticky-hdr"), egui::Sense::click());
                        if hit.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        if hit.clicked_by(egui::PointerButton::Primary) {
                            let row = self.line_to_row(def_line);
                            self.pending_scroll =
                                Some((row as f32 * row_h - 2.0 * row_h).max(0.0));
                        }
                    }
                }
            }

            // Hover byte for the app's LSP hover popup (works without focus).
            let hover_pt = ui
                .input(|i| i.pointer.hover_pos())
                .filter(|p| ui.clip_rect().contains(*p));
            self.hover_byte = hover_pt.map(|p| {
                let row = ((p.y - origin.y) / row_h).floor() as usize;
                let line = self.row_to_line(row, total);
                self.byte_at(&geoms, rope, line, p, text_left)
            });
            self.hover_row_rect = hover_pt.map(|p| {
                let row = ((p.y - origin.y) / row_h).floor();
                let top = origin.y + row * row_h;
                egui::Rect::from_min_max(
                    egui::pos2(p.x, top),
                    egui::pos2(p.x, top + row_h),
                )
            });
            if self.find.open && !self.find.matches.is_empty() {
                Self::paint_find_matches(&self.find, painter, &geoms, rope, text_left, row_h);
            }
            // Ctrl+Shift+F7 marks. Dropped the moment the buffer changes: the spans are byte
            // offsets into the text they were computed from, and painting them over edited text
            // would light up the wrong words.
            if let Some((gen, marks)) = &self.usage_marks {
                if *gen == buf_generation {
                    Self::paint_usage_marks(marks, painter, &geoms, rope, text_left, row_h);
                }
            }
            if !self.diagnostics.is_empty() {
                let pointer = ui.input(|i| i.pointer.hover_pos());
                if let Some(msg) =
                    self.paint_diagnostics(painter, &geoms, rope, text_left, row_h, pointer)
                {
                    egui::show_tooltip_at_pointer(
                        ui.ctx(),
                        ui.layer_id(),
                        egui::Id::new("diag-tip"),
                        |ui| ui.label(msg),
                    );
                }
            }
            // Indent guides sit UNDER the text (drawn first): faint verticals at each enclosing
            // 4-column tab stop, bridged across blank lines so a block reads as one column.
            let char_w = ui.fonts(|f| f.glyph_width(&font, 'm')).max(1.0);
            self.paint_indent_guides(painter, rope, first, last, total, text_left, origin.y, row_h, char_w);
            self.paint_ghost(painter, &geoms, rope, text_left, row_h, &font);
            self.paint_match_pair(painter, &geoms, rope, text_left, row_h);
            self.paint_selections(painter, &geoms, rope, text_left, row_h, focused);

            // Minimap overlays everything painted so far (needs &mut self for the lens drag).
            let (first_l, last_l, total_l) = (first, last, total);
            let gen = buffer.generation;
            self.paint_minimap(ui, buffer.rope(), total_l, first_l, last_l, row_h, gen);
            // After the minimap, so the stripe is never painted under it.
            self.paint_error_stripe(ui, total_l, buffer.rope());

            // 3) Pointer → selection (uses this frame's galleys; caret shows next frame). Runs
            //    regardless of focus: the focusing click also places the caret, and right-click
            //    opens the app's context menu. Keyboard stays focus-gated above.
            self.handle_pointer(ui, &resp, &geoms, buffer, origin, gutter, row_h);
        });
    }

    /// Caret x within a visible line's galley (char index clamped to the line's content).
    fn caret_x(geom: &LineGeom, cidx: usize) -> f32 {
        let idx = cidx.min(geom.content_chars);
        geom.galley.pos_from_ccursor(CCursor::new(idx)).min.x
    }

    /// Caret (x, y) within a line's galley — y is 0 on an unwrapped line, else the sub-row offset.
    fn caret_xy(geom: &LineGeom, cidx: usize) -> (f32, f32) {
        let idx = cidx.min(geom.content_chars);
        let p = geom.galley.pos_from_ccursor(CCursor::new(idx)).min;
        (p.x, p.y)
    }

    /// Byte → (char index within its line) for a line we have geometry for.
    fn cidx_of(rope: &Rope, geom: &LineGeom, byte: usize) -> usize {
        rope.byte_to_char(byte).saturating_sub(geom.line_char_start).min(geom.content_chars)
    }

    /// The bracket pair to emphasize: the matched bracket the caret sits AT, else the one
    /// immediately before it (so a caret just past a closer still lights its opener). `None` unless
    /// there is a single empty caret on a matched `()`/`[]`/`{}`.
    fn compute_match_pair(&self, rope: &Rope) -> Option<(usize, usize)> {
        if self.selections.ranges.len() != 1 || !self.selections.primary().is_empty() {
            return None;
        }
        let head = self.selections.primary().head;
        let is_bracket = |b: u8| matches!(b, b'(' | b'[' | b'{' | b')' | b']' | b'}');
        let is_open = |b: u8| matches!(b, b'(' | b'[' | b'{');
        let at = (head < rope.len_bytes()).then(|| rope.byte(head)).filter(|b| is_bracket(*b));
        let before = (head > 0).then(|| rope.byte(head - 1)).filter(|b| is_bracket(*b));
        let (pos, byte) = if let Some(b) = at {
            (head, b)
        } else if let Some(b) = before {
            (head - 1, b)
        } else {
            return None;
        };
        let m = self.brackets.matching(pos, is_open(byte))?;
        Some((pos, m))
    }

    /// Faint vertical indent guides at every enclosing 4-column tab stop. Blank lines inherit the
    /// smaller of their nearest non-blank neighbours' depth, so a guide runs unbroken down a block
    /// even past the empty lines inside it. Drawn before text; guides land in whitespace only.
    #[allow(clippy::too_many_arguments)]
    fn paint_indent_guides(
        &self,
        painter: &egui::Painter,
        rope: &Rope,
        first: usize,
        last: usize,
        total: usize,
        text_left: f32,
        origin_y: f32,
        row_h: f32,
        char_w: f32,
    ) {
        let step = 4.0 * char_w; // one indent level = 4 columns (matches soft-tab TAB)
        let color = Color32::from_gray(58);
        for line in first..last {
            if self.folds.is_hidden(line) {
                continue;
            }
            let depth = guide_depth(rope, line, total);
            for k in 1..depth {
                let x = text_left + k as f32 * step;
                let top = origin_y + self.line_to_row(line) as f32 * row_h;
                painter.line_segment(
                    [Pos2::new(x, top), Pos2::new(x, top + row_h)],
                    egui::Stroke::new(1.0, color),
                );
            }
        }
    }

    /// The end line of the outermost multi-line tree-sitter scope that STARTS on `line`, if any —
    /// i.e. whether `line` is a fold header and where its region ends.
    fn foldable_end(&self, rope: &Rope, line: usize) -> Option<usize> {
        let syn = self.syntax.as_ref()?;
        let probe = line_content_end(rope, line).min(rope.len_bytes().saturating_sub(1));
        syn.scopes_at(rope, probe)
            .into_iter()
            .filter_map(|c| {
                let dl = rope.byte_to_line(c.def_byte);
                let el = rope.byte_to_line(c.end_byte);
                (dl == line && el > line).then_some(el)
            })
            .max()
    }

    /// The innermost multi-line scope enclosing `byte`, as `(header_line, end_line)`.
    fn enclosing_foldable(&self, rope: &Rope, byte: usize) -> Option<(usize, usize)> {
        let syn = self.syntax.as_ref()?;
        syn.scopes_at(rope, byte.min(rope.len_bytes().saturating_sub(1)))
            .into_iter()
            .filter_map(|c| {
                let dl = rope.byte_to_line(c.def_byte);
                let el = rope.byte_to_line(c.end_byte);
                (el > dl).then_some((dl, el))
            })
            .min_by_key(|(h, e)| e - h)
    }

    /// Toggle the fold anchored at `line` (gutter-chevron click): unfold if it's a header, else
    /// fold the scope that opens there.
    fn toggle_fold_at_line(&mut self, rope: &Rope, line: usize) {
        if self.folds.is_header(line) {
            self.folds.toggle(line, 0);
        } else if let Some(end) = self.foldable_end(rope, line) {
            self.folds.toggle(line, end);
        }
    }

    /// Collapse the innermost scope containing the caret (JetBrains Ctrl+NumPad-).
    pub fn fold_at_caret(&mut self, buffer: &Buffer) {
        let rope = buffer.rope();
        if let Some((h, e)) = self.enclosing_foldable(rope, self.caret_byte()) {
            if !self.folds.is_header(h) {
                self.folds.toggle(h, e);
            }
        }
    }

    /// Expand the fold at the caret — the header on the caret's line, or the fold hiding it
    /// (JetBrains Ctrl+NumPad+).
    pub fn unfold_at_caret(&mut self, buffer: &Buffer) {
        let rope = buffer.rope();
        let line = rope.byte_to_line(self.caret_byte());
        if self.folds.is_header(line) {
            self.folds.toggle(line, 0);
        } else {
            self.folds.regions.retain(|(h, e)| !(line > *h && line <= *e));
        }
    }

    /// Alt+Z — toggle soft wrap for this editor.
    pub fn toggle_wrap(&mut self) {
        self.wrap = !self.wrap;
        self.row_index = None; // force a rebuild on the next paint
        self.row_index_key = None;
    }

    /// Rebuild the wrap row-index if its inputs changed. No-op (and cheap) when wrap is off.
    fn refresh_row_index(&mut self, rope: &Rope, generation: u64, wrap_cols: usize) {
        if !self.wrap {
            self.row_index = None;
            return;
        }
        let key = (generation, wrap_cols, self.folds.regions.clone());
        if self.row_index_key.as_ref() == Some(&key) && self.row_index.is_some() {
            return;
        }
        self.row_index = Some(RowIndex::build(rope, &self.folds, wrap_cols));
        self.row_index_key = Some(key);
    }

    /// First visual row of doc `line` (wrap-aware, else fold-aware).
    fn line_to_row(&self, line: usize) -> usize {
        match (self.wrap, &self.row_index) {
            (true, Some(ri)) => ri.line_to_row(line),
            _ => self.folds.line_to_row(line),
        }
    }

    /// Doc line owning visual `row`.
    fn row_to_line(&self, row: usize, total: usize) -> usize {
        match (self.wrap, &self.row_index) {
            (true, Some(ri)) => ri.row_to_line(row),
            _ => self.folds.row_to_line(row, total),
        }
    }

    /// Total visible rows for the document.
    fn total_rows(&self, total: usize) -> usize {
        match (self.wrap, &self.row_index) {
            (true, Some(ri)) => ri.total_rows(),
            _ => self.folds.total_rows(total),
        }
    }

    /// Ctrl+. — fold the scope at the caret, or unfold if the caret is on/inside a fold.
    pub fn toggle_fold_at_caret(&mut self, buffer: &Buffer) {
        let rope = buffer.rope();
        let line = rope.byte_to_line(self.caret_byte());
        if self.folds.is_header(line) || self.folds.is_hidden(line) {
            self.unfold_at_caret(buffer);
        } else {
            self.fold_at_caret(buffer);
        }
    }

    /// Ctrl+Shift+\ — jump the caret ONTO the bracket matching the one it sits on (JetBrains/VS
    /// Code "go to matching bracket"). Landing on the partner (not just past it) makes the jump
    /// involutive — pressing it again returns — even for an empty `()` pair where "just past"
    /// would absorb the caret between the two.
    pub fn jump_to_matching_bracket(&mut self, buffer: &mut Buffer) {
        if let Some((_, m)) = self.compute_match_pair(buffer.rope()) {
            self.selections.set_single(m);
            buffer.seal();
        }
    }

    /// Paint a soft box behind each bracket of [`Self::match_pair`] — the standard subtle
    /// emphasis that shows which brackets pair up.
    fn paint_match_pair(&self, painter: &egui::Painter, geoms: &[LineGeom], rope: &Rope, text_left: f32, row_h: f32) {
        let Some((a, b)) = self.match_pair else { return };
        for off in [a, b] {
            // Defensive: never index past the buffer. match_pair is recomputed each frame against
            // the current rope so this should always hold, but a byte_to_line past the end panics —
            // never worth risking on a per-frame paint.
            if off >= rope.len_bytes() {
                continue;
            }
            let line = rope.byte_to_line(off);
            let Some(geom) = geoms.iter().find(|g| g.line == line) else { continue };
            let ci = Self::cidx_of(rope, geom, off);
            // caret_xy for the sub-row y: on a wrapped line the bracket may sit on sub-row >0,
            // and painting at geom.top (sub-row 0) put the highlight on the wrong visual row.
            let (cx0, cy) = Self::caret_xy(geom, ci);
            let cx1 = Self::caret_x(geom, ci + 1);
            let x0 = text_left + cx0;
            // If the next char wrapped to the following sub-row, cx1 is that row's x=0 (< cx0);
            // fall back to a one-char width so the bracket box stays sane.
            let x1 = if cx1 > cx0 { text_left + cx1 } else { x0 + self.font_size * 0.6 };
            let top = geom.top + cy;
            painter.rect(
                Rect::from_min_max(Pos2::new(x0, top), Pos2::new(x1, top + row_h)),
                2.0,
                MATCH_BRACKET_FILL(),
                egui::Stroke::new(1.0, MATCH_BRACKET_STROKE()),
            );
        }
    }

    /// Dimmed inline preview of the pending AI completion at the caret. First line renders in
    /// place; extra lines are summarized with a return-mark hint (accepting inserts everything).
    fn paint_ghost(
        &self,
        painter: &egui::Painter,
        geoms: &[LineGeom],
        rope: &Rope,
        text_left: f32,
        row_h: f32,
        font: &FontId,
    ) {
        let Some(g) = &self.ghost else { return };
        let line = rope.byte_to_line(g.byte);
        let Some(geom) = geoms.iter().find(|ge| ge.line == line) else { return };
        // caret_xy for the sub-row y — a ghost at a caret on a wrapped sub-row was drawn on
        // sub-row 0 (mid-text, wrong line).
        let (cx, cy) = Self::caret_xy(geom, Self::cidx_of(rope, geom, g.byte));
        let x = text_left + cx;
        const GHOST: Color32 = Color32::from_gray(118);
        // EVERY line is painted. Showing only the first with a "⏎+N" counter meant Tab inserted
        // text the user had never seen — the accepted completion looked like a different
        // suggestion entirely. Continuation lines start at the text margin, as they will once
        // inserted; the first is inline at the caret.
        for (i, seg) in g.text.split('\n').enumerate() {
            let (px, py) = match i {
                0 => (x, geom.top + cy),
                _ => (text_left, geom.top + cy + row_h * i as f32),
            };
            if seg.is_empty() {
                continue;
            }
            painter.text(
                Pos2::new(px, py),
                egui::Align2::LEFT_TOP,
                seg,
                font.clone(),
                GHOST,
            );
        }
    }

    fn paint_selections(
        &mut self,
        painter: &egui::Painter,
        geoms: &[LineGeom],
        rope: &Rope,
        text_left: f32,
        row_h: f32,
        focused: bool,
    ) {
        for sel in &self.selections.ranges {
            let range = sel.range();
            for geom in geoms {
                let line_start = rope.line_to_byte(geom.line);
                let line_end = line_start + rope.line(geom.line).len_bytes();
                // Selection highlight on this line. Under soft wrap a line is several visual
                // sub-rows; a selection spanning them needs ONE rect PER sub-row (the old single
                // rect painted only sub-row 0 with a bogus width). caret_xy gives each end's
                // (x, sub-row y); the sub-row index is y/row_h.
                if !sel.is_empty() && range.start < line_end && range.end > line_start {
                    let (sx, sy) = Self::caret_xy(geom, Self::cidx_of(rope, geom, range.start.max(line_start)));
                    let crosses_eol = range.end >= line_end && geom.line + 1 < rope.len_lines();
                    let (ex, ey) = if crosses_eol {
                        let (x, y) = Self::caret_xy(geom, geom.content_chars);
                        (x + self.font_size * 0.35, y)
                    } else {
                        Self::caret_xy(geom, Self::cidx_of(rope, geom, range.end.min(line_end)))
                    };
                    let r0 = (sy / row_h).round() as i64;
                    let r1 = (ey / row_h).round() as i64;
                    let full_w = geom.galley.size().x;
                    for r in r0..=r1 {
                        let ly = r as f32 * row_h;
                        let left = if r == r0 { sx } else { 0.0 };
                        // Non-final sub-rows extend to the wrapped row width; the final one stops
                        // at the selection end.
                        let right = if r == r1 { ex } else { full_w };
                        if right <= left {
                            continue;
                        }
                        painter.rect_filled(
                            Rect::from_min_max(
                                Pos2::new(text_left + left, geom.top + ly),
                                Pos2::new(text_left + right, geom.top + ly + row_h),
                            ),
                            0.0,
                            SEL(),
                        );
                    }
                }
            }
        }
        // Caret blink (JetBrains cadence): 500ms on / 500ms off, restarted VISIBLE by any
        // caret movement or edit — a moving caret never disappears mid-motion. The repaint
        // for the next phase flip is requested explicitly; egui is reactive and would
        // otherwise never wake to hide/show it.
        let now = painter.ctx().input(|i| i.time);
        let head = self.selections.primary().head;
        if head != self.blink_head {
            self.blink_head = head;
            self.blink_epoch = now;
        }
        const BLINK: f64 = 0.5;
        let elapsed = (now - self.blink_epoch).max(0.0);
        let caret_visible = !focused || (elapsed / BLINK) as u64 % 2 == 0;
        if focused {
            let until_flip = BLINK - (elapsed % BLINK);
            painter.ctx().request_repaint_after(std::time::Duration::from_secs_f64(until_flip));
        }
        // Current-line band + carets on top.
        for sel in &self.selections.ranges {
            let head_line = rope.byte_to_line(sel.head);
            if let Some(geom) = geoms.iter().find(|g| g.line == head_line) {
                if sel.is_empty() {
                    painter.rect_filled(
                        Rect::from_min_max(
                            Pos2::new(text_left - TEXT_PAD, geom.top),
                            Pos2::new(text_left + self.max_width + TEXT_PAD, geom.top + row_h),
                        ),
                        0.0,
                        CURLINE(),
                    );
                }
                if focused {
                    let (cx, cy) = Self::caret_xy(geom, Self::cidx_of(rope, geom, sel.head));
                    let x = text_left + cx;
                    let y = geom.top + cy; // sub-row offset for wrapped lines (0 otherwise)
                    if caret_visible {
                        painter.rect_filled(
                            Rect::from_min_max(Pos2::new(x - 1.0, y), Pos2::new(x + 1.0, y + row_h)),
                            0.0,
                            CARET(),
                        );
                    }
                    // The anchor position is tracked even through the off phase — popups must
                    // not jump when the caret blinks.
                    self.caret_pos = Some((x, y + row_h));
                }
            }
        }
    }

    // ------------------------------------------------------------------------------------------
    // input
    // ------------------------------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn handle_pointer(
        &mut self,
        ui: &egui::Ui,
        resp: &egui::Response,
        geoms: &[LineGeom],
        buffer: &mut Buffer,
        origin: Pos2,
        gutter: f32,
        row_h: f32,
    ) {
        let Some(p) = ui.input(|i| i.pointer.interact_pos()) else { return };
        let (shift, alt) = ui.input(|i| (i.modifiers.shift, i.modifiers.alt));
        // Everything that needs the rope is resolved up front so the borrow ends before seal().
        let rope = buffer.rope();
        let total = rope.len_lines();
        // Clamp the clicked visual row to the last VISIBLE row: a click in the slack area past
        // EOF otherwise mapped (via row_to_line's clamp) to the last DOC line — which may be
        // hidden inside a fold, dropping the caret on an unpainted line (invisible until an
        // arrow key). Clamping lands it on the last visible line instead.
        let row = ((p.y - origin.y) / row_h).floor() as usize;
        let row = row.min(self.total_rows(total).saturating_sub(1));
        let line = self.row_to_line(row, total);
        let byte = self.byte_at(geoms, rope, line, p, origin.x + gutter + TEXT_PAD);
        // Word/line select fires ONLY for a genuine double/triple click in the TEXT area with no
        // Alt held. Without the guards: (a) an Alt+double-click added a caret on the first
        // release then word-select on the second release WIPED the whole multi-caret set
        // (self-cancel); (b) a double-click on the gutter/fold-strip fell through to a spurious
        // word-select at the gutter byte.
        let in_gutter = p.x < origin.x + gutter;
        let dbl = (resp.double_clicked() && !alt && !in_gutter)
            .then(|| selection::word_range(rope, byte));
        let trp = (resp.triple_clicked() && !alt && !in_gutter)
            .then(|| selection::line_range(rope, byte));

        // ONLY real pointer clicks drive the pointer branches. egui fakes a primary click on the
        // focused widget when Space/Enter is pressed ("Space/enter works like a primary click") —
        // and the editor IS the focused click-sensing widget while typing. Reacting to that fake
        // click teleported the caret to wherever the mouse was parked on every Space/Enter.
        let clicked = resp.clicked_by(egui::PointerButton::Primary);
        // The gutter ends in a fold strip; a click there toggles the fold on this line.
        let fold_w = self.font_size;
        if clicked && p.x >= origin.x + gutter - fold_w && p.x < origin.x + gutter {
            self.toggle_fold_at_line(rope, line);
            buffer.seal();
            return;
        }
        // Click further left in the gutter (over the line numbers): on a test-declaration
        // line it RUNS the test; anywhere else it toggles a breakpoint.
        if clicked && p.x < origin.x + gutter - fold_w {
            // Click priority mirrors PAINT priority: an existing breakpoint dot always toggles
            // off (the visible affordance must match the action); the test ▶ runs otherwise.
            if self.breakpoint_lines.binary_search(&line).is_err()
                && self.test_lines.binary_search(&line).is_ok()
            {
                self.test_click = Some(line);
            } else {
                self.gutter_click = Some(line);
            }
            return;
        }
        // Right-click: place the caret (unless inside an existing selection — JetBrains keeps
        // the selection for selection-scoped actions) and hand the byte to the app's menu.
        if resp.secondary_clicked() {
            let inside_selection = self
                .selections
                .ranges
                .iter()
                .any(|s| !s.is_empty() && s.range().contains(&byte));
            if !inside_selection {
                self.selections.set_single(byte);
            }
            buffer.seal();
            self.context_click = Some(byte);
            return;
        }
        // Alt+Click adds/removes a caret; Alt+drag is column (rectangular) selection.
        if alt && clicked {
            self.selections.toggle_caret(byte);
            buffer.seal();
            return;
        }
        if alt && resp.drag_started() {
            self.column_anchor = Some(byte);
            self.selections.set_column_selection(buffer.rope(), byte, byte);
            buffer.seal();
            return;
        }
        if resp.dragged() {
            if let Some(anchor) = self.column_anchor {
                self.selections.set_column_selection(buffer.rope(), anchor, byte);
                buffer.seal();
                return;
            }
        } else {
            self.column_anchor = None; // drag ended (or never started)
        }
        if let Some(r) = dbl {
            self.selections = Selections { ranges: vec![Selection { anchor: r.start, head: r.end, goal_col: None }], primary: 0 };
        } else if let Some(r) = trp {
            self.selections = Selections { ranges: vec![Selection { anchor: r.start, head: r.end, goal_col: None }], primary: 0 };
        } else if resp.drag_started() {
            if shift {
                self.extend_primary(byte);
            } else {
                self.selections.set_single(byte);
            }
        } else if resp.dragged() {
            self.extend_primary(byte);
        } else if clicked {
            if ui.input(|i| i.modifiers.command) {
                self.ctrl_click = Some(byte); // goto-definition; caret still moves
            }
            if shift {
                self.extend_primary(byte);
            } else {
                self.selections.set_single(byte);
            }
        } else {
            return; // no selection change → nothing to seal
        }
        buffer.seal();
    }

    /// Screen position → byte offset, via the target line's galley when visible.
    fn byte_at(&self, geoms: &[LineGeom], rope: &Rope, line: usize, p: Pos2, text_left: f32) -> usize {
        if let Some(geom) = geoms.iter().find(|g| g.line == line) {
            // Pass the y offset within the line's galley so a wrapped (multi-row) line hit-tests to
            // the right visual sub-row; for an unwrapped single-row galley this is a no-op.
            let cur = geom.galley.cursor_from_pos(Vec2::new(p.x - text_left, p.y - geom.top));
            let cidx = cur.ccursor.index.min(geom.content_chars);
            rope.char_to_byte(geom.line_char_start + cidx)
        } else {
            rope.line_to_byte(line.min(rope.len_lines().saturating_sub(1)))
        }
    }

    fn extend_primary(&mut self, byte: usize) {
        let i = self.selections.primary;
        self.selections.ranges[i].head = byte;
        self.selections.ranges[i].goal_col = None;
    }

    /// Editor zoom: adjust the monospace size (clamped to sane bounds).
    pub fn set_font_size(&mut self, px: f32) {
        self.font_size = px.clamp(8.0, 40.0);
        self.max_width = 0.0; // re-measure content width at the new size
    }

    pub fn font_size(&self) -> f32 {
        self.font_size
    }

    pub fn take_gutter_click(&mut self) -> Option<usize> {
        self.gutter_click.take()
    }

    /// Gutter click on a test-declaration line (0-based), if any this frame.
    pub fn take_test_click(&mut self) -> Option<usize> {
        self.test_click.take()
    }

    /// Ctrl+W. Grow every selection to the next enclosing syntactic range.
    ///
    /// The ladder is REMEMBERED rather than recomputed, so Ctrl+Shift+W returns to exactly the
    /// range you saw rather than whatever a fresh walk would produce for the widened selection.
    pub fn expand_selection(&mut self, buffer: &mut Buffer) {
        if !self.expand_valid(buffer) {
            // Seed one stack per caret at its current range.
            let stacks = self
                .selections
                .ranges
                .iter()
                .map(|s| ExpandStack { ranges: vec![s.range()], at: 0 })
                .collect();
            self.expand = Some((buffer.generation, stacks));
        }
        let rope = buffer.rope().clone();
        let Some((_, stacks)) = self.expand.as_mut() else { return };
        let mut moved = false;
        for (i, st) in stacks.iter_mut().enumerate() {
            let cur = st.ranges[st.at].clone();
            // Already-computed rung above? Reuse it, so expand-shrink-expand is stable.
            if st.at + 1 < st.ranges.len() {
                st.at += 1;
                moved = true;
            } else if let Some(next) = Self::expand_step(self.syntax.as_ref(), self.lang, &rope, cur) {
                st.ranges.push(next);
                st.at += 1;
                moved = true;
            }
            let r = st.ranges[st.at].clone();
            if let Some(sel) = self.selections.ranges.get_mut(i) {
                *sel = Selection { anchor: r.start, head: r.end, goal_col: None };
            }
        }
        if !moved {
            return;
        }
        let before = self.selections.ranges.len();
        self.selections.merge_overlaps();
        if self.selections.ranges.len() != before {
            // Merged carets have no honest ladder between them; drop it rather than let shrink
            // walk a stack that no longer corresponds to the selections.
            self.expand = None;
        }
        buffer.seal();
    }

    /// Ctrl+Shift+W: step back down the remembered ladder. A no-op when there is no live ladder.
    pub fn shrink_selection(&mut self, buffer: &mut Buffer) {
        if !self.expand_valid(buffer) {
            return;
        }
        let Some((_, stacks)) = self.expand.as_mut() else { return };
        for (i, st) in stacks.iter_mut().enumerate() {
            if st.at == 0 {
                continue;
            }
            st.at -= 1;
            let r = st.ranges[st.at].clone();
            if let Some(sel) = self.selections.ranges.get_mut(i) {
                *sel = Selection { anchor: r.start, head: r.end, goal_col: None };
            }
        }
        buffer.seal();
    }

    /// Is the remembered ladder still the one describing the live selections? False after any
    /// edit or any caret movement that did not come from expand/shrink.
    fn expand_valid(&self, buffer: &Buffer) -> bool {
        let Some((gen, stacks)) = &self.expand else { return false };
        *gen == buffer.generation
            && stacks.len() == self.selections.ranges.len()
            && stacks
                .iter()
                .zip(&self.selections.ranges)
                .all(|(st, sel)| st.ranges.get(st.at) == Some(&sel.range()))
    }

    /// One rung up from `cur`.
    fn expand_step(
        syntax: Option<&Syntax>,
        lang: Option<Lang>,
        rope: &Rope,
        cur: Range<usize>,
    ) -> Option<Range<usize>> {
        // A bare caret starts at the token. Prefer the word to the LEFT when the caret sits just
        // after one (`foo|(`), which is where it lands after typing an identifier.
        if cur.is_empty() {
            let w = selection::word_range(rope, cur.start);
            if !w.is_empty() {
                return Some(w);
            }
            if cur.start > 0 {
                let w = selection::word_range(rope, cur.start - 1);
                if !w.is_empty() {
                    return Some(w);
                }
            }
        }
        let bigger = |r: Range<usize>| (r.start < cur.start || r.end > cur.end).then_some(r);

        // Inside a string, the CONTENTS come before the quotes.
        if let (Some(syn), Some(l)) = (syntax, lang) {
            if let Some(s) = syn.innermost_of_kind(cur.start, l.string_kinds()) {
                let inner = inner_span(rope, &s);
                if cur.start >= inner.start && cur.end <= inner.end {
                    if let Some(r) = bigger(inner) {
                        return Some(r);
                    }
                }
                if let Some(r) = bigger(s) {
                    return Some(r);
                }
            }
        }
        let Some(syn) = syntax else {
            // No parse tree (plain text): word -> line -> whole buffer, so Ctrl+W is never dead.
            let line = selection::line_range(rope, cur.start);
            return bigger(line).or_else(|| bigger(0..rope.len_bytes()));
        };
        let mut range = syn.enclosing_range(cur.clone())?;
        // A delimited list yields its INTERIOR first, so no rung ever includes one paren but not
        // the other, and `f(a, b|, c)` steps b -> a, b, c -> (a, b, c).
        if let Some(l) = lang {
            let node_is_list = syn
                .innermost_of_kind(cur.start, l.list_kinds())
                .is_some_and(|r| r == range);
            if node_is_list {
                let inner = inner_span(rope, &range);
                if let Some(r) = bigger(inner) {
                    return Some(r);
                }
            }
        }
        // Climb until something is genuinely bigger.
        loop {
            if let Some(r) = bigger(range.clone()) {
                return Some(r);
            }
            range = syn.enclosing_range(range)?;
        }
    }

    /// Tab on a bare caret: expand a postfix or live template, returning whether one fired.
    ///
    /// Postfix is tried FIRST. `x.if` ends in a word that is also a live-template key in several
    /// languages, and the postfix reading is unambiguous where the live one would silently eat the
    /// expression the user just wrote.
    fn expand_template(&mut self, buffer: &mut Buffer, now: f64) -> bool {
        let caret = self.selections.primary().head;
        let text = buffer.rope().to_string();
        if let Some(hit) = crate::templates::resolve_postfix(&text, caret, self.lang) {
            // Re-indent the body to the line it lands on: a snippet written flat would otherwise
            // paste its second line at column zero.
            let indent = leading_ws(buffer.rope(), hit.replace.start);
            let body = reindent_snippet(&hit.snippet, &indent);
            self.insert_snippet(buffer, hit.replace, &body, now);
            return true;
        }
        if let Some((range, body)) = crate::templates::resolve_live(&text, caret, self.lang) {
            let indent = leading_ws(buffer.rope(), range.start);
            let body = reindent_snippet(body, &indent);
            self.insert_snippet(buffer, range, &body, now);
            return true;
        }
        false
    }

    /// Ctrl+Alt+V: pull the selected expression out into a new local declared on its own line
    /// above, and replace the selection with the new name. Returns the byte range of the inserted
    /// NAME so the caller can start an inline rename on it — you almost always want to name it
    /// something other than the placeholder.
    ///
    /// Requires a selection: inferring the "expression under the caret" is exactly the guess that
    /// makes this refactoring feel unsafe, and a wrong guess silently restructures code.
    pub fn extract_variable(
        &mut self,
        buffer: &mut Buffer,
        decl_prefix: &str,
        name: &str,
        now: f64,
    ) -> Option<Range<usize>> {
        let sel = self.selections.primary().range();
        if sel.is_empty() || self.selections.ranges.len() != 1 {
            return None;
        }
        let rope = buffer.rope();
        let expr: String = rope.byte_slice(sel.clone()).into();
        if expr.contains('\n') {
            return None; // a multi-line expression is not something to guess the shape of
        }
        let line_ix = rope.byte_to_line(sel.start);
        let ls = rope.line_to_byte(line_ix);
        let indent = leading_ws(rope, ls);
        let decl = format!("{indent}{decl_prefix}{name} = {expr};\n");
        // Two edits in ONE transaction so undo restores both together — an extract that half
        // undoes leaves code that does not compile.
        let name_at = ls + indent.len() + decl_prefix.len();
        let name_len = name.len();
        let before = self.selections.snapshot();
        let tx = crate::buffer::Transaction {
            changes: vec![
                crate::buffer::Change { start: ls, end: ls, text: decl },
                crate::buffer::Change { start: sel.start, end: sel.end, text: name.to_string() },
            ],
        };
        self.remap_snippet(&tx);
        let pre = rope.clone();
        self.folds.remap(&pre, &tx);
        let after_sel = Selections::single(name_at + name_len);
        let after = after_sel.snapshot();
        buffer.record(&tx, EditMeta { kind: EditKind::Other, carets: 1, time: now, before, after });
        if let Some(syn) = &mut self.syntax {
            syn.edited(buffer.rope(), &tx.changes);
        }
        self.edits_out.push((pre, tx));
        self.selections = after_sel;
        Some(name_at..name_at + name_len)
    }

    /// Ctrl+Shift+Enter: finish the statement the caret is in and put the caret where you would
    /// type next.
    ///
    /// Deliberately conservative — it completes what is unambiguous from the line's own text and
    /// does nothing otherwise. A "smart" action that guesses wrong is worse than one that
    /// occasionally declines, because it silently rewrites code you were mid-thought on.
    ///
    /// * unbalanced `(` → close it, then apply the rules below to the result
    /// * `if (…)` / `for (…)` / `while (…)` with nothing after → ` {`, newline, indent, `}`
    /// * an expression statement missing its `;` → append one
    /// * already terminated → just move to the next line, indented
    pub fn complete_statement(&mut self, buffer: &mut Buffer, now: f64) {
        if !self.lang.is_some_and(Lang::brace_indented) {
            return;
        }
        let rope = buffer.rope();
        let head = self.selections.primary().head;
        let line_ix = rope.byte_to_line(head);
        let ls = rope.line_to_byte(line_ix);
        let line: String = rope.line(line_ix).into();
        let line = line.trim_end_matches(['\n', '\r']).to_string();
        let end = ls + line.len();
        let indent = leading_ws(rope, ls);
        let code = code_of_line(&line, self.lang);
        let trimmed = code.trim_end();
        // Count on a copy with literal CONTENTS blanked, so `puts("(")` is not read as having an
        // unclosed paren. Comments are already gone; lengths are preserved, so `trimmed` and the
        // mask agree position for position.
        let masked = mask_literals(trimmed, self.lang);
        let opens = masked.chars().filter(|c| *c == '(').count();
        let closes = masked.chars().filter(|c| *c == ')').count();
        let mut tail = String::new();
        tail.push_str(&")".repeat(opens.saturating_sub(closes)));

        let with_parens = format!("{trimmed}{tail}");
        let starts_block = ["if", "for", "while", "switch"]
            .iter()
            .any(|kw| starts_with_keyword(&with_parens, kw));

        let (insert, caret_back) = if starts_block && with_parens.ends_with(')') {
            // `if (x)` -> block, caret on the body line.
            let body = format!("{indent}{TAB}");
            (format!("{tail} {{\n{body}\n{indent}}}"), indent.len() + 2)
        } else if with_parens.is_empty() || with_parens.ends_with([';', '{', '}', ':']) {
            // Nothing to finish — just go to the next line, like Enter would.
            (format!("{tail}\n{indent}"), 0)
        } else {
            (format!("{tail};\n{indent}"), 0)
        };

        // Insert at END OF LINE regardless of where in the line the caret sits: finishing a
        // statement is a statement-level action, and requiring the caret at the end would make it
        // useless mid-edit, which is exactly when it is reached for.
        let n = insert.len();
        self.apply_edits_caret(buffer, EditKind::Other, now, move |_, _| {
            (end..end, insert.clone(), n - caret_back)
        });
    }

    /// Ctrl+Shift+F7: mark every occurrence of the identifier under the caret in THIS file.
    ///
    /// Whole-word, case-sensitive, and literal — this is the cheap in-file companion to Find
    /// Usages, not a semantic search. It deliberately does not consult the language server: it
    /// must answer instantly on every keystroke-free frame, and a stale LSP answer painted over
    /// live text is worse than an honest lexical one.
    pub fn highlight_usages(&mut self, buffer: &Buffer) {
        let rope = buffer.rope();
        let w = selection::word_range(rope, self.selections.primary().head);
        if w.is_empty() {
            self.usage_marks = None;
            return;
        }
        let needle: String = rope.byte_slice(w).into();
        let hay = rope.to_string();
        let mut marks = Vec::new();
        let mut at = 0usize;
        while let Some(off) = hay[at..].find(&needle) {
            let s = at + off;
            let e = s + needle.len();
            // Whole word only: `count` must not light up inside `counter` or `recount`.
            let before_ok = s == 0 || !is_word_byte(hay.as_bytes()[s - 1]);
            let after_ok = e >= hay.len() || !is_word_byte(hay.as_bytes()[e]);
            if before_ok && after_ok {
                marks.push(s..e);
            }
            at = e.max(s + 1);
        }
        self.usage_marks = Some((buffer.generation, marks));
    }

    /// The primary selection as a byte range (empty when it is a bare caret).
    pub fn primary_selection_range(&self) -> std::ops::Range<usize> {
        self.selections.primary().range()
    }

    /// Are usage marks currently shown? Lets the app route Escape to clearing them first.
    pub fn usage_marks_shown(&self) -> bool {
        self.usage_marks.is_some()
    }

    pub fn clear_usage_marks(&mut self) {
        self.usage_marks = None;
    }

    /// Install an AI ghost completion (shown dimmed at the caret; Tab accepts).
    pub fn set_ghost(&mut self, byte: usize, generation: u64, text: String) {
        if !text.is_empty() {
            self.ghost = Some(Ghost { byte, generation, text });
        }
    }

    /// Did Tab accept a ghost during THIS frame's key handling? Read-only: the flag is reset at
    /// the top of every `handle_keys`, so it can never leak into a later frame where the app
    /// would wrongly suppress a real completion accept.
    pub fn ghost_accepted(&self) -> bool {
        self.ghost_accepted
    }

    pub fn clear_ghost(&mut self) {
        self.ghost = None;
    }

    pub fn ghost_visible(&self) -> bool {
        self.ghost.is_some()
    }

    /// (caret byte, generation) if a single bare caret is idle — the app's cue to request a
    /// ghost completion. None during selections/multi-caret (ghosts would be ambiguous).
    pub fn ghost_anchor(&self, buffer: &Buffer) -> Option<(usize, u64)> {
        if self.selections.ranges.len() != 1 {
            return None;
        }
        let sel = self.selections.primary();
        if !sel.is_empty() {
            return None;
        }
        Some((sel.head, buffer.generation))
    }

    fn handle_keys(&mut self, ui: &egui::Ui, buffer: &mut Buffer) {
        // Frame-scoped (see ghost_accepted).
        self.ghost_accepted = false;
        let now = ui.input(|i| i.time);
        let events = ui.input(|i| i.events.clone());
        for event in events {
            match event {
                egui::Event::Text(t) if !t.is_empty() => {
                    // A single printable char gets the auto-pair / surround / skip treatment; if it
                    // fully handled the keystroke, skip the literal insert. Multi-char Text (IME,
                    // paste-as-text) always inserts verbatim.
                    let mut chars = t.chars();
                    let handled = match (chars.next(), chars.next()) {
                        (Some(c), None) => self.typed_char(buffer, c, now),
                        _ => false,
                    };
                    if !handled {
                        self.insert(buffer, &t, EditKind::InsertText, now);
                    }
                }
                egui::Event::Paste(s) if !s.is_empty() => self.insert(buffer, &s, EditKind::Paste, now),
                egui::Event::Copy => {
                    self.copy_to_clipboard(ui, buffer);
                }
                egui::Event::Cut => {
                    self.copy_to_clipboard(ui, buffer);
                    // Delete the selection (an empty caret cuts nothing — matches most editors).
                    if self.selections.ranges.iter().any(|s| !s.is_empty()) {
                        self.apply_edits(buffer, EditKind::Cut, now, |sel, _| (sel.range(), String::new()));
                    }
                }
                egui::Event::Key { key, pressed: true, modifiers, .. } => {
                    self.handle_key(buffer, key, modifiers, now);
                }
                _ => {}
            }
        }
    }

    fn handle_key(&mut self, buffer: &mut Buffer, key: Key, m: egui::Modifiers, now: f64) {
        // A modal overlay owns these outright (Enter included) while it is open.
        if self.modal_keys_stolen
            && matches!(key, Key::Enter | Key::ArrowUp | Key::ArrowDown | Key::Tab | Key::Escape)
        {
            return;
        }
        let ext = m.shift;
        let word = m.command;
        match key {
            // --- add-caret gestures (checked before the plain motion arms) --------------------
            Key::ArrowUp if m.command && m.alt => {
                self.selections.add_caret_vertical(buffer.rope(), false);
                buffer.seal();
            }
            Key::ArrowDown if m.command && m.alt => {
                self.selections.add_caret_vertical(buffer.rope(), true);
                buffer.seal();
            }
            Key::J if m.command && m.alt && m.shift => {
                self.selections.select_all_occurrences(buffer.rope());
                buffer.seal();
            }
            Key::J if m.alt && m.shift => {
                self.selections.unselect_last_occurrence();
                buffer.seal();
            }
            Key::J if m.alt => {
                self.selections.add_next_occurrence(buffer.rope());
                buffer.seal();
            }
            // --- line-structural edits (checked before the plain motion arrow arms) -----------
            Key::ArrowUp if m.command && m.shift => self.move_statement(buffer, false, now),
            Key::ArrowDown if m.command && m.shift => self.move_statement(buffer, true, now),
            Key::ArrowUp if m.alt && m.shift => self.move_lines(buffer, false, now),
            Key::ArrowDown if m.alt && m.shift => self.move_lines(buffer, true, now),
            // Ghost completion: Tab accepts, Esc dismisses (before popup routing — an offered
            // ghost is the more specific state).
            Key::Tab if self.ghost.is_some() && !m.shift => {
                if let Some(g) = self.ghost.take() {
                    self.insert(buffer, &g.text, EditKind::Other, now);
                    // Claim the keystroke. The completion popup reads Tab from the same frame's
                    // input, so without this ONE Tab both accepted the ghost and committed the
                    // popup's selection on top of it — the user saw grey text and got something
                    // else entirely.
                    self.ghost_accepted = true;
                }
            }
            // Escape clears usage marks before anything else claims it — they are the most
            // recently raised transient state on screen.
            Key::Escape if self.usage_marks.is_some() && self.ghost.is_none() => {
                self.usage_marks = None;
            }
            Key::Escape if self.ghost.is_some() => {
                self.ghost = None;
            }
            // Completion popup owns arrows/Tab/Esc while open (the app routes them to the list).
            // Enter is NEVER stolen: it ALWAYS makes a newline. Completions accept on Tab or click.
            // (The old suppress_enter path handed Enter to the popup based on a cross-frame flag set
            // AFTER the editor ran — a race that ate newlines and, on a stale LSP range, dropped the
            // caret on a random line. Unambiguous Enter=newline removes the race entirely.)
            Key::ArrowUp | Key::ArrowDown | Key::Tab | Key::Escape if self.suppress_nav_keys => {}
            // --- motion (each seals the coalescing group) ------------------------------------
            Key::ArrowLeft => self.motion(buffer, if word { Motion::WordLeft } else { Motion::Left }, ext),
            Key::ArrowRight if word && !ext && self.ghost.is_some() => {
                // Partial accept: take the ghost's next word (incl. leading ws/punct run).
                if let Some(g) = self.ghost.take() {
                    let t = &g.text;
                    let mut end = 0;
                    let mut seen_word = false;
                    for (i, ch) in t.char_indices() {
                        let is_word = ch.is_alphanumeric() || ch == '_';
                        if seen_word && !is_word {
                            end = i;
                            break;
                        }
                        if is_word {
                            seen_word = true;
                        }
                        end = i + ch.len_utf8();
                    }
                    let (head, rest) = t.split_at(end.max(1).min(t.len()));
                    let head = head.to_string();
                    let rest = rest.to_string();
                    self.insert(buffer, &head, EditKind::InsertText, now);
                    if !rest.is_empty() {
                        let byte = self.selections.primary().head;
                        self.ghost = Some(Ghost { byte, generation: buffer.generation, text: rest });
                    }
                }
            }
            Key::ArrowRight => self.motion(buffer, if word { Motion::WordRight } else { Motion::Right }, ext),
            Key::ArrowUp => self.motion(buffer, Motion::Up, ext),
            Key::ArrowDown => self.motion(buffer, Motion::Down, ext),
            Key::Home => self.motion(buffer, if word { Motion::BufStart } else { Motion::LineStart }, ext),
            Key::End => self.motion(buffer, if word { Motion::BufEnd } else { Motion::LineEnd }, ext),
            // --- edits -----------------------------------------------------------------------
            Key::Backspace if !word && self.try_delete_pair(buffer, now) => {}
            Key::Backspace => self.delete_side(buffer, false, word, now),
            Key::Delete => self.delete_side(buffer, true, word, now),
            Key::Enter => self.insert_newline(buffer, now),
            // Snippet session: Tab/Shift+Tab traverse the stops, Esc (below) ends it. After
            // the popup/ghost arms — accepting a completion mid-snippet stays possible.
            Key::Tab if self.snippet.is_some() && !m.shift => self.snippet_step(buffer, 1),
            Key::Tab if self.snippet.is_some() && m.shift => self.snippet_step(buffer, -1),
            // Shift+Tab always unindents the spanned line(s); Tab with a selection block-indents,
            // Tab on a bare caret expands a template if one matches, else inserts (JetBrains).
            Key::Tab if m.shift => self.indent_lines(buffer, true, now),
            Key::Tab => {
                if self.selections.ranges.iter().any(|s| !s.is_empty()) {
                    self.indent_lines(buffer, false, now);
                } else if !self.expand_template(buffer, now) {
                    self.insert(buffer, TAB, EditKind::Other, now);
                }
            }
            Key::Escape if self.snippet.is_some() => {
                self.snippet = None; // end the snippet session, keep the caret where it is
            }
            Key::Escape => {
                if self.find.open {
                    self.find.open = false; // Esc closes the find bar first (JetBrains)
                } else {
                    let head = self.selections.primary().head;
                    self.selections.set_single(head);
                    buffer.seal();
                }
            }
            Key::F if m.command => self.open_find(buffer, false),
            Key::R if m.command => self.open_find(buffer, true),
            Key::F3 if !self.find.query.is_empty() => self.goto_match(buffer, !m.shift, true),
            Key::A if m.command => {
                self.selections.select_all(buffer.rope());
                buffer.seal();
            }
            Key::Z if m.command => {
                if m.shift {
                    self.redo(buffer);
                } else {
                    self.undo(buffer);
                }
            }
            // Ctrl+Y = Delete Line (JetBrains). Redo keeps Ctrl+Shift+Z, which this arm merely
            // duplicated.
            Key::Y if m.command => self.delete_lines(buffer, now),
            Key::U if m.command && m.shift => self.toggle_case(buffer, now),
            Key::Enter if m.command && m.shift => self.complete_statement(buffer, now),
            Key::W if m.command && m.shift => self.shrink_selection(buffer),
            Key::W if m.command => self.expand_selection(buffer),
            Key::G if m.alt && m.shift => self.carets_to_line_ends(buffer),
            Key::D if m.command => self.duplicate_lines(buffer, now),
            // Ctrl+/ toggle line comment, Ctrl+Shift+/ toggle block comment, Ctrl+Shift+J join.
            Key::Slash if m.command && m.shift => self.toggle_block_comment(buffer, now),
            Key::Slash if m.command => self.toggle_line_comment(buffer, now),
            Key::J if m.command && m.shift => self.join_lines(buffer, now),
            Key::Backslash if m.command && m.shift => self.jump_to_matching_bracket(buffer),
            Key::Period if m.command => self.toggle_fold_at_caret(buffer),
            Key::Z if m.alt => self.toggle_wrap(),
            _ => {}
        }
    }

    /// Move all carets, then seal the undo group (a caret move breaks type-a-word coalescing).
    fn motion(&mut self, buffer: &mut Buffer, motion: Motion, extend: bool) {
        self.selections.move_all(buffer.rope(), motion, extend);
        buffer.seal();
    }

    // ------------------------------------------------------------------------------------------
    // edits — every path funnels through `apply_edits`
    // ------------------------------------------------------------------------------------------

    /// Build one transaction from a per-selection `(range, replacement)`, file it in history with
    /// the given `kind`/`now` (drives undo coalescing) and the before/after caret snapshots (drives
    /// caret restore), then move every caret to the end of its inserted text.
    fn apply_edits(
        &mut self,
        buffer: &mut Buffer,
        kind: EditKind,
        now: f64,
        f: impl Fn(&Selection, &Rope) -> (Range<usize>, String),
    ) {
        // Caret lands at the end of the inserted text — the universal case.
        self.apply_edits_caret(buffer, kind, now, |sel, rope| {
            let (r, t) = f(sel, rope);
            let n = t.len();
            (r, t, n)
        });
    }

    /// As [`Self::apply_edits`], but `f` also returns where the caret should land WITHIN the
    /// inserted text. Needed by the brace split, which inserts two lines and must leave the caret
    /// on the first — every other edit wants the end, which `apply_edits` supplies.
    fn apply_edits_caret(
        &mut self,
        buffer: &mut Buffer,
        kind: EditKind,
        now: f64,
        f: impl Fn(&Selection, &Rope) -> (Range<usize>, String, usize),
    ) {
        let before = self.selections.snapshot();
        let raw: Vec<(Range<usize>, String, usize)> =
            self.selections.ranges.iter().map(|s| f(s, buffer.rope())).collect();
        // Carry the caret offset alongside each item through the sort/coalesce below.
        let items: Vec<(Range<usize>, String)> =
            raw.iter().map(|(r, t, _)| (r.clone(), t.clone())).collect();
        let mut caret_at: Vec<usize> = raw.iter().map(|(_, _, c)| *c).collect();
        let mut order: Vec<usize> = (0..items.len()).collect();
        order.sort_by_key(|&i| items[i].0.start);
        caret_at = order.iter().map(|&i| caret_at[i]).collect();
        let items: Vec<(Range<usize>, String)> = order.into_iter().map(|i| items[i].clone()).collect();
        let mut items = items;
        // (already sorted above, alongside caret_at)
        // Coalesce OVERLAPPING ranges so the Transaction's changes stay sorted+disjoint (its hard
        // invariant). Per-caret motion can overlap: two bare carets inside one word both Ctrl+
        // Backspace to the word start, yielding e.g. 6..8 and 6..10 — which panicked apply_inner.
        // Merge any range that starts before the previous one ends into it (union range, append
        // text in order); the carets then collapse onto the shared point via merge_overlaps below.
        let mut merged: Vec<(Range<usize>, String)> = Vec::with_capacity(items.len());
        for (r, t) in items {
            match merged.last_mut() {
                Some((pr, pt)) if r.start < pr.end => {
                    pr.end = pr.end.max(r.end);
                    pt.push_str(&t);
                }
                _ => merged.push((r, t)),
            }
        }
        let items = merged;
        // The transaction carries only real changes — zero-effect edits (empty range + empty text,
        // e.g. backspace at byte 0) are excluded so an all-empty edit never pushes an undo group or
        // clears redo. The caret repositioning below still walks ALL items: a no-op caret keeps its
        // spot (shifted by earlier deltas) instead of being dropped from the selection set.
        let changes: Vec<Change> = items
            .iter()
            .filter(|(r, t)| !(r.start == r.end && t.is_empty()))
            .map(|(r, t)| Change { start: r.start, end: r.end, text: t.clone() })
            .collect();
        if changes.is_empty() {
            return;
        }
        let tx = Transaction { changes };
        self.remap_snippet(&tx);

        // Reposition carets purely (independent of rope state): caret i lands at
        // range.start + Σ(earlier deltas) + inserted len. For a no-op item both terms of its own
        // delta are zero, so it stays put under the same formula.
        let mut delta: isize = 0;
        let ranges: Vec<Selection> = items
            .iter()
            .enumerate()
            .map(|(i, (r, t))| {
                // Coalescing can merge items; a merged entry keeps the FIRST one's caret offset,
                // clamped so it can never point past the (now longer) text.
                let within = caret_at.get(i).copied().unwrap_or(t.len()).min(t.len());
                let caret = (r.start as isize + delta + within as isize) as usize;
                delta += t.len() as isize - (r.end - r.start) as isize;
                Selection::at(caret)
            })
            .collect();
        let mut new_sel = Selections { primary: ranges.len() - 1, ranges };
        // Carets that collapsed onto the same offset (adjacent backspaces meeting) merge into one.
        new_sel.merge_overlaps();
        let after = new_sel.snapshot();

        // carets = the LIVE caret count, not the surviving-change count: a multi-caret edit where
        // some carets produced no-ops (backspace at byte 0) must still refuse to coalesce.
        let pre = buffer.rope().clone(); // O(1) Arc-shared snapshot for the LSP didChange queue
        self.folds.remap(&pre, &tx); // keep folds hiding the same lines after the edit
        buffer.record(&tx, EditMeta { kind, carets: before.ranges.len(), time: now, before, after });
        if let Some(syn) = &mut self.syntax {
            syn.edited(buffer.rope(), &tx.changes);
        }
        self.edits_out.push((pre, tx));
        self.selections = new_sel;
    }

    fn insert(&mut self, buffer: &mut Buffer, s: &str, kind: EditKind, now: f64) {
        // TabNine-style type-through: typing the ghost's own next characters ADVANCES the
        // suggestion instead of killing it.
        let survive = match (&self.ghost, kind) {
            (Some(g), EditKind::InsertText)
                if self.selections.ranges.len() == 1
                    && self.selections.primary().is_empty()
                    && self.selections.primary().head == g.byte
                    && g.text.starts_with(s)
                    && g.text.len() > s.len() =>
            {
                true
            }
            _ => false,
        };
        let text = s.to_string();
        self.apply_edits(buffer, kind, now, |sel, _| (sel.range(), text.clone()));
        if survive {
            if let Some(g) = &mut self.ghost {
                g.byte += s.len();
                g.text.drain(..s.len());
                g.generation = buffer.generation;
            }
        }
    }

    /// Enter. Copies the current indent, and for brace languages adds three things a bare copy
    /// cannot do: a trailing `{` steps in one level; splitting `{|}` opens a body and leaves the
    /// closer on its own dedented line; and Enter inside a `/* … */` continues the comment.
    ///
    /// Every rule is computed from the text BEFORE the caret on the caret's own line, with
    /// strings and comments stripped ([`code_of_line`]) so a brace inside `"{"` never counts.
    fn insert_newline(&mut self, buffer: &mut Buffer, now: f64) {
        let lang = self.lang;
        self.apply_edits_caret(buffer, EditKind::Newline, now, move |sel, rope| {
            let start = sel.range().start;
            let indent = leading_ws(rope, start);
            let line_ix = rope.byte_to_line(start);
            let line_start = rope.line_to_byte(line_ix);
            // Only what precedes the caret decides the indent: pressing Enter in the middle of
            // `foo() { bar` must not be swayed by text that is about to move to the next line.
            let before: String =
                rope.slice(rope.byte_to_char(line_start)..rope.byte_to_char(start)).chars().collect();
            let code = code_of_line(&before, lang);

            if let Some(marker) = block_comment_continuation(&before, lang) {
                let text = format!("\n{indent}{marker}");
                let n = text.len();
                return (sel.range(), text, n);
            }

            let deeper = indent_for_new_line(code, &indent, lang);
            // Brace split: `{|}` becomes an open body with the closer dedented on its own line.
            // Only when the very next non-space character is the matching `}`.
            let opened = deeper.len() > indent.len();
            let next_is_close = rope
                .slice(rope.byte_to_char(start)..)
                .chars()
                .find(|c| *c != ' ' && *c != '\t')
                == Some('}');
            if opened && next_is_close {
                let text = format!("\n{deeper}\n{indent}");
                // Caret on the MIDDLE line, not after the closer.
                let caret = 1 + deeper.len();
                return (sel.range(), text, caret);
            }
            let text = format!("\n{deeper}");
            let n = text.len();
            (sel.range(), text, n)
        });
    }

    /// Backspace / Delete. On an empty caret, remove one grapheme (or word with Ctrl) to the side;
    /// on a selection, remove the selection.
    fn delete_side(&mut self, buffer: &mut Buffer, forward: bool, word: bool, now: f64) {
        // Word-deletes and SELECTION-deletes are `Other` so each is its own undo step (JetBrains:
        // deleting a selection never joins a backspace run — mirrors the typing-over-selection
        // guard); plain char-deletes coalesce by direction.
        let has_selection = self.selections.ranges.iter().any(|s| !s.is_empty());
        let kind = if word || has_selection {
            EditKind::Other
        } else if forward {
            EditKind::DeleteFwd
        } else {
            EditKind::DeleteBack
        };
        let soft_tab = !forward && !word;
        self.apply_edits(buffer, kind, now, move |sel, rope| {
            if !sel.is_empty() {
                return (sel.range(), String::new());
            }
            let h = sel.head;
            // Backspace inside the LEADING whitespace removes a whole indent unit, so one press
            // undoes one Tab instead of leaving the caret stranded mid-indent. Only when
            // everything to the left is whitespace — after real code, backspace is per-character.
            if soft_tab && h > 0 {
                let line = rope.byte_to_line(h);
                let ls = rope.line_to_byte(line);
                let col = h - ls;
                if col > 0 && col % TAB.len() == 0 {
                    let head: String = rope.byte_slice(ls..h).into();
                    if head.chars().all(|c| c == ' ') {
                        return (h - TAB.len()..h, String::new());
                    }
                }
            }
            let range = match (forward, word) {
                (false, false) => selection::prev_grapheme(rope, h)..h,
                (true, false) => h..selection::next_grapheme(rope, h),
                (false, true) => selection::prev_word(rope, h)..h,
                (true, true) => h..selection::next_word(rope, h),
            };
            (range, String::new())
        });
    }

    /// JetBrains Ctrl+D: duplicate the caret's line(s) below. Multiple carets on the SAME line
    /// duplicate it ONCE (JetBrains behavior) — the first caret to reach a line claims it; the
    /// rest yield a zero-effect edit that apply_edits filters out (their carets still ride the
    /// insertion delta).
    /// Ctrl+Y: delete the whole line(s) the carets sit on, leaving the caret at the same column
    /// on what is now that line. Distinct from a selection delete — no selection is required, and
    /// the line's newline goes with it so the file does not accumulate blanks.
    pub fn delete_lines(&mut self, buffer: &mut Buffer, now: f64) {
        let claimed = std::cell::RefCell::new(std::collections::HashSet::new());
        self.apply_edits(buffer, EditKind::Other, now, |sel, rope| {
            // A selection spanning several lines deletes all of them; a bare caret deletes one.
            let r = sel.range();
            let first = rope.byte_to_line(r.start);
            let last = rope.byte_to_line(r.end);
            if !claimed.borrow_mut().insert(first) {
                return (r.start..r.start, String::new()); // another caret owns this line
            }
            let start = rope.line_to_byte(first);
            let end = match last + 1 < rope.len_lines() {
                true => rope.line_to_byte(last + 1),
                // Last line with no trailing newline: take the preceding newline instead, or the
                // file would keep a stray empty line forever.
                false => rope.len_bytes(),
            };
            let start = match end == rope.len_bytes() && start > 0 {
                true => start - 1,
                false => start,
            };
            (start..end, String::new())
        });
    }

    /// Ctrl+Shift+U: lower -> UPPER -> lower on the selection, or on the word under a bare caret.
    /// The direction is decided ONCE for the whole edit from the first affected text, so a mixed
    /// selection flips as a unit rather than each caret disagreeing.
    pub fn toggle_case(&mut self, buffer: &mut Buffer, now: f64) {
        let rope = buffer.rope();
        let target = |sel: &Selection, rope: &Rope| -> Range<usize> {
            match sel.is_empty() {
                true => selection::word_range(rope, sel.head),
                false => sel.range(),
            }
        };
        // "Has any lowercase" -> uppercase it; otherwise lowercase. Matches the intuition that
        // the first press on ordinary code SHOUTS, and the second press undoes it.
        let to_upper = self
            .selections
            .ranges
            .iter()
            .map(|s| target(s, rope))
            .any(|r| rope.byte_slice(r).chars().any(char::is_lowercase));
        self.apply_edits(buffer, EditKind::Other, now, move |sel, rope| {
            let r = target(sel, rope);
            let text: String = rope.byte_slice(r.clone()).into();
            let out = match to_upper {
                true => text.to_uppercase(),
                false => text.to_lowercase(),
            };
            (r, out)
        });
    }

    /// Sort the lines spanned by the selection alphabetically (byte order, stable). A bare caret
    /// sorts nothing — sorting the whole file from a stray keystroke would be catastrophic and
    /// hard to notice.
    pub fn sort_lines(&mut self, buffer: &mut Buffer, now: f64, reverse: bool) {
        let rope = buffer.rope();
        let Some(sel) = self.selections.ranges.iter().find(|s| !s.is_empty()) else { return };
        let r = sel.range();
        let first = rope.byte_to_line(r.start);
        let mut last = rope.byte_to_line(r.end);
        // A selection ending at column 0 does not include that line (same rule as indent_lines).
        if last > first && rope.line_to_byte(last) == r.end {
            last -= 1;
        }
        if last <= first {
            return; // one line is already sorted
        }
        let start = rope.line_to_byte(first);
        let end = match last + 1 < rope.len_lines() {
            true => rope.line_to_byte(last + 1),
            false => rope.len_bytes(),
        };
        let block: String = rope.byte_slice(start..end).into();
        let had_trailing_newline = block.ends_with('\n');
        let mut lines: Vec<&str> = block.lines().collect();
        lines.sort();
        if reverse {
            lines.reverse();
        }
        let mut out = lines.join("\n");
        if had_trailing_newline {
            out.push('\n');
        }
        self.apply_edits(buffer, EditKind::Other, now, move |_, _| (start..end, out.clone()));
    }

    /// Alt+Shift+G: put a caret at the end of every line the selection spans, then drop the
    /// selection. The fastest way into a column edit over a block.
    pub fn carets_to_line_ends(&mut self, buffer: &mut Buffer) {
        let rope = buffer.rope();
        let mut lines = std::collections::BTreeSet::new();
        for sel in &self.selections.ranges {
            let r = sel.range();
            let first = rope.byte_to_line(r.start);
            let mut last = rope.byte_to_line(r.end);
            if last > first && rope.line_to_byte(last) == r.end {
                last -= 1;
            }
            for l in first..=last {
                lines.insert(l);
            }
        }
        if lines.len() < 2 {
            return; // one line: nothing multi-caret about it
        }
        let ranges: Vec<Selection> = lines
            .into_iter()
            .map(|l| {
                let start = rope.line_to_byte(l);
                let len = rope.line(l).len_bytes();
                let text: String = rope.line(l).into();
                // Land BEFORE the newline, not after it.
                let trimmed = text.trim_end_matches(['\n', '\r']).len();
                Selection::at(start + trimmed.min(len))
            })
            .collect();
        self.selections = Selections { primary: ranges.len() - 1, ranges };
        buffer.seal();
    }

    fn duplicate_lines(&mut self, buffer: &mut Buffer, now: f64) {
        let claimed = std::cell::RefCell::new(std::collections::HashSet::new());
        self.apply_edits(buffer, EditKind::Duplicate, now, |sel, rope| {
            let line = rope.byte_to_line(sel.range().start);
            if !claimed.borrow_mut().insert(line) {
                let p = sel.range().start;
                return (p..p, String::new()); // already duplicated by an earlier caret here
            }
            let r = selection::line_range(rope, sel.range().start);
            let line_text: String = rope.byte_slice(r.clone()).into();
            let dup = if line_text.ends_with('\n') {
                line_text.clone()
            } else {
                format!("\n{line_text}") // last line without a trailing newline
            };
            (r.end..r.end, dup)
        });
    }

    /// Tab/Shift+Tab block indent: add or strip one indent level ([`TAB`]) at the start of every
    /// line spanned by any selection. Unlike [`EditorView::apply_edits`] the selections are
    /// PRESERVED (shifted through the edit), not collapsed to carets — JetBrains keeps the block
    /// selected so you can Tab repeatedly.
    fn indent_lines(&mut self, buffer: &mut Buffer, unindent: bool, now: f64) {
        let rope = buffer.rope();
        // Lines spanned by the selections. A selection ending exactly at a line's column 0 does
        // NOT include that line (select two lines via triple-click → only those two indent).
        let mut lines = std::collections::BTreeSet::new();
        for sel in &self.selections.ranges {
            let r = sel.range();
            let end = if r.end > r.start && rope.line_to_byte(rope.byte_to_line(r.end)) == r.end {
                r.end - 1
            } else {
                r.end
            };
            for line in rope.byte_to_line(r.start)..=rope.byte_to_line(end) {
                lines.insert(line);
            }
        }

        let mut changes: Vec<Change> = Vec::new();
        for &line in &lines {
            let start = rope.line_to_byte(line);
            if unindent {
                // Strip up to one indent level: a leading tab, or up to TAB.len() spaces.
                let mut n = 0usize;
                for (i, ch) in rope.line(line).chars().take(TAB.len()).enumerate() {
                    match ch {
                        '\t' if i == 0 => {
                            n = 1;
                            break;
                        }
                        ' ' => n += 1,
                        _ => break,
                    }
                }
                if n > 0 {
                    changes.push(Change { start, end: start + n, text: String::new() });
                }
            } else {
                // Skip content-empty lines — indenting them would only add trailing whitespace.
                let is_empty = rope.line(line).chars().all(|c| c == '\n' || c == '\r');
                if !is_empty {
                    changes.push(Change { start, end: start, text: TAB.to_string() });
                }
            }
        }
        if changes.is_empty() {
            return;
        }

        // Map a byte offset through the (sorted) changes: inserts at P shift offsets STRICTLY
        // after P (so a block selection anchored at column 0 grows to include the new indent);
        // deletions collapse offsets inside the removed span onto its start.
        let map = |pos: usize| -> usize {
            let mut delta: isize = 0;
            for c in &changes {
                if c.start >= pos {
                    break;
                }
                let removed = c.end - c.start;
                if pos < c.end {
                    // Inside a removed span: collapse to its start.
                    return (c.start as isize + delta) as usize;
                }
                delta += c.text.len() as isize - removed as isize;
            }
            (pos as isize + delta) as usize
        };
        let before = self.selections.snapshot();
        let mut new_sel = self.selections.clone();
        for sel in &mut new_sel.ranges {
            sel.anchor = map(sel.anchor);
            sel.head = map(sel.head);
            sel.goal_col = None;
        }
        new_sel.merge_overlaps();
        let after = new_sel.snapshot();

        let carets = self.selections.ranges.len();
        let tx = Transaction { changes };
        let pre = buffer.rope().clone();
        buffer.record(&tx, EditMeta { kind: EditKind::Other, carets, time: now, before, after });
        if let Some(syn) = &mut self.syntax {
            syn.edited(buffer.rope(), &tx.changes);
        }
        self.edits_out.push((pre, tx));
        self.selections = new_sel;
    }

    /// Record a structural multi-line edit built as a set of [`Change`]s, with an explicit new
    /// selection set — the shared tail of comment-toggle / move-line / join-line. Mirrors the tail
    /// of [`EditorView::apply_edits`]: files the transaction (undo + caret snapshots), keeps
    /// tree-sitter and the LSP `didChange` queue (`edits_out`) in sync, installs the new selections.
    fn commit_structural(
        &mut self,
        buffer: &mut Buffer,
        changes: Vec<Change>,
        kind: EditKind,
        now: f64,
        new_sel: Selections,
    ) {
        if changes.is_empty() {
            return;
        }
        let before = self.selections.snapshot();
        let after = new_sel.snapshot();
        let carets = self.selections.ranges.len();
        let tx = Transaction { changes };
        let pre = buffer.rope().clone();
        buffer.record(&tx, EditMeta { kind, carets, time: now, before, after });
        if let Some(syn) = &mut self.syntax {
            syn.edited(buffer.rope(), &tx.changes);
        }
        self.edits_out.push((pre, tx));
        self.selections = new_sel;
    }

    /// The distinct line numbers any selection touches — the target set for line-oriented commands.
    /// A selection ending exactly at a line's column 0 does NOT pull in that line (matches
    /// [`EditorView::indent_lines`]): select two lines and only those two are affected.
    fn spanned_lines(&self, rope: &Rope) -> Vec<usize> {
        let mut lines = std::collections::BTreeSet::new();
        for sel in &self.selections.ranges {
            let r = sel.range();
            let end = if r.end > r.start && rope.line_to_byte(rope.byte_to_line(r.end)) == r.end {
                r.end - 1
            } else {
                r.end
            };
            for line in rope.byte_to_line(r.start)..=rope.byte_to_line(end) {
                lines.insert(line);
            }
        }
        lines.into_iter().collect()
    }

    /// Ctrl+/ — toggle a line comment on every spanned line. Uncomments when EVERY non-blank
    /// spanned line is already commented, else comments; the token is inserted at the least-indented
    /// column so a block stays visually aligned (VS Code behavior). Languages with no line comment
    /// (CSS/HTML) fall back to a block comment around the selection.
    pub fn toggle_line_comment(&mut self, buffer: &mut Buffer, now: f64) {
        let Some(tok) = self.lang.and_then(|l| l.line_comment()) else {
            self.toggle_block_comment(buffer, now);
            return;
        };
        let rope = buffer.rope().clone();
        let lines = self.spanned_lines(&rope);

        // Per non-blank line: (line_start_byte, leading-ws byte count, the line's text).
        let mut targets: Vec<(usize, usize, String)> = Vec::new();
        let mut min_indent = usize::MAX;
        for &line in &lines {
            let text: String = rope.line(line).chars().collect();
            let ws = text.bytes().take_while(|b| *b == b' ' || *b == b'\t').count();
            let blank = text[ws..].trim_end_matches(['\n', '\r']).is_empty();
            if blank {
                continue;
            }
            min_indent = min_indent.min(ws);
            targets.push((rope.line_to_byte(line), ws, text));
        }
        if targets.is_empty() {
            return;
        }
        let all_commented = targets.iter().all(|(_, ws, text)| text[*ws..].starts_with(tok));

        let mut changes: Vec<Change> = Vec::new();
        for (start, ws, text) in &targets {
            if all_commented {
                // Remove the token where it sits (after this line's own indent) plus ONE following
                // space, restoring exactly what commenting inserted.
                let at = start + ws;
                let after = ws + tok.len();
                let extra = usize::from(text[after..].starts_with(' '));
                changes.push(Change { start: at, end: at + tok.len() + extra, text: String::new() });
            } else {
                // Insert at the shared least-indent column so the tokens line up.
                let at = start + min_indent;
                changes.push(Change { start: at, end: at, text: format!("{tok} ") });
            }
        }
        self.commit_line_comment(buffer, changes, now);
    }

    /// Ctrl+Shift+/ (and the CSS/HTML Ctrl+/ fallback) — wrap the selection in block-comment
    /// delimiters, or unwrap when it is already exactly wrapped. Operates on the primary selection;
    /// an empty selection wraps the current line.
    pub fn toggle_block_comment(&mut self, buffer: &mut Buffer, now: f64) {
        let Some((open, close)) = self.lang.and_then(|l| l.block_comment()) else { return };
        let rope = buffer.rope().clone();
        let prim = self.selections.primary().range();
        let span = if prim.start == prim.end {
            selection::line_range(&rope, prim.start)
        } else {
            prim.clone()
        };
        // Trim a trailing newline from a line-range span so the close delimiter hugs the text.
        let end = if span.end > span.start && rope.byte(span.end - 1) == b'\n' {
            span.end - 1
        } else {
            span.end
        };
        let inner: String = rope.byte_slice(span.start..end).chars().collect();
        let trimmed = inner.trim();
        let changes = if trimmed.starts_with(open) && trimmed.ends_with(close) && trimmed.len() >= open.len() + close.len() {
            // Unwrap: delete each delimiter PLUS the one padding space wrapping added, so a
            // wrap→unwrap round-trip is exact.
            let io = inner.find(open).unwrap();
            let ic = inner.rfind(close).unwrap();
            let open_space = usize::from(inner[io + open.len()..].starts_with(' '));
            let close_space = usize::from(inner[..ic].ends_with(' '));
            let o = span.start + io;
            let c = span.start + ic;
            vec![
                Change { start: c - close_space, end: c + close.len(), text: String::new() },
                Change { start: o, end: o + open.len() + open_space, text: String::new() },
            ]
        } else {
            vec![
                Change { start: end, end, text: format!(" {close}") },
                Change { start: span.start, end: span.start, text: format!("{open} ") },
            ]
        };
        let mut sorted = changes;
        sorted.sort_by_key(|c| c.start);
        let new_head = map_offset(self.selections.primary().head, &sorted);
        let new_sel = Selections { primary: 0, ranges: vec![Selection::at(new_head)] };
        self.commit_structural(buffer, sorted, EditKind::Other, now, new_sel);
    }

    /// Shared tail of [`EditorView::toggle_line_comment`]: sort the per-line changes, remap every
    /// selection through them (so the block stays selected — Ctrl+/ again toggles back), commit.
    fn commit_line_comment(&mut self, buffer: &mut Buffer, mut changes: Vec<Change>, now: f64) {
        changes.sort_by_key(|c| c.start);
        let mut new_sel = self.selections.clone();
        for sel in &mut new_sel.ranges {
            sel.anchor = map_offset(sel.anchor, &changes);
            sel.head = map_offset(sel.head, &changes);
            sel.goal_col = None;
        }
        new_sel.merge_overlaps();
        self.commit_structural(buffer, changes, EditKind::Other, now, new_sel);
    }

    /// Alt+Shift+Up / Alt+Shift+Down — move the block of lines spanned by the selection up or down
    /// past its neighbor, carrying the selection with it (JetBrains/VS Code). A no-op at the top
    /// (moving up) or bottom (moving down) edge.
    /// Ctrl+Shift+Up/Down: move the whole STATEMENT the caret is in, over its neighbour.
    ///
    /// Distinct from move-line ([`Self::move_lines`], Alt+Shift+Up/Down), which shifts one
    /// physical line and happily rips a `}` off its block or drags a line out of an `if` body.
    /// This moves the syntax node — a whole `if`, a whole loop, a whole declaration — and swaps it
    /// with the sibling statement on the other side, so the code stays valid at every step.
    ///
    /// Falls back to a line move when there is no parse tree: doing nothing would make the
    /// keystroke feel broken in a file the editor simply cannot parse.
    pub fn move_statement(&mut self, buffer: &mut Buffer, down: bool, now: f64) {
        let rope = buffer.rope().clone();
        let Some(syn) = &self.syntax else {
            return self.move_lines(buffer, down, now);
        };
        let caret = self.selections.primary().head;
        // The outermost node that still starts on the caret's own line: for a multi-line `if`
        // this is the whole statement, not the condition the caret happens to sit in.
        let line = rope.byte_to_line(caret);
        let line_start = rope.line_to_byte(line);
        let mut node = syn.tree.root_node().named_descendant_for_byte_range(caret, caret);
        let mut stmt = None;
        while let Some(n) = node {
            if n.start_byte() >= line_start && n.parent().is_some() {
                stmt = Some(n);
            }
            node = n.parent();
        }
        let Some(stmt) = stmt else { return self.move_lines(buffer, down, now) };
        // The sibling to swap with, skipping nothing: comments and attributes are named siblings
        // and moving past them silently would reorder code and its comment independently.
        let sibling = match down {
            true => stmt.next_named_sibling(),
            false => stmt.prev_named_sibling(),
        };
        let Some(sib) = sibling else {
            // No sibling that way — at the top or bottom of its block. A line move here would
            // push the statement out of its block, so decline instead.
            return;
        };
        let (a, b) = match down {
            true => (stmt, sib),
            false => (sib, stmt),
        };
        // Swap whole LINES spanning each node, so indentation and trailing comments travel with
        // them and the result never lands mid-line.
        let a_start = rope.line_to_byte(rope.byte_to_line(a.start_byte()));
        let a_end = line_content_end_at(&rope, a.end_byte());
        let b_start = rope.line_to_byte(rope.byte_to_line(b.start_byte()));
        let b_end = line_content_end_at(&rope, b.end_byte());
        if a_end > b_start {
            return; // overlapping spans (a node sharing a line with its sibling) — not safe
        }
        let a_text: String = rope.byte_slice(a_start..a_end).into();
        let mid: String = rope.byte_slice(a_end..b_start).into();
        let b_text: String = rope.byte_slice(b_start..b_end).into();
        let combined = format!("{b_text}{mid}{a_text}");
        // Keep the caret on the statement the user moved, at the same offset within it.
        let within = caret.saturating_sub(if down { a_start } else { b_start });
        let new_caret = match down {
            true => a_start + b_text.len() + mid.len() + within,
            false => a_start + within,
        };
        self.apply_edits_caret(buffer, EditKind::Other, now, move |_, _| {
            (a_start..b_end, combined.clone(), new_caret - a_start)
        });
    }

    pub fn move_lines(&mut self, buffer: &mut Buffer, down: bool, now: f64) {
        let rope = buffer.rope().clone();
        let lines = self.spanned_lines(&rope);
        let (Some(&first), Some(&last)) = (lines.first(), lines.last()) else { return };
        if (!down && first == 0) || (down && last >= last_real_line(&rope)) {
            return;
        }

        // Swap two adjacent regions U (upper) and L (lower) → L, U. For a downward move the block
        // is the upper region and its neighbor below is the lower; for upward, vice-versa. `block`
        // is the region whose selection must follow the move.
        let (u_lines, l_lines, block_is_lower) = if down {
            ((first, last), (last + 1, last + 1), false)
        } else {
            ((first - 1, first - 1), (first, last), true)
        };
        let u_start = rope.line_to_byte(u_lines.0);
        let l_end = line_end_byte(&rope, l_lines.1);

        // (content, break) for each region — the break is the ACTUAL "\r\n"/"\n"/"" so CRLF and the
        // unterminated final line survive the swap. Content is sliced up to the break length off the
        // region end, so a CRLF's `\r` lands in the break, not the content (interior breaks of a
        // multi-line block stay in the content).
        let u_brk = line_parts(&rope, u_lines.1).1;
        let u_content: String =
            rope.byte_slice(u_start..line_end_byte(&rope, u_lines.1) - u_brk.len()).chars().collect();
        let l_brk = line_parts(&rope, l_lines.1).1;
        let l_content: String = rope
            .byte_slice(rope.line_to_byte(l_lines.0)..line_end_byte(&rope, l_lines.1) - l_brk.len())
            .chars()
            .collect();

        // Reconstruct L,U. When L had NO trailing break (it was the last line), U becomes last and
        // sheds ITS break, with U's break reused as the L↔U separator — never a fabricated '\n'.
        let (swapped, sep_len) = if l_brk.is_empty() {
            (format!("{l_content}{u_brk}{u_content}"), u_brk.len())
        } else {
            (format!("{l_content}{l_brk}{u_content}{u_brk}"), l_brk.len())
        };

        // The block's forward/backward displacement is the RECONSTRUCTED neighbor length, not the
        // original span length (they differ by a break when the last line participates).
        let shift: isize = if block_is_lower {
            -((u_content.len() + u_brk.len()) as isize) // block moved up to u_start
        } else {
            (l_content.len() + sep_len) as isize // block moved down past L + separator
        };
        let mut new_sel = self.selections.clone();
        for sel in &mut new_sel.ranges {
            sel.anchor = (sel.anchor as isize + shift).max(0) as usize;
            sel.head = (sel.head as isize + shift).max(0) as usize;
            sel.goal_col = None;
        }
        let tx_change = Change { start: u_start, end: l_end, text: swapped };
        self.commit_structural(buffer, vec![tx_change], EditKind::Other, now, new_sel);
    }

    /// Ctrl+Shift+J — join the spanned lines into one, collapsing each line break and the next
    /// line's leading whitespace to a single space (JetBrains). A bare caret joins the current line
    /// with the one below.
    pub fn join_lines(&mut self, buffer: &mut Buffer, now: f64) {
        let rope = buffer.rope().clone();
        let lines = self.spanned_lines(&rope);
        let (Some(&first), Some(&last)) = (lines.first(), lines.last()) else { return };
        // A single line joins with the one below it. Clamp to the last REAL line — the phantom
        // sentinel line after a final newline is not joinable, and treating it as one would delete
        // the file's trailing newline (HIGH bug: it fired on the last text line of every file).
        let last = if first == last { last + 1 } else { last };
        let last = last.min(last_real_line(&rope));
        if last == first {
            return; // nothing below to join
        }

        let mut changes: Vec<Change> = Vec::new();
        for line in first..last {
            let eol = line_content_end(&rope, line); // byte before the '\n'
            let nl_end = rope.line_to_byte(line + 1); // start of the next line
            // Skip the next line's leading whitespace too.
            let mut next_ws = nl_end;
            for ch in rope.line(line + 1).chars() {
                if ch == ' ' || ch == '\t' {
                    next_ws += ch.len_utf8();
                } else {
                    break;
                }
            }
            // One space between non-empty neighbors; nothing if the current line is already empty
            // or ends in whitespace, or the next line's remainder is empty.
            let cur_blank = eol == rope.line_to_byte(line);
            let next_rest_empty = rope.byte(next_ws.min(rope.len_bytes().saturating_sub(1))) == b'\n'
                || next_ws >= rope.len_bytes();
            let sep = if cur_blank || next_rest_empty { "" } else { " " };
            changes.push(Change { start: eol, end: next_ws, text: sep.to_string() });
        }
        if changes.is_empty() {
            return;
        }
        changes.sort_by_key(|c| c.start);
        // Caret lands at the first join point (JetBrains parks it at the seam).
        let caret = map_offset(self.selections.primary().head, &changes);
        let new_sel = Selections { primary: 0, ranges: vec![Selection::at(caret)] };
        self.commit_structural(buffer, changes, EditKind::Other, now, new_sel);
    }

    /// A just-typed printable character, given the chance to auto-pair / surround / skip / suppress
    /// before it inserts literally. Returns true when it fully handled the keystroke (the caller
    /// then skips the normal insert). The multi-caret cases (surround, insert-pair) are handled;
    /// skip-over and the quote heuristics apply to a single caret, where they are unambiguous.
    fn typed_char(&mut self, buffer: &mut Buffer, ch: char, now: f64) -> bool {
        let single = self.selections.ranges.len() == 1;
        let empty = self.selections.ranges.iter().all(|s| s.is_empty());
        let rope = buffer.rope();
        let head = self.selections.primary().head;
        // Read whole CHARACTERS, not bytes: `rope.byte(head-1) as char` would read a UTF-8
        // continuation byte next to non-ASCII text and misjudge the word/quote heuristics below.
        let ci = rope.byte_to_char(head);
        let next = (ci < rope.len_chars()).then(|| rope.char(ci));
        let prev = (ci > 0).then(|| rope.char(ci - 1));

        // Skip OVER a closer/quote you typed right before an auto-inserted one: `(|)` + `)` → `()|`.
        if single && empty && next == Some(ch) && (is_closer(ch) || is_quote(ch)) {
            self.selections.set_single(head + ch.len_utf8());
            buffer.seal();
            return true;
        }

        // Electric `}`: typing it on an otherwise-blank line pulls that line out one level, so
        // a closer lands under its opener instead of under the body. Only on a blank line — a `}`
        // typed after real code is just a character, and re-indenting there would fight the user.
        if single
            && empty
            && ch == '}'
            && self.lang.is_some_and(Lang::brace_indented)
            && next != Some('}')
        {
            let line_ix = rope.byte_to_line(head);
            let line_start = rope.line_to_byte(line_ix);
            let before: String = rope
                .slice(rope.byte_to_char(line_start)..rope.byte_to_char(head))
                .chars()
                .collect();
            if !before.is_empty() && before.chars().all(|c| c == ' ' || c == '\t') {
                let dedented = before.strip_suffix(TAB).map(str::to_string).unwrap_or_else(|| {
                    // Not a full indent unit (hand-aligned, or tabs): drop one whitespace char
                    // rather than refusing, so the closer still moves toward its opener.
                    let mut t = before.clone();
                    t.pop();
                    t
                });
                self.apply_edits(buffer, EditKind::Other, now, move |_, _| {
                    (line_start..head, format!("{dedented}}}"))
                });
                buffer.seal();
                return true;
            }
        }

        let Some(close) = open_to_close(ch) else { return false };

        // A non-empty selection: wrap it (each selection, for multi-caret).
        if !empty {
            self.surround(buffer, ch, close, now);
            return true;
        }
        // Quotes need care: an apostrophe in `don't`, a Rust lifetime `'a`, or a char literal must
        // insert literally, not auto-close. Only auto-close a quote away from word characters.
        if is_quote(ch) {
            let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
            if word(prev) || word(next) || prev == Some(ch) {
                return false;
            }
        }
        self.insert_pair(buffer, ch, close, now);
        true
    }

    /// Backspace inside an empty auto-pair (`(|)`, `"|"`, …) deletes BOTH delimiters — the standard
    /// counterpart to auto-pairing. Single empty caret only; returns true when it fired.
    fn try_delete_pair(&mut self, buffer: &mut Buffer, now: f64) -> bool {
        if self.selections.ranges.len() != 1 || !self.selections.primary().is_empty() {
            return false;
        }
        let rope = buffer.rope();
        let head = self.selections.primary().head;
        if head == 0 || head >= rope.len_bytes() {
            return false;
        }
        let prev = rope.byte(head - 1) as char;
        let next = rope.byte(head) as char;
        if open_to_close(prev) != Some(next) {
            return false;
        }
        let changes = vec![Change { start: head - 1, end: head + next.len_utf8(), text: String::new() }];
        let new_sel = Selections { primary: 0, ranges: vec![Selection::at(head - 1)] };
        self.commit_structural(buffer, changes, EditKind::DeleteBack, now, new_sel);
        true
    }

    /// Insert `open``close` at every (empty) caret, leaving each caret BETWEEN the two.
    fn insert_pair(&mut self, buffer: &mut Buffer, open: char, close: char, now: f64) {
        let mut carets: Vec<usize> = self.selections.ranges.iter().map(|s| s.head).collect();
        carets.sort_unstable();
        let ins = format!("{open}{close}");
        let changes: Vec<Change> =
            carets.iter().map(|&p| Change { start: p, end: p, text: ins.clone() }).collect();
        let mut delta = 0usize;
        let ranges: Vec<Selection> = carets
            .iter()
            .map(|&p| {
                let caret = p + delta + open.len_utf8();
                delta += ins.len();
                Selection::at(caret)
            })
            .collect();
        let new_sel = Selections { primary: ranges.len() - 1, ranges };
        self.commit_structural(buffer, changes, EditKind::InsertText, now, new_sel);
    }

    /// Wrap every selection in `open`…`close`, keeping the inner text selected (JetBrains/VS Code
    /// "surround with"). Also the path for auto-pairing when text is selected.
    fn surround(&mut self, buffer: &mut Buffer, open: char, close: char, now: f64) {
        let mut sels: Vec<(usize, usize)> = self
            .selections
            .ranges
            .iter()
            .map(|s| (s.range().start, s.range().end))
            .collect();
        sels.sort_by_key(|(s, _)| *s);
        let mut changes: Vec<Change> = Vec::new();
        let (ol, cl) = (open.len_utf8(), close.len_utf8());
        let mut new_ranges: Vec<Selection> = Vec::new();
        for (i, (s, e)) in sels.iter().enumerate() {
            changes.push(Change { start: *s, end: *s, text: open.to_string() });
            changes.push(Change { start: *e, end: *e, text: close.to_string() });
            // Earlier selections each added open+close ahead of this one; this one added its open.
            let shift = i * (ol + cl) + ol;
            new_ranges.push(Selection { anchor: s + shift, head: e + shift, goal_col: None });
        }
        changes.sort_by_key(|c| c.start);
        let new_sel = Selections { primary: new_ranges.len() - 1, ranges: new_ranges };
        self.commit_structural(buffer, changes, EditKind::Other, now, new_sel);
    }

    /// Byte offset under the mouse this frame (for the app's LSP hover popup).
    pub fn hovered_byte(&self) -> Option<usize> {
        self.hover_byte
    }

    /// Screen rect of the row under the mouse (see [`Self::hover_row_rect`]) — the hover popup's
    /// anchor. Zero-width: only the x of the pointer and the row's vertical extent are meaningful.
    pub fn hovered_row_rect(&self) -> Option<egui::Rect> {
        self.hover_row_rect
    }

    /// Primary caret byte offset (session persistence).
    pub fn caret_byte(&self) -> usize {
        self.selections.primary().head
    }

    /// The primary selection as an ordered byte range; collapsed (`caret..caret`) when nothing
    /// is selected. Code-action requests need the REAL range — servers only offer
    /// extract/inline refactorings over a non-empty selection.
    pub fn selection_byte_range(&self) -> std::ops::Range<usize> {
        self.selections.primary().range()
    }

    /// Ask the editor to take keyboard focus on its next painted frame (takeover dismissed).
    pub fn grab_focus(&mut self) {
        self.focus_pending = true;
    }

    /// Did the editor widget hold egui keyboard focus on its last painted frame? The app gates
    /// completion-popup key handling on this: Tab/arrows typed into the terminal, find bar, or an
    /// overlay must never accept/steer a completion in the editor behind them.
    pub fn has_focus(&self) -> bool {
        self.focused
    }

    /// The right-edge MINIMAP: per-line density bars + diagnostic marks + a soft viewport lens.
    /// Overlay-painted inside the visible rect; click/drag jumps the scroll. Skipped when the
    /// whole file already fits on screen.
    #[allow(clippy::too_many_arguments)]
    /// The error stripe: a full-file column of problem marks down the right edge.
    ///
    /// Deliberately NOT part of the minimap, which is a sliding window showing the neighbourhood
    /// of the viewport. The stripe's whole value is that it is proportional to the WHOLE file — a
    /// glance tells you there are three errors near the end of a two-thousand-line file, which a
    /// windowed view structurally cannot. Clicking a mark jumps to it.
    fn paint_error_stripe(&mut self, ui: &mut egui::Ui, total: usize, rope: &Rope) {
        const STRIPE_W: f32 = 5.0;
        if self.diagnostics.is_empty() || total == 0 {
            return;
        }
        let vis = ui.clip_rect();
        if vis.height() < 60.0 {
            return;
        }
        let stripe = Rect::from_min_max(
            Pos2::new(vis.right() - STRIPE_W, vis.top()),
            Pos2::new(vis.right(), vis.bottom()),
        );
        let p = ui.painter();
        let h = stripe.height();
        // Marks are 3px tall regardless of file size: one line of a huge file would otherwise be
        // a sub-pixel sliver that never actually paints.
        let mut hit: Option<usize> = None;
        let pointer = ui.input(|i| i.pointer.hover_pos());
        let clicked = ui.input(|i| i.pointer.primary_pressed());
        for d in &self.diagnostics {
            let line = rope.byte_to_line(d.range.start.min(rope.len_bytes()));
            let y = stripe.top() + (line as f32 / total as f32) * h;
            let r = Rect::from_min_max(
                Pos2::new(stripe.left() + 1.0, y),
                Pos2::new(stripe.right() - 1.0, y + 3.0),
            );
            p.rect_filled(r, 1.0, d.color());
            if let Some(pt) = pointer {
                // A generous hit box: a 3px target is not clickable in practice.
                let hot = r.expand2(egui::vec2(2.0, 3.0));
                if hot.contains(pt) && clicked {
                    hit = Some(d.range.start);
                }
            }
        }
        if let Some(byte) = hit {
            self.jump_to(byte, rope);
        }
    }

    fn paint_minimap(
        &mut self,
        ui: &mut egui::Ui,
        rope: &Rope,
        total: usize,
        first: usize,
        last: usize,
        row_h: f32,
        generation: u64,
    ) {
        let map_w = self.mini_w;
        /// Fixed miniature line height — the JetBrains model: the map is a magnified strip that
        /// SLIDES through big files (it shows the neighborhood of the viewport, not a squashed
        /// whole-file view).
        const MINI_H: f32 = 2.0;
        let vis = ui.clip_rect();
        if total == 0 || (last - first) >= total || vis.height() < 60.0 {
            return;
        }
        let now = ui.input(|i| i.time);
        if self.mini.refresh(rope, generation, now) {
            // Owed a throttled rebuild — repaint after the window so the map catches up when the
            // typing burst ends (egui would otherwise idle with a stale miniature).
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(140));
        }
        let map = Rect::from_min_max(
            Pos2::new(vis.right() - map_w, vis.top()),
            Pos2::new(vis.right(), vis.bottom()),
        );
        let p = ui.painter();
        p.rect_filled(map, 0.0, Color32::from_rgba_premultiplied(16, 15, 19, 205));
        p.rect_filled(
            Rect::from_min_max(map.left_top(), Pos2::new(map.left() + 1.0, map.bottom())),
            0.0,
            Color32::from_rgba_premultiplied(18, 18, 18, 18),
        );

        let h = map.height();
        let capacity = (h / MINI_H).floor().max(1.0) as usize;
        // Window start: slide proportionally with the scroll position so the lens travels the
        // full map height exactly as the viewport travels the full file.
        let vis_lines = last - first;
        let start = if total <= capacity {
            0
        } else {
            let denom = (total - vis_lines).max(1) as f32;
            let frac = (first as f32 / denom).clamp(0.0, 1.0);
            (frac * (total - capacity) as f32).round() as usize
        };
        let end = (start + capacity).min(total);
        let y_of = |line: usize| map.top() + (line.saturating_sub(start)) as f32 * MINI_H;

        for line in start..end {
            if let Some(ml) = self.mini.lines.get(line) {
                if ml.len > 0 {
                    let y = y_of(line);
                    let x0 = map.left() + 6.0 + (ml.indent as f32).min(24.0) * 1.4;
                    let w = (ml.len as f32 / 96.0).min(1.0) * (map.right() - x0 - 10.0);
                    p.rect_filled(
                        Rect::from_min_size(Pos2::new(x0, y), egui::vec2(w.max(2.0), MINI_H - 0.6)),
                        0.0,
                        ml.color(),
                    );
                }
            }
        }
        // Diagnostic ticks (only those inside the window).
        for d in &self.diagnostics {
            let dl = rope.byte_to_line(d.range.start.min(rope.len_bytes()));
            if dl >= start && dl < end {
                p.rect_filled(
                    Rect::from_min_size(Pos2::new(map.right() - 5.0, y_of(dl)), egui::vec2(4.0, 2.0)),
                    0.0,
                    d.color(),
                );
            }
        }
        // Viewport lens — soft layered translucency + a quiet border.
        let lens = Rect::from_min_max(
            Pos2::new(map.left() + 1.0, y_of(first.max(start))),
            Pos2::new(map.right(), y_of(last.min(end))),
        );
        for (grow, a) in [(3.0, 8u8), (1.5, 14), (0.0, 22)] {
            p.rect_filled(
                lens.expand2(egui::vec2(0.0, grow)),
                4.0,
                Color32::from_rgba_premultiplied(a, a, a, a),
            );
        }
        p.rect_stroke(
            lens,
            4.0,
            egui::Stroke::new(1.0, Color32::from_rgba_premultiplied(30, 30, 30, 30)),
        );

        // Resize: a 6px grab strip on the map's left edge (drag horizontally).
        let grip = Rect::from_min_max(
            Pos2::new(map.left() - 3.0, map.top()),
            Pos2::new(map.left() + 3.0, map.bottom()),
        );
        let grip_resp = ui.interact(grip, ui.id().with("minimap-resize"), egui::Sense::drag());
        if grip_resp.hovered() || grip_resp.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
            p.rect_filled(
                Rect::from_min_max(map.left_top(), Pos2::new(map.left() + 2.0, map.bottom())),
                0.0,
                Color32::from_rgba_premultiplied(40, 40, 40, 40),
            );
        }
        if grip_resp.dragged() {
            self.mini_w = (self.mini_w - grip_resp.drag_delta().x).clamp(48.0, 240.0);
        }

        // Click / drag on the map body: center the view on the clicked miniature line.
        // clicked_by: real pointer clicks only — never egui's Space/Enter fake click on a
        // focused widget (which would re-center the view mid-typing).
        let resp = ui.interact(map, ui.id().with("minimap"), egui::Sense::click_and_drag());
        if !grip_resp.dragged() && (resp.clicked_by(egui::PointerButton::Primary) || resp.dragged())
        {
            if let Some(pos) = resp.interact_pointer_pos() {
                let target_line = (start as f32 + (pos.y - map.top()) / MINI_H) as usize;
                // Map the clicked doc line into visible-row space so folds don't offset the jump.
                let target_row = self.line_to_row(target_line.min(total.saturating_sub(1)));
                // Center the target ROW in the viewport: subtract half the viewport HEIGHT
                // (the map spans it), not half the doc-LINE span — the two differ under wrap
                // and folds, which mis-landed the jump.
                self.pending_scroll =
                    Some((target_row as f32 * row_h - map.height() / 2.0).max(0.0));
            }
        }
    }

    // ------------------------------------------------------------------------------------------
    // find / replace
    // ------------------------------------------------------------------------------------------

    /// Open the find bar (Ctrl+F) or find+replace (Ctrl+R), seeding the query from a single-line
    /// primary selection — JetBrains behavior.
    fn open_find(&mut self, buffer: &Buffer, replace: bool) {
        let p = self.selections.primary().range();
        if !p.is_empty() && p.len() <= 200 {
            let text = buffer.rope().byte_slice(p).to_string();
            if !text.contains('\n') {
                self.find.query = text;
            }
        }
        self.find.open = true;
        self.find.replace_open = replace;
        self.find.focus_pending = true;
        self.find.computed_for = None; // force refresh
    }

    /// The find (+replace) bar above the text. Runs whether or not the editor has focus.
    fn find_bar_ui(&mut self, ui: &mut egui::Ui, buffer: &mut Buffer) {
        let now = ui.input(|i| i.time);
        self.find.refresh(buffer.rope(), buffer.generation);
        let (esc, f3, shift) =
            ui.input(|i| (i.key_pressed(Key::Escape), i.key_pressed(Key::F3), i.modifiers.shift));

        ui.horizontal(|ui| {
            let qr = ui.add(
                egui::TextEdit::singleline(&mut self.find.query)
                    .hint_text("Find…")
                    .desired_width(220.0)
                    .font(egui::TextStyle::Monospace),
            );
            if self.find.focus_pending {
                qr.request_focus();
                self.find.focus_pending = false;
            }
            if qr.changed() {
                // Find-as-you-type: jump to the first match at/after the caret.
                self.goto_match(buffer, true, false);
            }
            // lost_focus, NOT has_focus: a singleline TextEdit SURRENDERS focus on Enter, so
            // has_focus() is already false on the Enter frame — the jump never fired (find-bar
            // Enter was dead). lost_focus() is true on exactly that frame; focus_pending takes
            // the field back. (Same egui trap as Find-in-Files and the AI panel.)
            if qr.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                self.goto_match(buffer, !shift, true);
                self.find.focus_pending = true; // Enter blurred the field; take it back
            }
            if ui.button("↑").clicked_by(egui::PointerButton::Primary) {
                self.goto_match(buffer, false, true);
            }
            if ui.button("↓").clicked_by(egui::PointerButton::Primary) {
                self.goto_match(buffer, true, true);
            }
            if ui.selectable_label(self.find.case_sensitive, "Cc").on_hover_text("Match case").clicked_by(egui::PointerButton::Primary) {
                self.find.case_sensitive = !self.find.case_sensitive;
                self.goto_match(buffer, true, false);
            }
            if ui
                .selectable_label(self.find.regex, ".*")
                .on_hover_text("Regular expression")
                .clicked_by(egui::PointerButton::Primary)
            {
                self.find.regex = !self.find.regex;
                self.goto_match(buffer, true, false);
            }
            let label = if self.find.bad_regex {
                "bad pattern".to_string()
            } else {
                match (self.find.matches.len(), self.find.current) {
                    (0, _) if self.find.query.is_empty() => String::new(),
                    (0, _) => "0 results".to_string(),
                    (n, Some(c)) => format!("{}/{n}", c + 1),
                    (n, None) => format!("{n} results"),
                }
            };
            ui.colored_label(GUTTER().gamma_multiply(0.8), label);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("✕").clicked_by(egui::PointerButton::Primary) {
                    self.find.open = false;
                }
            });
        });
        if self.find.replace_open {
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.find.replacement)
                        .hint_text("Replace…")
                        .desired_width(220.0)
                        .font(egui::TextStyle::Monospace),
                );
                if ui.button("Replace").clicked_by(egui::PointerButton::Primary) {
                    self.replace_current(buffer, now);
                }
                if ui.button("Replace All").clicked_by(egui::PointerButton::Primary) {
                    self.replace_all(buffer, now);
                }
            });
        }
        ui.separator();

        if esc {
            self.find.open = false;
        }
        if f3 {
            self.goto_match(buffer, !shift, true);
        }
    }

    /// Navigate to a match and select it. `advance`: step strictly past the current position
    /// (F3/Enter); otherwise land on the match at/after the caret (find-as-you-type).
    fn goto_match(&mut self, buffer: &mut Buffer, forward: bool, advance: bool) {
        self.find.refresh(buffer.rope(), buffer.generation);
        let matches = &self.find.matches;
        if matches.is_empty() {
            self.find.current = None;
            return;
        }
        let head = self.selections.primary().range().start;
        let i = if forward {
            let first_after = if advance {
                matches.partition_point(|m| m.start <= head)
            } else {
                matches.partition_point(|m| m.start < head)
            };
            first_after % matches.len() // wrap to the top
        } else {
            let first_at_or_after = matches.partition_point(|m| m.start < head);
            first_at_or_after.checked_sub(1).unwrap_or(matches.len() - 1) // wrap to the bottom
        };
        let m = matches[i].clone();
        self.selections =
            Selections { ranges: vec![Selection { anchor: m.start, head: m.end, goal_col: None }], primary: 0 };
        buffer.seal();
        self.find.current = Some(i);
        // Scroll the match's line a third of the way down the viewport (reveal it if folded).
        let line = buffer.rope().byte_to_line(m.start);
        if self.folds.is_hidden(line) {
            self.folds.regions.retain(|(h, e)| !(line > *h && line <= *e));
        }
        let row = self.line_to_row(line);
        self.pending_scroll = Some((row as f32 * self.last_row_h - 6.0 * self.last_row_h).max(0.0));
    }

    /// Replace the currently selected match (or just navigate to one if none is selected).
    fn replace_current(&mut self, buffer: &mut Buffer, now: f64) {
        self.find.refresh(buffer.rope(), buffer.generation);
        let prim = self.selections.primary().range();
        if !self.find.matches.contains(&prim) {
            self.goto_match(buffer, true, false);
            return;
        }
        let text = self.find.replacement.clone();
        let before = self.selections.snapshot();
        let caret = prim.start + text.len();
        let after = SelectionSnapshot { ranges: vec![(caret, caret)], primary: 0 };
        let tx = Transaction::replace(prim.start, prim.end, text);
        let pre = buffer.rope().clone();
        buffer.record(&tx, EditMeta { kind: EditKind::Other, carets: 1, time: now, before, after: after.clone() });
        if let Some(syn) = &mut self.syntax {
            syn.edited(buffer.rope(), &tx.changes);
        }
        self.edits_out.push((pre, tx));
        self.selections = Selections::from_snapshot(&after);
        self.goto_match(buffer, true, false); // select the next match (refresh sees the new generation)
    }

    /// Replace every match in ONE transaction (one undo step).
    fn replace_all(&mut self, buffer: &mut Buffer, now: f64) {
        self.find.refresh(buffer.rope(), buffer.generation);
        if self.find.matches.is_empty() {
            return;
        }
        let text = self.find.replacement.clone();
        let changes: Vec<Change> = self
            .find
            .matches
            .iter()
            .map(|m| Change { start: m.start, end: m.end, text: text.clone() })
            .collect();
        let before = self.selections.snapshot();
        // Caret lands after the LAST replacement, shifted by all earlier deltas.
        let mut delta: isize = 0;
        let mut caret = 0usize;
        for c in &changes {
            caret = (c.start as isize + delta) as usize + c.text.len();
            delta += c.text.len() as isize - (c.end - c.start) as isize;
        }
        let after = SelectionSnapshot { ranges: vec![(caret, caret)], primary: 0 };
        let carets = changes.len();
        let tx = Transaction { changes };
        let pre = buffer.rope().clone();
        buffer.record(&tx, EditMeta { kind: EditKind::Other, carets, time: now, before, after: after.clone() });
        if let Some(syn) = &mut self.syntax {
            syn.edited(buffer.rope(), &tx.changes);
        }
        self.edits_out.push((pre, tx));
        self.selections = Selections::from_snapshot(&after);
    }

    /// Paint every visible match with a dim wash; the current match gets an outline.
    /// The Ctrl+Shift+F7 wash. Deliberately a different tint from find matches — the two can be
    /// on screen together and must stay distinguishable.
    fn paint_usage_marks(
        marks: &[Range<usize>],
        painter: &egui::Painter,
        geoms: &[LineGeom],
        rope: &Rope,
        text_left: f32,
        row_h: f32,
    ) {
        const WASH: Color32 = Color32::from_rgba_premultiplied(20, 38, 30, 52);
        for geom in geoms {
            let line_start = rope.line_to_byte(geom.line);
            let line_end = line_start + rope.line(geom.line).len_bytes();
            for m in marks.iter().filter(|m| m.start < line_end && m.end > line_start) {
                let s = m.start.max(line_start);
                let e = m.end.min(line_end);
                let (sx, sy) = Self::caret_xy(geom, Self::cidx_of(rope, geom, s));
                let (ex, _) = Self::caret_xy(geom, Self::cidx_of(rope, geom, e));
                if ex > sx {
                    painter.rect_filled(
                        egui::Rect::from_min_max(
                            Pos2::new(text_left + sx, geom.top + sy),
                            Pos2::new(text_left + ex, geom.top + sy + row_h),
                        ),
                        2.0,
                        WASH,
                    );
                }
            }
        }
    }

    fn paint_find_matches(
        find: &FindState,
        painter: &egui::Painter,
        geoms: &[LineGeom],
        rope: &Rope,
        text_left: f32,
        row_h: f32,
    ) {
        // ≈ rgba(217,164,65,45) unmultiplied, correctly premultiplied for the const context.
        const WASH: Color32 = Color32::from_rgba_premultiplied(38, 29, 11, 45);
        for geom in geoms {
            let line_start = rope.line_to_byte(geom.line);
            let line_end = line_start + rope.line(geom.line).len_bytes();
            // First match that could touch this line.
            let mut i = find.matches.partition_point(|m| m.end <= line_start);
            while i < find.matches.len() && find.matches[i].start < line_end {
                let m = &find.matches[i];
                // caret_xy for the START's sub-row y (was always geom.top = sub-row 0, wrong on
                // wrapped lines). A match spanning a wrap boundary still paints one rect on its
                // start row — the multi-sub-row rect rework is the separate selection-highlight
                // finding; this at least lands it on the right row for the common same-row case.
                let (s, sy) = Self::caret_xy(geom, Self::cidx_of(rope, geom, m.start.max(line_start)));
                let e = Self::caret_x(geom, Self::cidx_of(rope, geom, m.end.min(line_end)));
                let e = if e > s { e } else { s + row_h * 0.5 };
                let rect = Rect::from_min_max(
                    Pos2::new(text_left + s, geom.top + sy),
                    Pos2::new(text_left + e, geom.top + sy + row_h),
                );
                painter.rect_filled(rect, 2.0, WASH);
                if find.current == Some(i) {
                    painter.rect_stroke(rect, 2.0, egui::Stroke::new(1.0, CARET()));
                }
                i += 1;
            }
        }
    }

    fn undo(&mut self, buffer: &mut Buffer) {
        self.snippet = None; // history jumps invalidate the session's offsets
        let pre = buffer.rope().clone();
        let mut applied: Vec<Change> = Vec::new();
        if let Some(snap) = buffer.undo_with(|_, changes| applied = changes.to_vec()) {
            let tx = Transaction { changes: applied };
            // Keep folds hiding the same lines after the history jump — apply_edits/
            // apply_external remap on every OTHER edit path; undo/redo used to skip it, so
            // an undo across a line-count change left folds pointing at the wrong lines.
            self.folds.remap(&pre, &tx);
            self.edits_out.push((pre, tx));
            self.after_history_edit(buffer, &snap);
        }
    }

    fn redo(&mut self, buffer: &mut Buffer) {
        self.snippet = None; // history jumps invalidate the session's offsets
        let pre = buffer.rope().clone();
        let mut applied: Vec<Change> = Vec::new();
        if let Some(snap) = buffer.redo_with(|_, changes| applied = changes.to_vec()) {
            let tx = Transaction { changes: applied };
            self.folds.remap(&pre, &tx);
            self.edits_out.push((pre, tx));
            self.after_history_edit(buffer, &snap);
        }
    }

    // ------------------------------------------------------------------------------------------
    // external consumers (LSP)
    // ------------------------------------------------------------------------------------------

    /// Drain the applied-edit queue: `(rope before the transaction, the transaction)` per edit,
    /// in application order. The app forwards these to the LSP client as incremental didChange.
    pub fn take_edits(&mut self) -> Vec<(Rope, Transaction)> {
        std::mem::take(&mut self.edits_out)
    }

    /// Set the git gutter marks (0 added / 1 modified / 2 deletion-below), sorted by line.
    /// Set (or clear) the inline blame annotation. The app keeps it on the caret line and only
    /// for clean buffers — a dirty buffer's lines have shifted relative to what git blamed.
    /// Returns true when the annotation actually changed (the caller schedules a repaint —
    /// this runs after the paint pass).
    /// Install LSP inlay hints as `(0-based line, merged label)` — one entry per line,
    /// sorted ascending. Empty clears.
    /// Install inline debug values (`(0-based line, label)`, sorted). Empty clears — set on
    /// each debugger stop, cleared on resume/teardown.
    pub fn set_debug_values(&mut self, values: Vec<(usize, String)>) {
        self.debug_values = values;
    }

    pub fn set_inlay_hints(&mut self, hints: Vec<(usize, String)>) {
        self.inlay_hints = hints;
    }

    pub fn set_inline_blame(&mut self, blame: Option<(usize, String)>) -> bool {
        let changed = self.inline_blame != blame;
        self.inline_blame = blame;
        changed
    }

    /// Set (or clear) coverage marks: (0-based line, covered). Sorted for the paint lookup.
    pub fn set_coverage_marks(&mut self, mut marks: Vec<(usize, bool)>) {
        marks.sort_unstable_by_key(|(l, _)| *l);
        self.coverage_marks = marks;
    }

    pub fn set_gutter_marks(&mut self, mut marks: Vec<(usize, u8)>) {
        marks.sort_by_key(|(l, _)| *l);
        self.gutter_marks = marks;
    }

    /// The Ctrl+Click byte from this frame, if any (goto-definition trigger).
    pub fn take_ctrl_click(&mut self) -> Option<usize> {
        self.ctrl_click.take()
    }

    /// The right-click byte from this frame, if any (editor context menu anchor).
    pub fn take_context_click(&mut self) -> Option<usize> {
        self.context_click.take()
    }

    /// Menu edition of Copy: the selection (or current line when empty), for ctx.copy_text.
    pub fn copy_text_for_menu(&self, buffer: &Buffer) -> String {
        let rope = buffer.rope();
        let has_selection = self.selections.ranges.iter().any(|s| !s.is_empty());
        if has_selection {
            self.selections
                .ranges
                .iter()
                .filter(|s| !s.is_empty())
                .map(|s| rope.byte_slice(s.range()).to_string())
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            let r = selection::line_range(rope, self.selections.primary().head);
            rope.byte_slice(r).to_string()
        }
    }

    /// Menu edition of Cut: delete the selection after the app copied it.
    pub fn cut_selection_for_menu(&mut self, buffer: &mut Buffer, now: f64) {
        if self.selections.ranges.iter().any(|s| !s.is_empty()) {
            self.apply_edits(buffer, EditKind::Cut, now, |sel, _| (sel.range(), String::new()));
        }
    }

    /// Menu edition of Paste: insert at every caret / over every selection.
    pub fn paste_for_menu(&mut self, buffer: &mut Buffer, text: &str, now: f64) {
        if !text.is_empty() {
            self.insert(buffer, text, EditKind::Paste, now);
        }
    }

    /// Menu edition of Select All.
    pub fn select_all_for_menu(&mut self, buffer: &Buffer) {
        self.selections.select_all(buffer.rope());
    }

    /// Primary caret's screen position from the last paint (completion popup anchor).
    pub fn caret_screen_pos(&self) -> Option<egui::Pos2> {
        self.caret_pos.map(|(x, y)| Pos2::new(x, y))
    }

    /// Replace `range` with `text` through the normal edit path (completion accept): caret lands
    /// after the inserted text, history/LSP/reparse all flow as if typed.
    /// Expand an LSP snippet in place of `range`: the plain text goes through the normal edit
    /// path (one undo step), the caret lands on the first stop with its placeholder selected,
    /// and Tab walks the remaining stops (see the key arms). A snippet with only the final
    /// caret opens no session.
    pub fn insert_snippet(&mut self, buffer: &mut Buffer, range: Range<usize>, snippet: &str, now: f64) {
        let (plain, stops) = crate::snippet::parse(snippet);
        let base = range.start.min(buffer.len_bytes());
        self.replace_range(buffer, range, &plain, now);
        let abs: Vec<Range<usize>> =
            stops.iter().map(|s| base + s.range.start..base + s.range.end).collect();
        if let Some(first) = abs.first() {
            let len = buffer.len_bytes();
            let (s, e) = (first.start.min(len), first.end.min(len));
            self.selections = Selections {
                ranges: vec![Selection { anchor: s, head: e.max(s), goal_col: None }],
                primary: 0,
            };
        }
        self.snippet = (abs.len() > 1).then_some((abs, 0));
        buffer.seal();
    }

    /// Move the snippet session by `dir` stops, selecting the target's placeholder. Arriving
    /// at the LAST stop (the final caret) ends the session — the next Tab indents normally.
    fn snippet_step(&mut self, buffer: &mut Buffer, dir: isize) {
        let Some((stops, idx)) = &mut self.snippet else { return };
        let next = *idx as isize + dir;
        if next < 0 {
            return;
        }
        let next = next as usize;
        if next >= stops.len() {
            self.snippet = None;
            return;
        }
        *idx = next;
        let r = stops[next].clone();
        let last = next + 1 == stops.len();
        let len = buffer.len_bytes();
        let (s, e) = (r.start.min(len), r.end.min(len));
        self.selections = Selections {
            ranges: vec![Selection { anchor: s, head: e.max(s), goal_col: None }],
            primary: 0,
        };
        buffer.seal();
        if last {
            self.snippet = None;
        }
    }

    /// Shift snippet stops through a transaction (PRE-edit coordinates): edits before a stop
    /// shift it, edits inside one collapse it to the insertion end. Keeps Tab landing on the
    /// right spots while the user fills earlier placeholders.
    fn remap_snippet(&mut self, tx: &Transaction) {
        let Some((stops, _)) = &mut self.snippet else { return };
        // Walk changes back-to-front so each applies in pristine pre-edit coordinates.
        for c in tx.changes.iter().rev() {
            let delta = c.text.len() as isize - (c.end - c.start) as isize;
            for r in stops.iter_mut() {
                if c.end <= r.start {
                    r.start = (r.start as isize + delta).max(0) as usize;
                    r.end = (r.end as isize + delta).max(0) as usize;
                } else if c.start < r.end || (c.start == r.start && r.start == r.end) {
                    // Overlap (or typing exactly at an empty stop): collapse to the insert end.
                    let p = c.start + c.text.len();
                    *r = p..p;
                }
            }
        }
    }

    pub fn replace_range(
        &mut self,
        buffer: &mut Buffer,
        range: Range<usize>,
        text: &str,
        now: f64,
    ) {
        let len = buffer.len_bytes();
        let (start, end) = (range.start.min(len), range.end.min(len));
        self.selections = Selections {
            ranges: vec![Selection { anchor: start, head: end.max(start), goal_col: None }],
            primary: 0,
        };
        self.insert(buffer, text, EditKind::Other, now);
    }

    /// Apply an externally-built transaction (LSP quick fix / workspace edit) through the SAME
    /// machinery as typing: history record, incremental reparse, LSP sync queue, caret clamp.
    /// One undo step; kind Other (never coalesces).
    pub fn apply_external(&mut self, buffer: &mut Buffer, tx: &Transaction, now: f64) {
        if tx.changes.is_empty() {
            return;
        }
        let _ = now;
        let pre = buffer.rope().clone();
        self.folds.remap(&pre, tx); // keep folds hiding the same lines after an external edit
        self.remap_snippet(tx);
        // NOT an undo step. Undoing a reload would put the stale text back in a buffer that then
        // reads as dirty, and the next save would overwrite the newer file on disk with it.
        buffer.replace_from_disk(tx);
        if let Some(syn) = &mut self.syntax {
            syn.edited(buffer.rope(), &tx.changes);
        }
        self.edits_out.push((pre, tx.clone()));
        self.selections.clamp(buffer.rope());
    }

    /// Jump the caret to `byte` and scroll it into view (Problems-panel click, goto-definition).
    pub fn jump_to(&mut self, byte: usize, rope: &Rope) {
        let byte = byte.min(rope.len_bytes());
        self.selections.set_single(byte);
        let line = rope.byte_to_line(byte);
        // Reveal the destination if it's inside a collapsed fold.
        if self.folds.is_hidden(line) {
            self.folds.regions.retain(|(h, e)| !(line > *h && line <= *e));
        }
        let row = self.line_to_row(line);
        self.pending_scroll = Some((row as f32 * self.last_row_h - 6.0 * self.last_row_h).max(0.0));
    }

    /// Replace the diagnostics painted under the text (byte ranges; will be sorted by start,
    /// worst [`ViewDiag::rank`] first on ties — so the first hit at a position IS the worst one,
    /// with NASA/PoT findings weighted as errors).
    pub fn set_diagnostics(&mut self, mut diags: Vec<ViewDiag>) {
        diags.sort_by_key(|d| (d.range.start, d.rank()));
        self.diagnostics = diags;
    }

    /// Paint zigzag underlines for every visible diagnostic; returns the hovered message (if any)
    /// so the caller can pop a tooltip outside the painter borrow.
    fn paint_diagnostics(
        &self,
        painter: &egui::Painter,
        geoms: &[LineGeom],
        rope: &Rope,
        text_left: f32,
        row_h: f32,
        pointer: Option<Pos2>,
    ) -> Option<String> {
        let mut hovered = None;
        for geom in geoms {
            let line_start = rope.line_to_byte(geom.line);
            let line_end = line_start + rope.line(geom.line).len_bytes();
            let mut i = self.diagnostics.partition_point(|d| d.range.end <= line_start);
            while i < self.diagnostics.len() && self.diagnostics[i].range.start < line_end {
                let d = &self.diagnostics[i];
                i += 1;
                if d.range.start >= line_end || d.range.end <= line_start {
                    continue;
                }
                // caret_xy for the start's sub-row y — a squiggle on a wrapped sub-row was drawn
                // under sub-row 0 (wrong visual row).
                let (sx, sy) = Self::caret_xy(geom, Self::cidx_of(rope, geom, d.range.start.max(line_start)));
                let x0 = text_left + sx;
                let x1 = text_left + Self::caret_x(geom, Self::cidx_of(rope, geom, d.range.end.min(line_end)));
                let x1 = x1.max(x0 + 6.0); // minimum visible squiggle
                let row_top = geom.top + sy;
                let y = row_top + row_h - 2.0;
                // Zigzag: 3 px run, ±1.5 px amplitude.
                let mut points = Vec::with_capacity(((x1 - x0) / 3.0) as usize + 2);
                let mut x = x0;
                let mut up = true;
                while x < x1 {
                    points.push(Pos2::new(x, if up { y - 1.5 } else { y + 1.5 }));
                    up = !up;
                    x += 3.0;
                }
                points.push(Pos2::new(x1, if up { y - 1.5 } else { y + 1.5 }));
                painter.add(egui::Shape::line(points, egui::Stroke::new(1.0, d.color())));
                // Gutter dot at the line's worst severity is drawn by the first (worst) diag hit.
                if let Some(p) = pointer {
                    let hit = Rect::from_min_max(Pos2::new(x0, row_top), Pos2::new(x1, row_top + row_h));
                    if hit.contains(p) && hovered.is_none() {
                        hovered = Some(d.message.clone());
                    }
                }
            }
        }
        hovered
    }

    /// After an undo/redo: restore the caret snapshot and rebuild the tree from scratch. The
    /// incremental `syntax.edited` path assumes positions stay valid in the post-edit rope, which
    /// breaks when undo SHRINKS the buffer below a (multi-caret) inverse change's offset — so
    /// undo/redo (a cold, one-per-action path, unlike per-keystroke typing) does a full reparse.
    fn after_history_edit(&mut self, buffer: &Buffer, snap: &crate::buffer::SelectionSnapshot) {
        self.syntax = self.lang.and_then(|l| Syntax::new(l, buffer.rope()));
        self.selections = Selections::from_snapshot(snap);
        self.selections.clamp(buffer.rope());
    }

    fn copy_to_clipboard(&self, ui: &egui::Ui, buffer: &Buffer) {
        let rope = buffer.rope();
        let has_selection = self.selections.ranges.iter().any(|s| !s.is_empty());
        let text = if has_selection {
            self.selections
                .ranges
                .iter()
                .filter(|s| !s.is_empty())
                .map(|s| rope.byte_slice(s.range()).to_string())
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            // No selection: whole current line (JetBrains copies the line).
            let r = selection::line_range(rope, self.selections.primary().head);
            rope.byte_slice(r).to_string()
        };
        ui.ctx().copy_text(text);
    }
}

/// The next char boundary strictly greater than `i` (or `text.len()+1` when `i` is at the end).
/// Used to step a regex search past a zero-width match without splitting a UTF-8 sequence.
fn next_char_boundary(text: &str, i: usize) -> usize {
    let mut j = i + 1;
    while j < text.len() && !text.is_char_boundary(j) {
        j += 1;
    }
    j
}

/// Leading-whitespace width of `line` in visual columns, or `None` when the line is blank
/// (empty or whitespace-only). Tabs advance to the next multiple of 4.
fn indent_cols(rope: &Rope, line: usize) -> Option<usize> {
    let mut cols = 0usize;
    for ch in rope.line(line).chars() {
        match ch {
            ' ' => cols += 1,
            '\t' => cols += 4 - (cols % 4),
            '\n' | '\r' => return None, // reached line end with no content → blank
            _ => return Some(cols),
        }
    }
    None
}

/// Indent-guide depth for `line`: the number of 4-column levels it (or, for a blank line, the
/// shallower of its nearest non-blank neighbours) is nested under. Bridging blanks keeps a guide
/// continuous through the empty lines inside a block. Neighbour search is bounded so a huge run of
/// blanks can't turn one paint into an O(file) scan.
fn guide_depth(rope: &Rope, line: usize, total: usize) -> usize {
    if let Some(cols) = indent_cols(rope, line) {
        return cols / 4;
    }
    const BOUND: usize = 500;
    let up = (1..=line.min(BOUND)).find_map(|d| indent_cols(rope, line - d).map(|c| c / 4));
    let down = (1..=BOUND)
        .filter_map(|d| (line + d < total).then_some(line + d))
        .find_map(|l| indent_cols(rope, l).map(|c| c / 4));
    match (up, down) {
        (Some(u), Some(d)) => u.min(d),
        (Some(u), None) => u,
        (None, Some(d)) => d,
        (None, None) => 0,
    }
}

/// Leading whitespace of the line containing `byte` (for newline auto-indent).
/// The CODE portion of one line: everything before a line comment or a block-comment opener,
/// with string and character literals respected so a delimiter inside quotes never truncates it.
///
/// Single quotes are the trap. In JS `'{'` is a string; in Rust `'a` is a LIFETIME that never
/// closes, and treating it as a string would swallow the rest of the line — including the `{`
/// that decides the indent. So `'` opens a string only where the language says it can, and in
/// C/Rust it is consumed only as a bounded character literal.
fn code_of_line(line: &str, lang: Option<Lang>) -> &str {
    let lc = lang.and_then(|l| l.line_comment());
    let bc = lang.and_then(|l| l.block_comment()).map(|(o, _)| o);
    let sq_string = lang.is_some_and(|l| l.single_quote_is_string());
    let mut i = 0usize;
    let mut quote: Option<char> = None;
    while i < line.len() {
        let Some(ch) = line[i..].chars().next() else { break };
        match quote {
            Some(q) => {
                if ch == '\\' {
                    // Skip the escape AND whatever it escapes, so `"\""` does not close early.
                    i += ch.len_utf8();
                    i += line[i..].chars().next().map_or(0, char::len_utf8);
                    continue;
                }
                if ch == q {
                    quote = None;
                }
                i += ch.len_utf8();
            }
            None => {
                if lc.is_some_and(|t| line[i..].starts_with(t))
                    || bc.is_some_and(|t| line[i..].starts_with(t))
                {
                    return &line[..i];
                }
                if ch == '"' {
                    quote = Some(ch);
                } else if ch == '\'' {
                    if sq_string {
                        quote = Some(ch);
                    } else if let Some(len) = char_literal_len(&line[i..]) {
                        // A complete `'x'` / `'\n'` — step over it whole. A lone `'` (Rust
                        // lifetime, an apostrophe in a comment we have not reached yet) is just
                        // an ordinary character and must not open anything.
                        i += len;
                        continue;
                    }
                }
                i += ch.len_utf8();
            }
        }
    }
    line
}

/// Byte length of a complete character literal at the start of `s` (`'a'`, `'\n'`, `'\\''`),
/// or None when `s` does not begin with one. Bounded on purpose: an unterminated `'` is a
/// lifetime or an apostrophe, never a literal running to end of line.
fn char_literal_len(s: &str) -> Option<usize> {
    let mut it = s.char_indices();
    if it.next()?.1 != '\'' {
        return None;
    }
    let (_, c) = it.next()?;
    if c == '\'' {
        return None; // `''` is not a literal
    }
    if c == '\\' {
        let _ = it.next()?; // the escaped char
    }
    let (i, c) = it.next()?;
    (c == '\'').then_some(i + c.len_utf8())
}

/// The indent a NEW line should carry, given the code before the caret and the current indent.
/// Only the open-brace rule: a trailing `{` opens a block, so the next line steps in one unit.
fn indent_for_new_line(code_before: &str, indent: &str, lang: Option<Lang>) -> String {
    if !lang.is_some_and(Lang::brace_indented) {
        return indent.to_string();
    }
    match code_before.trim_end().ends_with('{') {
        true => format!("{indent}{TAB}"),
        false => indent.to_string(),
    }
}

/// The continuation prefix for a new line inside a block comment, if we are in one.
/// `/* …` → ` * ` (aligned under the opener's slash); `* …` → `* `.
fn block_comment_continuation(code_line: &str, lang: Option<Lang>) -> Option<&'static str> {
    let (open, close) = lang?.block_comment()?;
    if open != "/*" {
        return None; // the alignment below is specific to the /* … */ shape
    }
    let t = code_line.trim_start();
    if t.contains(close) {
        return None; // the comment already ended on this line
    }
    if t.starts_with(open) {
        return Some(" * ");
    }
    t.starts_with('*').then_some("* ")
}

/// The span strictly between a delimited node's first and last character, trimmed of the
/// whitespace that follows an opening brace and precedes a closing one — selecting a block body
/// should not drag in the newline and indent that merely position it.
fn inner_span(rope: &Rope, outer: &Range<usize>) -> Range<usize> {
    if outer.end.saturating_sub(outer.start) < 2 {
        return outer.clone();
    }
    let mut s = outer.start + 1;
    let mut e = outer.end - 1;
    while s < e && rope.byte_slice(s..s + 1).chars().next().is_some_and(char::is_whitespace) {
        s += 1;
    }
    while e > s && rope.byte_slice(e - 1..e).chars().next().is_some_and(char::is_whitespace) {
        e -= 1;
    }
    s..e
}

/// The code part of a line with string and character literal CONTENTS blanked to spaces, so
/// bracket counting cannot be fooled by `puts("(")`. Lengths are preserved, keeping every byte
/// offset in the masked copy valid against the original.
fn mask_literals(line: &str, lang: Option<Lang>) -> String {
    let code = code_of_line(line, lang);
    let sq_string = lang.is_some_and(|l| l.single_quote_is_string());
    let mut out = String::with_capacity(code.len());
    let mut i = 0usize;
    let mut quote: Option<char> = None;
    while i < code.len() {
        let Some(ch) = code[i..].chars().next() else { break };
        let n = ch.len_utf8();
        match quote {
            Some(q) => {
                if ch == '\\' {
                    out.push_str(&" ".repeat(n));
                    i += n;
                    if let Some(c2) = code[i..].chars().next() {
                        out.push_str(&" ".repeat(c2.len_utf8()));
                        i += c2.len_utf8();
                    }
                    continue;
                }
                // Keep the CLOSING delimiter itself so a half-open literal is still visible.
                out.push(if ch == q { ch } else { ' ' });
                if ch == q {
                    quote = None;
                }
            }
            None => {
                if ch == '"' || (ch == '\'' && sq_string) {
                    quote = Some(ch);
                    out.push(ch);
                } else if ch == '\'' {
                    if let Some(len) = char_literal_len(&code[i..]) {
                        // Blank the whole `'x'` run in one go.
                        out.push_str(&" ".repeat(len));
                        i += len;
                        continue;
                    }
                    out.push(ch);
                } else {
                    out.push(ch);
                }
            }
        }
        i += n;
    }
    out
}

/// Does `s` begin with the keyword `kw` as a whole word (`if (` yes, `ifx (` no)?
fn starts_with_keyword(s: &str, kw: &str) -> bool {
    let t = s.trim_start();
    t.strip_prefix(kw).is_some_and(|rest| !rest.starts_with(|c: char| c.is_alphanumeric() || c == '_'))
}

/// Byte offset of the end of the line CONTAINING `byte`, excluding the newline itself.
/// (Distinct from `line_end_byte`, which takes a line index and includes the break.)
fn line_content_end_at(rope: &Rope, byte: usize) -> usize {
    let line = rope.byte_to_line(byte.min(rope.len_bytes()));
    let start = rope.line_to_byte(line);
    let text: String = rope.line(line).into();
    start + text.trim_end_matches(['\n', '\r']).len()
}

/// Prefix every line of a snippet body after the first with `indent`. Template bodies are
/// written flat so they read as code; the caret's own indentation is only known at expansion.
fn reindent_snippet(body: &str, indent: &str) -> String {
    if indent.is_empty() {
        return body.to_string();
    }
    let mut out = String::with_capacity(body.len() + indent.len() * 4);
    for (i, line) in body.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
            if !line.is_empty() {
                out.push_str(indent);
            }
        }
        out.push_str(line);
    }
    out
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

fn leading_ws(rope: &Rope, byte: usize) -> String {
    let line = rope.byte_to_line(byte);
    let mut out = String::new();
    for ch in rope.line(line).chars() {
        if ch == ' ' || ch == '\t' {
            out.push(ch);
        } else {
            break;
        }
    }
    out
}

/// The last line that actually holds text. `len_lines` counts a phantom empty line after a final
/// newline (ropey's sentinel); joining or moving into it is meaningless, so both commands clamp to
/// this. A buffer NOT ending in a newline has its last line as its real last line.
fn last_real_line(rope: &Rope) -> usize {
    let total = rope.len_lines();
    if rope.len_bytes() > 0 && rope.byte(rope.len_bytes() - 1) == b'\n' {
        total.saturating_sub(2)
    } else {
        total.saturating_sub(1)
    }
}

/// A line split into its content and its trailing line break (`"\r\n"`, `"\n"`, or `""` for the
/// final unterminated line) — so a swap can preserve CRLF instead of fabricating a bare `\n`.
fn line_parts(rope: &Rope, line: usize) -> (String, &'static str) {
    let full: String =
        rope.byte_slice(rope.line_to_byte(line)..line_end_byte(rope, line)).chars().collect();
    let brk = if full.ends_with("\r\n") {
        "\r\n"
    } else if full.ends_with('\n') {
        "\n"
    } else {
        ""
    };
    let content = full[..full.len() - brk.len()].to_string();
    (content, brk)
}

/// End byte of `line` INCLUDING its trailing newline — i.e. the start of the next line, or the
/// buffer length for the final line.
fn line_end_byte(rope: &Rope, line: usize) -> usize {
    if line + 1 < rope.len_lines() {
        rope.line_to_byte(line + 1)
    } else {
        rope.len_bytes()
    }
}

/// End byte of `line`'s CONTENT — the byte before its `\n` (or the buffer end for the last line).
fn line_content_end(rope: &Rope, line: usize) -> usize {
    let end = line_end_byte(rope, line);
    if end > rope.line_to_byte(line) && rope.byte(end - 1) == b'\n' {
        end - 1
    } else {
        end
    }
}

/// The auto-close partner of an opening bracket or quote, if any. Quotes are self-closing.
fn open_to_close(ch: char) -> Option<char> {
    match ch {
        '(' => Some(')'),
        '[' => Some(']'),
        '{' => Some('}'),
        '"' => Some('"'),
        '\'' => Some('\''),
        '`' => Some('`'),
        _ => None,
    }
}

fn is_closer(ch: char) -> bool {
    matches!(ch, ')' | ']' | '}')
}

fn is_quote(ch: char) -> bool {
    matches!(ch, '"' | '\'' | '`')
}

/// Map a byte offset through a set of SORTED changes, purely (independent of rope state). Mirrors
/// the caret remap in [`EditorView::indent_lines`]: an insert at P shifts offsets strictly after P;
/// a deletion collapses offsets inside the removed span onto its start.
fn map_offset(pos: usize, changes: &[Change]) -> usize {
    let mut delta: isize = 0;
    for c in changes {
        if c.start >= pos {
            break;
        }
        let removed = c.end - c.start;
        if pos < c.end {
            return (c.start as isize + delta) as usize;
        }
        delta += c.text.len() as isize - removed as isize;
    }
    (pos as isize + delta) as usize
}

/// A `LayoutJob` for one line: highlight spans + rainbow-bracket overrides, gaps in [`TEXT()`],
/// never wrap. Replaces the old highlighted/plain pair — an uncolored line is just empty `spans`
/// (brackets still paint, so plain-text files get rainbow brackets too).
fn line_job(
    text: &str,
    spans: &[(Range<usize>, HighlightKind)],
    brackets: &[(usize, Option<usize>)],
    line_start: usize,
    font: &FontId,
) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    for (r, c) in line_paint_spans(text.len(), spans, brackets, line_start) {
        append(&mut job, &text[r], c, font);
    }
    job
}

/// The final per-line paint list: highlight spans split at every bracket byte so the bracket's
/// depth color overrides whatever the span had (usually `Punctuation`), gaps filled with
/// [`TEXT()`]. `spans` are line-relative (the highlight contract); `brackets` carry ABSOLUTE byte
/// offsets (a [`crate::highlight::BracketIndex::in_range`] slice), rebased via `line_start`.
///
/// INVARIANT (the old highlighted-job one, strengthened): the result is line-relative, sorted,
/// non-overlapping, and GAPLESS over `[0, text_len)` — adjacent same-color runs are merged so
/// bracket-heavy lines don't explode the section count. Bracket splits are always char-safe:
/// bracket bytes are 1-byte ASCII.
fn line_paint_spans(
    text_len: usize,
    spans: &[(Range<usize>, HighlightKind)],
    brackets: &[(usize, Option<usize>)],
    line_start: usize,
) -> Vec<(Range<usize>, Color32)> {
    // 1) Base coverage: clamped highlight spans with TEXT()-colored gaps.
    let mut base: Vec<(Range<usize>, Color32)> = Vec::with_capacity(spans.len() * 2 + 1);
    let mut cursor = 0usize;
    for (range, kind) in spans {
        let start = range.start.min(text_len);
        let end = range.end.min(text_len);
        if start >= end {
            continue;
        }
        if cursor < start {
            base.push((cursor..start, TEXT()));
        }
        base.push((start..end, color(*kind)));
        cursor = end;
    }
    if cursor < text_len {
        base.push((cursor..text_len, TEXT()));
    }

    // 2) Overlay: split each base segment at the bracket bytes it contains.
    let mut out: Vec<(Range<usize>, Color32)> = Vec::with_capacity(base.len() + brackets.len() * 2);
    let mut bi = 0usize; // next unconsumed bracket entry
    for (seg, col) in base {
        let mut pos = seg.start;
        while bi < brackets.len() {
            let (abs, depth) = brackets[bi];
            let Some(off) = abs.checked_sub(line_start) else {
                bi += 1; // before this line's content — defensive skip
                continue;
            };
            if off >= seg.end {
                break; // belongs to a later segment (or past the content) — keep for next seg
            }
            if off >= pos {
                push_merged(&mut out, pos..off, col);
                push_merged(&mut out, off..off + 1, bracket_color(depth));
                pos = off + 1;
            }
            bi += 1;
        }
        push_merged(&mut out, pos..seg.end, col);
    }
    out
}

/// Append `(r, c)`, folding into the previous entry when it's color- and range-adjacent.
fn push_merged(out: &mut Vec<(Range<usize>, Color32)>, r: Range<usize>, c: Color32) {
    if r.start >= r.end {
        return;
    }
    if let Some((last, lc)) = out.last_mut() {
        if *lc == c && last.end == r.start {
            last.end = r.end;
            return;
        }
    }
    out.push((r, c));
}

fn append(job: &mut LayoutJob, text: &str, color: Color32, font: &FontId) {
    job.append(text, 0.0, TextFormat { font_id: font.clone(), color, ..Default::default() });
}

/// One minimap line: indentation, trimmed length, and a coarse lexical kind → color. This is the
/// JetBrains "averaged lines" look: enough structure to navigate by shape and color, no text.
#[derive(Default, Clone, Copy)]
struct MiniLine {
    indent: u8,
    len: u8,
    kind: u8, // 0 code · 1 comment · 2 preproc/attr · 3 string-ish · 4 declaration
}

impl MiniLine {
    fn color(&self) -> Color32 {
        match self.kind {
            1 => Color32::from_rgba_premultiplied(64, 61, 59, 90),    // comment — ash
            2 => Color32::from_rgba_premultiplied(96, 65, 94, 120),   // preproc — plum
            3 => Color32::from_rgba_premultiplied(78, 91, 67, 120),   // string — moss
            4 => Color32::from_rgba_premultiplied(140, 66, 26, 150),  // declaration — orange
            _ => Color32::from_rgba_premultiplied(92, 90, 88, 110),   // code — bone
        }
    }
}

/// Lazily rebuilt (per buffer generation) minimap model.
#[derive(Default)]
struct MiniModel {
    generation: u64,
    built: bool,
    lines: Vec<MiniLine>,
    /// Wall-clock (egui input time) of the last full rebuild — drives the typing-burst throttle.
    last_build: f64,
}

impl MiniModel {
    /// Rebuild the whole-file miniature. This is an O(lines) scan, so during a typing burst it is
    /// THROTTLED to ~8/sec: a fresh keystroke bumps the generation every frame, and rescanning the
    /// entire file on each one is a real per-keystroke cost on big files. Returns true when it
    /// skipped a rebuild it still owes (the caller schedules a catch-up repaint so the map never
    /// stays stale once typing pauses).
    fn refresh(&mut self, rope: &Rope, generation: u64, now: f64) -> bool {
        if self.built && self.generation == generation {
            return false;
        }
        if self.built && now - self.last_build < 0.13 {
            return true; // owe a rebuild, but not this frame
        }
        self.last_build = now;
        self.generation = generation;
        self.built = true;
        self.lines.clear();
        self.lines.reserve(rope.len_lines());
        for line in rope.lines() {
            let mut indent = 0u32;
            let mut started = false;
            let mut len = 0u32;
            let mut first: Option<char> = None;
            let mut second: Option<char> = None;
            let mut has_quote = false;
            for ch in line.chars() {
                match ch {
                    ' ' if !started => indent += 1,
                    '\t' if !started => indent += 4,
                    '\n' | '\r' => break,
                    c => {
                        if !started {
                            started = true;
                            first = Some(c);
                        } else if second.is_none() {
                            second = Some(c);
                        }
                        if c == '"' {
                            has_quote = true;
                        }
                        len += 1;
                    }
                }
            }
            let kind = match (first, second) {
                (Some('/'), Some('/')) | (Some('/'), Some('*')) | (Some('*'), _) => 1,
                (Some('#'), _) => 2,
                _ if has_quote => 3,
                (Some(c), _) if c.is_alphabetic() && len > 2 => {
                    // Declaration-ish starters get the orange shape landmark.
                    let s: String = line.chars().skip_while(|c| c.is_whitespace()).take(7).collect();
                    if s.starts_with("fn ")
                        || s.starts_with("pub ")
                        || s.starts_with("struct")
                        || s.starts_with("impl")
                        || s.starts_with("enum")
                        || s.starts_with("void ")
                        || s.starts_with("int ")
                        || s.starts_with("static")
                        || s.starts_with("typedef")
                    {
                        4
                    } else {
                        0
                    }
                }
                _ => 0,
            };
            self.lines.push(MiniLine {
                indent: indent.min(255) as u8,
                len: len.min(255) as u8,
                kind,
            });
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Monospace column advance: ASCII 1, tab 4, CJK/wide 2, combining 0 — so wrap rows and
    /// caret-follow match egui's galley on lines with wide glyphs.
    #[test]
    fn char_cols_widths() {
        assert_eq!(char_cols('a'), 1);
        assert_eq!(char_cols('\t'), 4);
        assert_eq!(char_cols('\n'), 0);
        assert_eq!(char_cols('東'), 2, "CJK ideograph is double-width");
        assert_eq!(char_cols('한'), 2, "Hangul syllable is double-width");
        assert_eq!(char_cols('\u{0301}'), 0, "combining acute accent has zero width");
        // display_cols sums them: "a東b" = 1 + 2 + 1 = 4 columns, not 3 chars.
        let rope = Rope::from_str("a東b\n");
        assert_eq!(display_cols(&rope, 0), 4);
    }

    /// A view + buffer over `src` (as a .c file so the tree-sitter path is exercised too).
    fn setup(src: &str) -> (EditorView, Buffer) {
        let buffer = Buffer::from_text(src);
        let view = EditorView::new(&buffer, "t.c");
        (view, buffer)
    }

    fn carets(v: &EditorView) -> Vec<usize> {
        v.selections.ranges.iter().map(|s| s.head).collect()
    }

    /// Brute-force reference: the ordered list of visible doc lines under `folds`.
    fn visible_lines(folds: &Folds, total: usize) -> Vec<usize> {
        (0..total).filter(|l| !folds.is_hidden(*l)).collect()
    }

    #[test]
    fn fold_mapping_matches_bruteforce() {
        let total = 20;
        for regions in [
            vec![],
            vec![(2, 5)],
            vec![(2, 4), (7, 9)],
            vec![(0, 3), (10, 15), (17, 19)],
        ] {
            let folds = Folds { regions: regions.clone() };
            let vis = visible_lines(&folds, total);
            assert_eq!(folds.total_rows(total), vis.len(), "total_rows {regions:?}");
            for (row, &line) in vis.iter().enumerate() {
                assert_eq!(folds.line_to_row(line), row, "line_to_row {regions:?} l{line}");
                assert_eq!(folds.row_to_line(row, total), line, "row_to_line {regions:?} r{row}");
            }
        }
    }

    #[test]
    fn row_index_wrap_matches_bruteforce() {
        let rope = Rope::from_str("aaaaa\nbb\n\ncccccccccc\nd\n");
        let folds = Folds { regions: vec![(1, 2)] }; // hide line 2 (the blank)
        let wrap_cols = 3;
        let ri = RowIndex::build(&rope, &folds, wrap_cols);

        // Brute force: the flat row→line sequence.
        let mut flat = Vec::new();
        for line in 0..rope.len_lines() {
            for _ in 0..rows_of(&rope, &folds, wrap_cols, line) {
                flat.push(line);
            }
        }
        assert_eq!(ri.total_rows(), flat.len());
        for (row, &line) in flat.iter().enumerate() {
            assert_eq!(ri.row_to_line(row), line, "row {row}");
        }
        for line in 0..rope.len_lines() {
            if !folds.is_hidden(line) {
                let first = flat.iter().position(|&l| l == line).unwrap();
                assert_eq!(ri.line_to_row(line), first, "line {line}");
            }
        }
    }

    #[test]
    fn display_cols_and_rows_of() {
        let rope = Rope::from_str("ab\n\tx\n\n");
        assert_eq!(display_cols(&rope, 0), 2); // "ab"
        assert_eq!(display_cols(&rope, 1), 5); // tab(→4) + 'x'
        assert_eq!(display_cols(&rope, 2), 0); // empty
        let f = Folds::default();
        assert_eq!(rows_of(&rope, &f, 4, 0), 1); // 2 cols / 4 → 1
        assert_eq!(rows_of(&rope, &f, 4, 1), 2); // 5 cols / 4 → 2
        assert_eq!(rows_of(&rope, &f, 4, 2), 1); // empty still owns a row
    }

    #[test]
    fn fold_at_caret_collapses_a_c_function_and_toggles_back() {
        let src = "int main(void) {\n    int x = 1;\n    return x;\n}\n";
        let (mut v, b) = setup(src);
        // Caret inside the body → fold the enclosing function scope.
        v.jump_to(b.rope().line_to_byte(1), b.rope());
        v.fold_at_caret(&b);
        assert!(!v.folds.regions.is_empty(), "tree-sitter found a foldable scope");
        let (h, e) = v.folds.regions[0];
        assert_eq!(h, 0, "header is the signature line");
        assert!(e >= 3, "fold reaches the closing brace");
        assert!(v.folds.is_hidden(1) && v.folds.is_hidden(2), "body collapsed");
        assert_eq!(v.folds.total_rows(b.rope().len_lines()), b.rope().len_lines() - (e - h));

        // Ctrl+. on the header unfolds.
        v.jump_to(b.rope().line_to_byte(0), b.rope());
        v.toggle_fold_at_caret(&b);
        assert!(v.folds.regions.is_empty(), "toggled back open");
    }

    #[test]
    fn fold_shifts_when_lines_are_inserted_above_it() {
        // Fold "world" body: header at line 3, end at line 5 in a 6-line buffer.
        let src = "l0\nl1\nl2\nhdr\nbody\nend\ntail\n";
        let (mut v, mut b) = setup(src);
        v.folds.toggle(3, 5); // fold lines 4..=5 under header line 3
        assert_eq!(v.folds.regions, vec![(3, 5)]);
        // Insert two lines at the very top (above the fold) — the fold must slide down by 2.
        let tx = Transaction::insert(0, "new0\nnew1\n");
        v.apply_external(&mut b, &tx, 0.0);
        assert_eq!(v.folds.regions, vec![(5, 7)], "fold slid down by the 2 inserted lines");

        // An edit INSIDE the folded body drops the fold (it just opens).
        let inside = b.rope().line_to_byte(6); // now within the fold's body
        let tx2 = Transaction::insert(inside, "x\n");
        v.apply_external(&mut b, &tx2, 0.0);
        assert!(v.folds.regions.is_empty(), "editing inside a fold drops it");
    }

    #[test]
    fn fold_toggle_is_disjoint_and_reversible() {
        let mut f = Folds::default();
        f.toggle(2, 8); // fold outer
        assert!(f.is_header(2));
        f.toggle(4, 6); // inner header is hidden → ignored
        assert_eq!(f.regions, vec![(2, 8)]);
        f.toggle(2, 8); // unfold
        assert!(f.regions.is_empty());
        // Adding an outer fold subsumes an existing inner one.
        f.toggle(4, 6);
        f.toggle(2, 8);
        assert_eq!(f.regions, vec![(2, 8)]);
    }

    #[test]
    fn find_regex_matches_and_bad_pattern() {
        let rope = Rope::from_str("foo1 bar22 baz333\n");
        let mut f = FindState { query: r"\d+".into(), regex: true, ..Default::default() };
        f.refresh(&rope, 0);
        assert!(!f.bad_regex);
        assert_eq!(f.matches, vec![3..4, 8..10, 14..17]); // 1, 22, 333

        // Case-insensitive regex.
        let mut f = FindState { query: "BAR".into(), regex: true, case_sensitive: false, ..Default::default() };
        f.refresh(&rope, 0);
        assert_eq!(f.matches, vec![5..8]);

        // Zero-width pattern must terminate, not spin.
        let mut f = FindState { query: r"\b".into(), regex: true, ..Default::default() };
        f.refresh(&rope, 0);
        assert!(!f.matches.is_empty());

        // A half-typed regex parses as bad, yields no matches, doesn't panic.
        let mut f = FindState { query: "(".into(), regex: true, ..Default::default() };
        f.refresh(&rope, 0);
        assert!(f.bad_regex);
        assert!(f.matches.is_empty());
    }

    #[test]
    fn code_of_line_stops_at_a_line_comment() {
        assert_eq!(code_of_line("int x = 1; // { nope", Some(Lang::C)), "int x = 1; ");
    }

    #[test]
    fn code_of_line_keeps_braces_inside_strings_out_of_it() {
        // The classic false positive: a brace in a string must not open a block.
        assert_eq!(code_of_line(r#"puts("{");"#, Some(Lang::C)), r#"puts("{");"#);
        assert!(!code_of_line(r#"char *s = "{";"#, Some(Lang::C)).trim_end().ends_with('{'));
    }

    #[test]
    fn rust_lifetime_does_not_swallow_the_line() {
        // `'a` never closes. Treating it as a string would hide the trailing brace and silently
        // disable indent for the whole body.
        let line = "fn f<'a>(x: &'a str) {";
        assert!(code_of_line(line, Some(Lang::Rust)).trim_end().ends_with('{'), "{line}");
    }

    #[test]
    fn c_char_literal_brace_is_not_code() {
        // `'{'` is one character, not an opener.
        let line = "if (c == '{') foo();";
        assert_eq!(code_of_line(line, Some(Lang::C)), line);
        assert!(!code_of_line(line, Some(Lang::C)).trim_end().ends_with('{'));
    }

    #[test]
    fn js_single_quoted_string_is_a_string() {
        // Same character, opposite meaning from Rust/C.
        let line = "if (s === '{') {";
        assert!(code_of_line(line, Some(Lang::Js)).trim_end().ends_with('{'));
        assert!(!code_of_line("x('//')", Some(Lang::Js)).is_empty());
        assert_eq!(code_of_line("x('//')", Some(Lang::Js)), "x('//')", "// inside a string");
    }

    #[test]
    fn indent_rules_do_not_apply_to_python() {
        // Python indents by other means; a stray brace must not step it in.
        assert_eq!(indent_for_new_line("d = {", "    ", Some(Lang::Python)), "    ");
        assert_eq!(indent_for_new_line("if (x) {", "    ", Some(Lang::C)), "        ");
    }

    #[test]
    fn block_comment_continuation_shapes() {
        assert_eq!(block_comment_continuation("/* hi", Some(Lang::C)), Some(" * "));
        assert_eq!(block_comment_continuation(" * hi", Some(Lang::C)), Some("* "));
        // Already closed on this line -> not a continuation.
        assert_eq!(block_comment_continuation("/* hi */", Some(Lang::C)), None);
        assert_eq!(block_comment_continuation("int x;", Some(Lang::C)), None);
        assert_eq!(block_comment_continuation("# hi", Some(Lang::Python)), None);
    }

    #[test]
    fn char_literal_len_is_bounded() {
        assert_eq!(char_literal_len("'a'x"), Some(3));
        assert_eq!(char_literal_len(r"'\n'x"), Some(4));
        assert_eq!(char_literal_len("'a"), None, "unterminated is not a literal");
        assert_eq!(char_literal_len("''"), None);
    }

    #[test]
    fn indent_cols_counts_spaces_and_tabs() {
        let r = Rope::from_str("no\n    four\n\tone_tab\n        eight\n   \n");
        assert_eq!(indent_cols(&r, 0), Some(0)); // "no"
        assert_eq!(indent_cols(&r, 1), Some(4)); // 4 spaces
        assert_eq!(indent_cols(&r, 2), Some(4)); // one tab → col 4
        assert_eq!(indent_cols(&r, 3), Some(8)); // 8 spaces
        assert_eq!(indent_cols(&r, 4), None); // whitespace-only → blank
    }

    #[test]
    fn guide_depth_bridges_blank_lines_at_shallower_neighbour() {
        //           line: 0        1            2      3            4
        let r = Rope::from_str("fn f() {\n        deep\n\n    shallow\n}\n");
        let total = r.len_lines();
        assert_eq!(guide_depth(&r, 0, total), 0); // top level
        assert_eq!(guide_depth(&r, 1, total), 2); // 8 spaces → 2 levels
        // Blank line 2 sits between depth-2 and depth-1 → inherits the shallower (1).
        assert_eq!(guide_depth(&r, 2, total), 1);
        assert_eq!(guide_depth(&r, 3, total), 1); // 4 spaces
    }

    /// Type each char of `s` as a separate InsertText edit at the primary caret (times auto-advance
    /// within the coalescing window). Mirrors what handle_keys does per Text event.
    fn type_str(v: &mut EditorView, b: &mut Buffer, s: &str, t0: f64) {
        for (i, ch) in s.chars().enumerate() {
            v.insert(b, &ch.to_string(), EditKind::InsertText, t0 + i as f64 * 0.05);
        }
    }

    #[test]
    fn typing_inserts_and_advances_caret() {
        let (mut v, mut b) = setup("ac");
        v.selections = Selections::single(1);
        v.insert(&mut b, "b", EditKind::InsertText, 0.0);
        assert_eq!(b.rope().to_string(), "abc");
        assert_eq!(carets(&v), vec![2]); // caret after the inserted char
    }

    #[test]
    fn multi_caret_insert_keeps_every_caret_after_its_text() {
        let (mut v, mut b) = setup("x.x.x");
        // carets after each 'x' (bytes 1, 3, 5)
        v.selections = Selections { ranges: vec![Selection::at(1), Selection::at(3), Selection::at(5)], primary: 0 };
        v.insert(&mut b, "Y", EditKind::InsertText, 0.0);
        assert_eq!(b.rope().to_string(), "xY.xY.xY");
        assert_eq!(carets(&v), vec![2, 5, 8]); // shifted by earlier insertions
    }

    #[test]
    fn backspace_removes_grapheme_before_caret() {
        let (mut v, mut b) = setup("héllo"); // é = 2 bytes
        v.selections = Selections::single(3); // after é
        v.delete_side(&mut b, false, false, 0.0);
        assert_eq!(b.rope().to_string(), "hllo");
        assert_eq!(carets(&v), vec![1]);
    }

    #[test]
    fn delete_selection_then_type_replaces() {
        let (mut v, mut b) = setup("hello world");
        v.selections = Selections { ranges: vec![Selection { anchor: 6, head: 11, goal_col: None }], primary: 0 };
        v.insert(&mut b, "there", EditKind::InsertText, 0.0);
        assert_eq!(b.rope().to_string(), "hello there");
    }

    #[test]
    fn newline_copies_leading_indent() {
        let (mut v, mut b) = setup("    foo");
        v.selections = Selections::single(7); // end of "    foo"
        v.insert_newline(&mut b, 0.0);
        assert_eq!(b.rope().to_string(), "    foo\n    ");
        assert_eq!(carets(&v), vec![12]); // after the '\n' + 4-space copied indent
    }

    #[test]
    fn newline_after_open_brace_steps_in() {
        let (mut v, mut b) = setup("int main(void) {");
        v.selections = Selections::single(16);
        v.insert_newline(&mut b, 0.0);
        assert_eq!(b.rope().to_string(), "int main(void) {\n    ");
    }

    #[test]
    fn newline_between_braces_opens_a_body() {
        // `{|}` -> body line, caret on it, closer dedented on its own line.
        let (mut v, mut b) = setup("int main(void) {}");
        v.selections = Selections::single(16); // between { and }
        v.insert_newline(&mut b, 0.0);
        assert_eq!(b.rope().to_string(), "int main(void) {\n    \n}");
        assert_eq!(carets(&v), vec![21], "caret on the BODY line, not after the closer");
    }

    #[test]
    fn newline_inside_a_block_comment_continues_it() {
        let (mut v, mut b) = setup("/* hello");
        v.selections = Selections::single(8);
        v.insert_newline(&mut b, 0.0);
        assert_eq!(b.rope().to_string(), "/* hello\n * ");
    }

    #[test]
    fn newline_after_a_brace_in_a_string_does_not_step_in() {
        // The regression a naive "line ends with {" rule would introduce.
        let (mut v, mut b) = setup("    char *s = \"{\";");
        let n = b.rope().len_bytes();
        v.selections = Selections::single(n);
        v.insert_newline(&mut b, 0.0);
        assert_eq!(b.rope().to_string(), "    char *s = \"{\";\n    ", "indent copied, not deepened");
    }

    #[test]
    fn newline_mid_line_uses_only_the_text_before_the_caret() {
        let (mut v, mut b) = setup("if (x) { y();");
        v.selections = Selections::single(8); // right after `{ `
        v.insert_newline(&mut b, 0.0);
        assert_eq!(b.rope().to_string(), "if (x) {\n     y();");
    }

    #[test]
    fn electric_close_brace_pulls_the_line_out_one_level() {
        let (mut v, mut b) = setup("int f(void) {\n    x();\n    ");
        let n = b.rope().len_bytes();
        v.selections = Selections::single(n);
        assert!(v.typed_char(&mut b, '}', 0.0), "electric arm must claim the keystroke");
        assert_eq!(b.rope().to_string(), "int f(void) {\n    x();\n}");
    }

    #[test]
    fn close_brace_after_code_is_just_a_character() {
        // Re-indenting here would fight the user mid-line.
        let (mut v, mut b) = setup("    x();");
        let n = b.rope().len_bytes();
        v.selections = Selections::single(n);
        v.typed_char(&mut b, '}', 0.0);
        // Either the pair logic or nothing handled it, but the leading indent must survive.
        assert!(b.rope().to_string().starts_with("    x();"), "{}", b.rope());
    }

    fn sel_text(v: &EditorView, b: &Buffer) -> String {
        let r = v.selections.primary().range();
        b.rope().byte_slice(r).into()
    }

    fn tab(v: &mut EditorView, b: &mut Buffer) {
        v.handle_key(b, Key::Tab, egui::Modifiers::default(), 0.0);
    }

    #[test]
    fn tab_expands_a_live_template() {
        let (mut v, mut b) = setup("for");
        v.selections = Selections::single(3);
        tab(&mut v, &mut b);
        let out = b.rope().to_string();
        assert!(out.starts_with("for (int i = 0; i < n; i++) {"), "{out}");
        assert!(!out.contains("${"), "placeholders must be expanded, not literal: {out}");
    }

    #[test]
    fn tab_after_a_non_template_word_still_indents() {
        let (mut v, mut b) = setup("format");
        v.selections = Selections::single(6);
        tab(&mut v, &mut b);
        assert_eq!(b.rope().to_string(), "format    ", "`format` is not `for`");
    }

    #[test]
    fn tab_expands_a_postfix_template_over_the_whole_expression() {
        let (mut v, mut b) = setup("    compute(a, b).if");
        let n = b.rope().len_bytes();
        v.selections = Selections::single(n);
        tab(&mut v, &mut b);
        let out = b.rope().to_string();
        assert!(out.starts_with("    if (compute(a, b)) {"), "{out}");
        // The body must be indented to the line the expansion landed on.
        assert!(out.contains("\n    }"), "closing brace not re-indented: {out:?}");
    }

    #[test]
    fn postfix_wins_over_a_live_template_of_the_same_name() {
        // `x.if` must not expand the bare `if` template and eat the expression.
        let (mut v, mut b) = setup("ready.if");
        let n = b.rope().len_bytes();
        v.selections = Selections::single(n);
        tab(&mut v, &mut b);
        assert!(b.rope().to_string().starts_with("if (ready) {"), "{}", b.rope());
    }

    #[test]
    fn tab_with_a_selection_still_indents_rather_than_expanding() {
        let (mut v, mut b) = setup("for\nbar\n");
        v.selections =
            Selections { ranges: vec![Selection { anchor: 0, head: 7, goal_col: None }], primary: 0 };
        tab(&mut v, &mut b);
        assert!(b.rope().to_string().starts_with("    for"), "{}", b.rope());
    }

    #[test]
    fn move_statement_swaps_whole_multiline_statements() {
        // The case move-LINE gets wrong: it would rip the `}` off the if-block.
        let src = "void f(void)\n{\n    if (a) {\n        one();\n    }\n    two();\n}\n";
        let (mut v, mut b) = setup(src);
        let at = src.find("if (a)").unwrap();
        v.selections = Selections::single(at + 2);
        v.move_statement(&mut b, true, 0.0);
        assert_eq!(
            b.rope().to_string(),
            "void f(void)\n{\n    two();\n    if (a) {\n        one();\n    }\n}\n"
        );
    }

    #[test]
    fn move_statement_back_up_restores_the_original() {
        let src = "void f(void)\n{\n    one();\n    two();\n}\n";
        let (mut v, mut b) = setup(src);
        let at = src.find("one();").unwrap();
        v.selections = Selections::single(at);
        v.move_statement(&mut b, true, 0.0);
        assert_eq!(b.rope().to_string(), "void f(void)\n{\n    two();\n    one();\n}\n");
        v.move_statement(&mut b, false, 1.0);
        assert_eq!(b.rope().to_string(), src, "down then up is a round trip");
    }

    #[test]
    fn move_statement_declines_at_the_end_of_its_block() {
        // A line move here would push the statement out through the closing brace.
        let src = "void f(void)\n{\n    only();\n}\n";
        let (mut v, mut b) = setup(src);
        let at = src.find("only();").unwrap();
        v.selections = Selections::single(at);
        v.move_statement(&mut b, true, 0.0);
        assert_eq!(b.rope().to_string(), src, "no sibling below — must not escape the block");
    }

    #[test]
    fn an_external_reload_is_not_undoable() {
        // Ctrl+Z after a reload would put the STALE text back into a buffer that then reads as
        // dirty, and the next save would overwrite the newer file on disk with it.
        let (mut v, mut b) = setup("old contents\n");
        v.insert(&mut b, "x", EditKind::InsertText, 0.0); // some real history to discard
        let pre_len = b.rope().len_bytes();
        let tx = Transaction::replace(0, pre_len, "new contents from disk\n");
        v.apply_external(&mut b, &tx, 1.0);
        assert_eq!(b.rope().to_string(), "new contents from disk\n");
        v.undo(&mut b);
        assert_eq!(
            b.rope().to_string(),
            "new contents from disk\n",
            "undo must not resurrect pre-reload text"
        );
    }

    #[test]
    fn extract_variable_declares_above_and_substitutes() {
        let (mut v, mut b) = setup("void f(void) {\n    g(a + b * 2);\n}\n");
        let src = b.rope().to_string();
        let s = src.find("a + b * 2").unwrap();
        let e = s + "a + b * 2".len();
        v.selections = Selections { ranges: vec![Selection { anchor: s, head: e, goal_col: None }], primary: 0 };
        let name_range = v.extract_variable(&mut b, "int ", "tmp", 0.0).unwrap();
        assert_eq!(b.rope().to_string(), "void f(void) {\n    int tmp = a + b * 2;\n    g(tmp);\n}\n");
        // The returned span must be the NAME in the new declaration, for an inline rename.
        assert_eq!(&b.rope().to_string()[name_range], "tmp");
    }

    #[test]
    fn extract_variable_undoes_as_one_step() {
        // Both edits must ride one transaction: a half-undone extract does not compile.
        let before = "void f(void) {\n    g(x + 1);\n}\n";
        let (mut v, mut b) = setup(before);
        let s = before.find("x + 1").unwrap();
        v.selections = Selections { ranges: vec![Selection { anchor: s, head: s + 5, goal_col: None }], primary: 0 };
        v.extract_variable(&mut b, "int ", "t", 0.0).unwrap();
        v.undo(&mut b);
        assert_eq!(b.rope().to_string(), before);
    }

    #[test]
    fn extract_variable_needs_a_single_line_selection() {
        let (mut v, mut b) = setup("int x = 1;\n");
        v.selections = Selections::single(4); // no selection
        assert!(v.extract_variable(&mut b, "int ", "t", 0.0).is_none());
        // Multi-line: the shape is not safely guessable.
        let (mut v, mut b) = setup("int x =\n  1;\n");
        v.selections = Selections { ranges: vec![Selection { anchor: 0, head: 11, goal_col: None }], primary: 0 };
        assert!(v.extract_variable(&mut b, "int ", "t", 0.0).is_none());
    }

    fn after_complete(src: &str, caret: usize) -> (String, usize) {
        let (mut v, mut b) = setup(src);
        v.selections = Selections::single(caret);
        v.complete_statement(&mut b, 0.0);
        (b.rope().to_string(), v.selections.primary().head)
    }

    #[test]
    fn complete_statement_adds_the_missing_semicolon() {
        let (out, _) = after_complete("int x = 1", 9);
        assert_eq!(out, "int x = 1;\n");
    }

    #[test]
    fn complete_statement_closes_open_parens_first() {
        let (out, _) = after_complete("foo(bar(1", 9);
        assert_eq!(out, "foo(bar(1));\n");
    }

    #[test]
    fn complete_statement_opens_a_block_for_if() {
        let (out, caret) = after_complete("if (x)", 6);
        assert_eq!(out, "if (x) {\n    \n}");
        // The caret must land on the BODY line, ready to type.
        assert_eq!(&out[..caret], "if (x) {\n    ");
    }

    #[test]
    fn complete_statement_closes_the_condition_then_opens_the_block() {
        let (out, _) = after_complete("if (x == 1", 10);
        assert_eq!(out, "if (x == 1) {\n    \n}");
    }

    #[test]
    fn complete_statement_on_a_finished_line_just_moves_on() {
        let (out, _) = after_complete("int x = 1;", 4);
        assert_eq!(out, "int x = 1;\n");
    }

    #[test]
    fn complete_statement_works_from_mid_line() {
        // Reached for precisely when the caret is NOT at the end.
        let (out, _) = after_complete("int x = 1", 4);
        assert_eq!(out, "int x = 1;\n");
    }

    #[test]
    fn complete_statement_ignores_parens_in_strings() {
        let (out, _) = after_complete(r#"puts("(")"#, 9);
        assert_eq!(out, "puts(\"(\");\n", "a paren inside a string is not unbalanced");
    }

    #[test]
    fn complete_statement_does_nothing_in_python() {
        let buffer = Buffer::from_text("x = 1");
        let mut v = EditorView::new(&buffer, "t.py");
        let mut b = buffer;
        v.selections = Selections::single(5);
        v.complete_statement(&mut b, 0.0);
        assert_eq!(b.rope().to_string(), "x = 1", "no semicolons in Python");
    }

    #[test]
    fn highlight_usages_marks_whole_words_only() {
        let (mut v, mut b) = setup("int count; int counter; count = count + 1;\n");
        let at = b.rope().to_string().find("count").unwrap();
        v.selections = Selections::single(at + 1);
        v.highlight_usages(&b);
        let (_, marks) = v.usage_marks.clone().unwrap();
        let hay = b.rope().to_string();
        for m in &marks {
            assert_eq!(&hay[m.clone()], "count");
        }
        // `count` declared once, assigned once, read once — `counter` must NOT be among them.
        assert_eq!(marks.len(), 3, "got {marks:?}");
    }

    #[test]
    fn highlight_usages_off_a_word_marks_nothing() {
        let (mut v, mut b) = setup("int a;   int b;\n");
        v.selections = Selections::single(7); // whitespace
        v.highlight_usages(&b);
        assert!(v.usage_marks.as_ref().is_none_or(|(_, m)| m.is_empty()));
    }

    #[test]
    fn escape_clears_usage_marks() {
        let (mut v, mut b) = setup("int x; x = 1;\n");
        v.selections = Selections::single(4);
        v.highlight_usages(&b);
        assert!(v.usage_marks_shown());
        v.handle_key(&mut b, Key::Escape, egui::Modifiers::default(), 0.0);
        assert!(!v.usage_marks_shown());
    }

    #[test]
    fn expand_grows_token_then_call_then_statement() {
        let (mut v, mut b) = setup("void f(void) { g(aa, bb); }\n");
        let at = b.rope().to_string().find("aa").unwrap();
        v.selections = Selections::single(at + 1);
        v.expand_selection(&mut b);
        assert_eq!(sel_text(&v, &b), "aa", "first press takes the token");
        let mut seen = vec![sel_text(&v, &b)];
        for _ in 0..4 {
            v.expand_selection(&mut b);
            seen.push(sel_text(&v, &b));
        }
        // Each rung must strictly contain the previous one.
        for w in seen.windows(2) {
            assert!(w[1].len() > w[0].len(), "rung did not grow: {:?} -> {:?}", w[0], w[1]);
        }
        assert!(seen.iter().any(|s| s == "aa, bb"), "list interior expected: {seen:?}");
        assert!(seen.iter().any(|s| s.contains("g(aa, bb)")), "call expected: {seen:?}");
    }

    #[test]
    fn shrink_returns_the_exact_previous_range() {
        let (mut v, mut b) = setup("void f(void) { g(aa, bb); }\n");
        let at = b.rope().to_string().find("bb").unwrap();
        v.selections = Selections::single(at + 1);
        v.expand_selection(&mut b);
        v.expand_selection(&mut b);
        let two = sel_text(&v, &b);
        v.expand_selection(&mut b);
        v.shrink_selection(&mut b);
        assert_eq!(sel_text(&v, &b), two, "shrink must retrace, not recompute");
    }

    #[test]
    fn moving_the_caret_discards_the_ladder() {
        let (mut v, mut b) = setup("void f(void) { g(aa, bb); }\n");
        let at = b.rope().to_string().find("aa").unwrap();
        v.selections = Selections::single(at + 1);
        v.expand_selection(&mut b);
        v.expand_selection(&mut b);
        v.selections = Selections::single(at); // any other motion
        v.shrink_selection(&mut b);
        assert!(v.selections.primary().is_empty(), "a stale ladder must not resurrect a selection");
        v.expand_selection(&mut b);
        assert_eq!(sel_text(&v, &b), "aa", "the next expand starts fresh at the token");
    }

    #[test]
    fn editing_discards_the_ladder() {
        let (mut v, mut b) = setup("void f(void) { g(aa, bb); }\n");
        let at = b.rope().to_string().find("aa").unwrap();
        v.selections = Selections::single(at + 1);
        v.expand_selection(&mut b);
        v.expand_selection(&mut b);
        v.selections = Selections::single(at + 1);
        v.insert(&mut b, "x", EditKind::InsertText, 0.0);
        v.shrink_selection(&mut b);
        assert!(v.selections.primary().is_empty());
    }

    #[test]
    fn expand_inside_a_string_takes_the_contents_before_the_quotes() {
        let (mut v, mut b) = setup("void f(void) { puts(\"abc\"); }\n");
        let at = b.rope().to_string().find("abc").unwrap();
        v.selections = Selections::single(at + 1);
        v.expand_selection(&mut b);
        assert_eq!(sel_text(&v, &b), "abc");
        v.expand_selection(&mut b);
        assert_eq!(sel_text(&v, &b), "\"abc\"", "then the quotes come in");
    }

    #[test]
    fn expand_is_never_a_dead_key_without_a_parse_tree() {
        // A plain-text buffer has no Syntax; Ctrl+W must still do something sensible.
        let buffer = Buffer::from_text("hello world\nsecond line\n");
        let mut v = EditorView::new(&buffer, "notes.txt");
        let mut b = buffer;
        v.selections = Selections::single(2);
        v.expand_selection(&mut b);
        assert_eq!(sel_text(&v, &b), "hello");
        v.expand_selection(&mut b);
        assert!(sel_text(&v, &b).starts_with("hello world"), "then the line");
    }

    #[test]
    fn expand_at_the_root_stops_instead_of_looping() {
        let (mut v, mut b) = setup("int x;\n");
        v.selections = Selections::single(4);
        for _ in 0..12 {
            v.expand_selection(&mut b);
        }
        let r = v.selections.primary().range();
        assert!(r.end <= b.rope().len_bytes(), "must not run past the buffer");
    }

    #[test]
    fn delete_line_removes_the_whole_line_including_its_newline() {
        let (mut v, mut b) = setup("one\ntwo\nthree\n");
        v.selections = Selections::single(5); // on "two"
        v.delete_lines(&mut b, 0.0);
        assert_eq!(b.rope().to_string(), "one\nthree\n");
    }

    #[test]
    fn delete_last_line_takes_the_preceding_newline() {
        // Otherwise the file keeps a stray empty line that nothing can remove.
        let (mut v, mut b) = setup("one\ntwo");
        v.selections = Selections::single(5);
        v.delete_lines(&mut b, 0.0);
        assert_eq!(b.rope().to_string(), "one");
    }

    #[test]
    fn toggle_case_uppercases_then_lowercases() {
        let (mut v, mut b) = setup("hello world");
        v.selections = Selections { ranges: vec![Selection { anchor: 0, head: 5, goal_col: None }], primary: 0 };
        v.toggle_case(&mut b, 0.0);
        assert_eq!(b.rope().to_string(), "HELLO world");
        v.selections = Selections { ranges: vec![Selection { anchor: 0, head: 5, goal_col: None }], primary: 0 };
        v.toggle_case(&mut b, 0.0);
        assert_eq!(b.rope().to_string(), "hello world");
    }

    #[test]
    fn toggle_case_on_a_bare_caret_takes_the_word() {
        let (mut v, mut b) = setup("int value = 1;");
        v.selections = Selections::single(6); // inside "value"
        v.toggle_case(&mut b, 0.0);
        assert_eq!(b.rope().to_string(), "int VALUE = 1;");
    }

    #[test]
    fn sort_lines_orders_the_selected_block_only() {
        let (mut v, mut b) = setup("keep\nc\na\nb\ntail\n");
        // Select the three middle lines.
        v.selections = Selections { ranges: vec![Selection { anchor: 5, head: 11, goal_col: None }], primary: 0 };
        v.sort_lines(&mut b, 0.0, false);
        assert_eq!(b.rope().to_string(), "keep\na\nb\nc\ntail\n");
    }

    #[test]
    fn sort_lines_does_nothing_without_a_selection() {
        // A stray keystroke must never reorder the whole file.
        let (mut v, mut b) = setup("c\na\nb\n");
        v.selections = Selections::single(0);
        v.sort_lines(&mut b, 0.0, false);
        assert_eq!(b.rope().to_string(), "c\na\nb\n");
    }

    #[test]
    fn carets_to_line_ends_lands_before_each_newline() {
        let (mut v, mut b) = setup("aa\nbbbb\ncc\n");
        v.selections = Selections { ranges: vec![Selection { anchor: 0, head: 10, goal_col: None }], primary: 0 };
        v.carets_to_line_ends(&mut b);
        assert_eq!(carets(&v), vec![2, 7, 10]);
    }

    #[test]
    fn backspace_in_leading_whitespace_removes_a_whole_indent() {
        let (mut v, mut b) = setup("        x");
        v.selections = Selections::single(8); // right before `x`
        v.delete_side(&mut b, false, false, 0.0);
        assert_eq!(b.rope().to_string(), "    x", "one press undoes one Tab");
    }

    #[test]
    fn backspace_after_code_is_still_one_character() {
        let (mut v, mut b) = setup("    ab");
        v.selections = Selections::single(6);
        v.delete_side(&mut b, false, false, 0.0);
        assert_eq!(b.rope().to_string(), "    a");
    }

    #[test]
    fn backspace_off_an_indent_boundary_is_one_character() {
        // Hand-aligned text (3 spaces) must not jump a full unit.
        let (mut v, mut b) = setup("   x");
        v.selections = Selections::single(3);
        v.delete_side(&mut b, false, false, 0.0);
        assert_eq!(b.rope().to_string(), "  x");
    }

    #[test]
    fn duplicate_line_ctrl_d() {
        let (mut v, mut b) = setup("int x;\nreturn;\n");
        v.selections = Selections::single(2); // on line 0
        v.duplicate_lines(&mut b, 0.0);
        assert_eq!(b.rope().to_string(), "int x;\nint x;\nreturn;\n");
    }

    /// Two carets on the same line duplicate it ONCE, not once per caret.
    #[test]
    fn duplicate_same_line_two_carets_once() {
        let (mut v, mut b) = setup("int x;\nreturn;\n");
        v.selections = Selections {
            ranges: vec![Selection::at(0), Selection::at(4)], // both on line 0
            primary: 0,
        };
        v.duplicate_lines(&mut b, 0.0);
        assert_eq!(b.rope().to_string(), "int x;\nint x;\nreturn;\n", "line 0 duplicated once");
    }

    #[test]
    fn duplicate_last_line_without_trailing_newline() {
        let (mut v, mut b) = setup("a\nb");
        v.selections = Selections::single(2); // on line 1 ("b", no newline)
        v.duplicate_lines(&mut b, 0.0);
        assert_eq!(b.rope().to_string(), "a\nb\nb");
    }

    #[test]
    fn undo_redo_reparses_incrementally() {
        let (mut v, mut b) = setup("int main;");
        v.selections = Selections::single(9);
        v.insert(&mut b, " // tail", EditKind::InsertText, 0.0);
        assert_eq!(b.rope().to_string(), "int main; // tail");
        v.undo(&mut b);
        assert_eq!(b.rope().to_string(), "int main;");
        assert!(v.selections.primary().head <= b.len_bytes()); // caret valid, no panic
        v.redo(&mut b);
        assert_eq!(b.rope().to_string(), "int main; // tail");
        // The incrementally-reparsed tree still highlights: 'int' is a keyword/type on line 0.
        let syn = v.syntax.as_ref().unwrap();
        let mut hl = Highlighter::new(Lang::C).unwrap();
        let spans = hl.line_spans(syn, b.rope(), 0..1, b.generation);
        assert!(!spans[0].is_empty());
    }

    // --- undo coalescing + caret restore at the view level ------------------------------------

    #[test]
    fn typed_run_undoes_as_one_group() {
        let (mut v, mut b) = setup("");
        v.selections = Selections::single(0);
        type_str(&mut v, &mut b, "hello", 0.0);
        assert_eq!(b.rope().to_string(), "hello");
        v.undo(&mut b);
        assert_eq!(b.rope().to_string(), "", "one undo removes the whole typed run");
    }

    #[test]
    fn caret_move_breaks_coalescing() {
        let (mut v, mut b) = setup("");
        v.selections = Selections::single(0);
        type_str(&mut v, &mut b, "ab", 0.0); // caret now at 2
        v.motion(&mut b, Motion::Left, false); // caret → 1, seals the group
        type_str(&mut v, &mut b, "cd", 1.0); // inserts at 1 → "acdb"
        assert_eq!(b.rope().to_string(), "acdb");
        v.undo(&mut b); // removes the "cd" group only
        assert_eq!(b.rope().to_string(), "ab");
        v.undo(&mut b); // removes the "ab" group
        assert_eq!(b.rope().to_string(), "");
    }

    #[test]
    fn newline_breaks_coalescing() {
        let (mut v, mut b) = setup("");
        v.selections = Selections::single(0);
        type_str(&mut v, &mut b, "ab", 0.0);
        v.insert_newline(&mut b, 0.2);
        type_str(&mut v, &mut b, "cd", 0.3);
        assert_eq!(b.rope().to_string(), "ab\ncd");
        v.undo(&mut b);
        assert_eq!(b.rope().to_string(), "ab\n"); // "cd" gone
        v.undo(&mut b);
        assert_eq!(b.rope().to_string(), "ab"); // newline gone
        v.undo(&mut b);
        assert_eq!(b.rope().to_string(), ""); // "ab" gone
    }

    #[test]
    fn ghost_type_through_advances() {
        let (mut v, mut b) = setup("fn main() { }");
        v.selections = Selections::single(12);
        let g = b.generation;
        v.set_ghost(12, g, "let x = 1;".to_string());
        v.insert(&mut b, "let", EditKind::InsertText, 0.0);
        let gh = v.ghost.as_ref().expect("ghost survives matching keystrokes");
        assert_eq!(gh.text, " x = 1;");
        assert_eq!(gh.byte, 15);
        assert_eq!(gh.generation, b.generation);
        // mismatching keystroke kills it
        v.insert(&mut b, "z", EditKind::InsertText, 0.0);
        // (invalidation happens in ui() normally; insert() only advances matches)
        assert!(v.ghost.as_ref().map(|g2| g2.byte != v.selections.primary().head).unwrap_or(true));
    }

    /// insert_snippet expands, selects the first placeholder, and Tab-steps through the
    /// remapped stops while the user types into earlier ones.
    #[test]
    fn snippet_session_expand_type_and_step() {
        let (mut v, mut b) = setup("");
        v.insert_snippet(&mut b, 0..0, "for ${1:item} in ${2:iter} {\n    $0\n}", 0.0);
        assert_eq!(b.rope().to_string(), "for item in iter {\n    \n}");
        // First stop selected: "item" (bytes 4..8).
        let s = v.selections.primary();
        assert_eq!((s.anchor.min(s.head), s.anchor.max(s.head)), (4, 8));
        assert!(v.snippet.is_some());
        // Type over the placeholder ("i" replaces the 4-byte selection → net -3).
        v.insert(&mut b, "i", EditKind::InsertText, 0.0);
        assert_eq!(b.rope().to_string(), "for i in iter {\n    \n}");
        // Tab → stop 2 must have shifted left by 3: "iter" now at 9..13.
        v.snippet_step(&mut b, 1);
        let s = v.selections.primary();
        assert_eq!(&b.rope().to_string()[s.anchor.min(s.head)..s.anchor.max(s.head)], "iter");
        // Tab → final caret ($0); the session ends on arrival.
        v.snippet_step(&mut b, 1);
        assert!(v.snippet.is_none());
        // A snippet with only the end caret opens no session.
        let (mut v, mut b) = setup("");
        v.insert_snippet(&mut b, 0..0, "plain()$0", 0.0);
        assert!(v.snippet.is_none());
        assert_eq!(v.caret_byte(), 7);
    }

    #[test]
    fn undo_restores_caret_position() {
        let (mut v, mut b) = setup("int main;");
        v.selections = Selections::single(9); // caret at end
        v.insert(&mut b, " // x", EditKind::InsertText, 0.0);
        assert_eq!(v.selections.primary().head, 14);
        v.undo(&mut b);
        assert_eq!(v.selections.primary().head, 9, "caret restored to before the edit");
        assert_eq!(v.selections.ranges.len(), 1);
        v.redo(&mut b);
        assert_eq!(v.selections.primary().head, 14, "caret restored to after the edit");
    }

    #[test]
    fn undo_restores_multi_caret_set() {
        let (mut v, mut b) = setup("x.x.x");
        let before = vec![Selection::at(1), Selection::at(3), Selection::at(5)];
        v.selections = Selections { ranges: before, primary: 0 };
        v.insert(&mut b, "Y", EditKind::InsertText, 0.0);
        v.undo(&mut b);
        assert_eq!(b.rope().to_string(), "x.x.x");
        assert_eq!(carets(&v), vec![1, 3, 5], "all three carets restored");
    }

    #[test]
    fn find_refresh_matches_case_smart() {
        let (mut v, b) = setup("Foo foo FOO\n");
        v.find.query = "foo".into();
        v.find.refresh(b.rope(), b.generation);
        assert_eq!(v.find.matches.len(), 3, "insensitive by default");
        v.find.case_sensitive = true;
        v.find.refresh(b.rope(), b.generation);
        assert_eq!(v.find.matches.len(), 1);
        assert_eq!(v.find.matches[0], 4..7);
    }

    #[test]
    fn goto_match_advances_and_wraps() {
        let (mut v, mut b) = setup("x foo y foo z foo\n");
        v.find.query = "foo".into();
        v.selections = Selections::single(0);
        v.goto_match(&mut b, true, false); // first match
        assert_eq!(v.selections.primary().range(), 2..5);
        v.goto_match(&mut b, true, true); // next
        assert_eq!(v.selections.primary().range(), 8..11);
        v.goto_match(&mut b, true, true);
        assert_eq!(v.selections.primary().range(), 14..17);
        v.goto_match(&mut b, true, true); // wraps to the first
        assert_eq!(v.selections.primary().range(), 2..5);
        v.goto_match(&mut b, false, true); // prev wraps to the last
        assert_eq!(v.selections.primary().range(), 14..17);
    }

    #[test]
    fn replace_current_replaces_and_moves_on() {
        let (mut v, mut b) = setup("aa bb aa\n");
        v.find.query = "aa".into();
        v.find.replacement = "XYZ".into();
        v.goto_match(&mut b, true, false); // select first "aa"
        v.replace_current(&mut b, 0.0);
        assert_eq!(b.rope().to_string(), "XYZ bb aa\n");
        // The next match is auto-selected.
        assert_eq!(v.selections.primary().range(), 7..9);
        // Undo is a single step restoring the original + the pre-replace selection.
        v.undo(&mut b);
        assert_eq!(b.rope().to_string(), "aa bb aa\n");
    }

    #[test]
    fn replace_all_is_one_undo_step() {
        let (mut v, mut b) = setup("aa bb aa cc aa\n");
        v.find.query = "aa".into();
        v.find.replacement = "Z".into();
        v.find.refresh(b.rope(), b.generation);
        v.replace_all(&mut b, 0.0);
        assert_eq!(b.rope().to_string(), "Z bb Z cc Z\n");
        // Caret after the last replacement.
        assert_eq!(v.selections.primary().head, 11);
        v.undo(&mut b);
        assert_eq!(b.rope().to_string(), "aa bb aa cc aa\n");
    }

    #[test]
    fn two_caret_backspace_both_deleting_reparses_without_panic() {
        // Both carets delete → a 2-change shrinking transaction through the INCREMENTAL reparse
        // path (regression for the syntax.edited coordinate bug).
        let (mut v, mut b) = setup("int aa;\nint bb;\n");
        v.selections = Selections { ranges: vec![Selection::at(6), Selection::at(14)], primary: 1 };
        v.delete_side(&mut b, false, false, 0.0); // deletes one char before each caret
        assert_eq!(b.rope().to_string(), "int a;\nint b;\n");
        assert_eq!(carets(&v), vec![5, 12]);
    }

    #[test]
    fn block_indent_indents_spanned_lines_and_keeps_selection() {
        let (mut v, mut b) = setup("aa\nbb\ncc\n");
        // Select from line 0 into line 1 (partial).
        v.selections = Selections { ranges: vec![Selection { anchor: 0, head: 4, goal_col: None }], primary: 0 };
        v.indent_lines(&mut b, false, 0.0);
        assert_eq!(b.rope().to_string(), "    aa\n    bb\ncc\n");
        // Selection preserved: anchor at col 0 keeps the indent inside the selection; the head
        // (was byte 4 = 'b'+1) shifted by two inserts before it... byte 4 is on line 1 after one
        // insert at 0 and one at 3: 4 + 4 + 4 = 12.
        let p = v.selections.primary();
        assert_eq!(p.range(), 0..12);
        // One undo restores everything (single transaction) including the selection.
        v.undo(&mut b);
        assert_eq!(b.rope().to_string(), "aa\nbb\ncc\n");
        assert_eq!(v.selections.primary().range(), 0..4);
    }

    #[test]
    fn block_indent_skips_line_when_selection_ends_at_its_col0() {
        let (mut v, mut b) = setup("aa\nbb\ncc\n");
        // Selection covers line 0 + the newline — ends exactly at line 1 col 0.
        v.selections = Selections { ranges: vec![Selection { anchor: 0, head: 3, goal_col: None }], primary: 0 };
        v.indent_lines(&mut b, false, 0.0);
        assert_eq!(b.rope().to_string(), "    aa\nbb\ncc\n", "line 1 must not indent");
    }

    #[test]
    fn block_indent_skips_empty_lines() {
        let (mut v, mut b) = setup("aa\n\nbb\n");
        v.selections = Selections { ranges: vec![Selection { anchor: 0, head: 7, goal_col: None }], primary: 0 };
        v.indent_lines(&mut b, false, 0.0);
        assert_eq!(b.rope().to_string(), "    aa\n\n    bb\n", "empty line gets no trailing ws");
    }

    #[test]
    fn block_unindent_strips_partial_and_full_levels() {
        let (mut v, mut b) = setup("    aa\n  bb\ncc\n\tdd\n");
        v.selections = Selections { ranges: vec![Selection { anchor: 0, head: b.len_bytes(), goal_col: None }], primary: 0 };
        v.indent_lines(&mut b, true, 0.0);
        // 4 spaces stripped, 2 spaces stripped, none to strip, one tab stripped.
        assert_eq!(b.rope().to_string(), "aa\nbb\ncc\ndd\n");
        v.undo(&mut b);
        assert_eq!(b.rope().to_string(), "    aa\n  bb\ncc\n\tdd\n");
    }

    #[test]
    fn shift_tab_unindents_bare_caret_line() {
        let (mut v, mut b) = setup("    aa\n");
        v.selections = Selections::single(5); // caret inside 'aa'
        v.indent_lines(&mut b, true, 0.0);
        assert_eq!(b.rope().to_string(), "aa\n");
        assert_eq!(v.selections.primary().head, 1, "caret shifted left with the stripped indent");
    }

    #[test]
    fn multi_caret_edit_with_noop_caret_still_refuses_to_coalesce() {
        // Two carets, one parked at byte 0 (its backspace is a no-op and is filtered out). The
        // surviving single-change edits must STILL count as multi-caret — no coalescing.
        let (mut v, mut b) = setup("abcd");
        v.selections = Selections { ranges: vec![Selection::at(0), Selection::at(4)], primary: 1 };
        v.delete_side(&mut b, false, false, 0.0); // deletes 'd' (caret@0 no-op)
        assert_eq!(b.rope().to_string(), "abc");
        v.delete_side(&mut b, false, false, 0.05); // deletes 'c'
        assert_eq!(b.rope().to_string(), "ab");
        v.undo(&mut b);
        assert_eq!(b.rope().to_string(), "abc", "each multi-caret keystroke is its own undo step");
        assert_eq!(carets(&v).len(), 2, "undo restores the 2-caret state");
        v.undo(&mut b);
        assert_eq!(b.rope().to_string(), "abcd");
    }

    #[test]
    fn selection_delete_is_its_own_group() {
        // Backspace on a selection must NOT join the following backspace run (JetBrains parity).
        let (mut v, mut b) = setup("abcd");
        v.selections = Selections { ranges: vec![Selection { anchor: 1, head: 3, goal_col: None }], primary: 0 };
        v.delete_side(&mut b, false, false, 0.0); // deletes the "bc" selection
        assert_eq!(b.rope().to_string(), "ad");
        v.delete_side(&mut b, false, false, 0.05); // backspace deletes 'a' — separate group
        assert_eq!(b.rope().to_string(), "d");
        v.undo(&mut b);
        assert_eq!(b.rope().to_string(), "ad", "first undo restores only the backspace");
        v.undo(&mut b);
        assert_eq!(b.rope().to_string(), "abcd", "second undo restores the selection-delete");
    }

    #[test]
    fn typing_over_selection_is_its_own_group() {
        let (mut v, mut b) = setup("abcd");
        // select "bc", type 'X' (a replace → non-empty forward change breaks coalescing)
        v.selections = Selections { ranges: vec![Selection { anchor: 1, head: 3, goal_col: None }], primary: 0 };
        v.insert(&mut b, "X", EditKind::InsertText, 0.0);
        assert_eq!(b.rope().to_string(), "aXd");
        type_str(&mut v, &mut b, "yz", 0.05); // subsequent typing is a separate run
        assert_eq!(b.rope().to_string(), "aXyzd");
        v.undo(&mut b); // removes "yz"
        assert_eq!(b.rope().to_string(), "aXd");
        v.undo(&mut b); // undoes the replace → restores "bc"
        assert_eq!(b.rope().to_string(), "abcd");
    }

    // --- rainbow brackets at the paint layer ---------------------------------------------------

    #[test]
    fn bracket_split_keeps_spans_sorted_non_overlapping_and_gapless() {
        use crate::highlight::{index_brackets, BRACKET_PALETTE};
        // A bracket-heavy line through the REAL tree-sitter spans + the REAL bracket index
        // (same invariant style as highlight's spans_are_line_relative_sorted_non_overlapping).
        let src = "int f(int a[3]) { return (a[0] + (a[1] * (a[2]))); }\n";
        let rope = Rope::from_str(src);
        let syn = Syntax::new(Lang::C, &rope).unwrap();
        let mut hl = Highlighter::new(Lang::C).unwrap();
        let spans = hl.line_spans(&syn, &rope, 0..1, 0);
        let brackets = index_brackets(&rope);
        let text = src.trim_end_matches('\n');
        let paint = line_paint_spans(text.len(), &spans[0], &brackets, 0);

        // Line-relative, sorted, non-overlapping — and (stronger) gapless full coverage.
        let mut prev_end = 0usize;
        for (r, _) in &paint {
            assert!(r.start < r.end, "empty span {r:?}");
            assert_eq!(r.start, prev_end, "gap/overlap at {r:?}");
            prev_end = r.end;
        }
        assert_eq!(prev_end, text.len());

        // Every bracket byte paints EXACTLY its depth color, overriding Punctuation.
        for &(off, depth) in &brackets {
            let (_, c) = paint.iter().find(|(r, _)| r.contains(&off)).unwrap();
            assert_eq!(*c, bracket_color(depth), "bracket at byte {off}");
        }
        // Spot-check the deepest pair: `[` of `[2]` sits at depth 4 → the 5th palette color.
        let at = src.find("[2]").unwrap();
        assert!(brackets.contains(&(at, Some(4))));
        let (_, c) = paint.iter().find(|(r, _)| r.contains(&at)).unwrap();
        assert_eq!(*c, BRACKET_PALETTE[4]);
    }

    #[test]
    fn plain_lines_and_nonzero_line_starts_still_get_bracket_colors() {
        use crate::highlight::BRACKET_PALETTE;
        // No highlighter (empty spans) + absolute bracket offsets rebased by line_start.
        let paint = line_paint_spans(5, &[], &[(11, Some(1)), (13, None)], 10);
        assert_eq!(
            paint,
            vec![
                (0..1, TEXT()),
                (1..2, BRACKET_PALETTE[1]),
                (2..3, TEXT()),
                (3..4, bracket_color(None)), // unmatched closer → error red
                (4..5, TEXT()),
            ]
        );
    }

    // --- diagnostics severity ------------------------------------------------------------------

    #[test]
    fn nasa_severity_is_orange_and_error_weight() {
        let d = |severity| ViewDiag { range: 0..1, severity, message: String::new() };
        assert_eq!(d(5).color(), Color32::from_rgb(233, 110, 44), "5 = NASA/PoT reserved orange");
        assert_ne!(d(1).color(), d(5).color(), "error red stays distinct from NASA orange");
        assert_eq!(d(5).rank(), d(1).rank(), "NASA findings order at error weight");
        assert!(d(5).rank() < d(2).rank(), "…so a PoT finding outranks a warning");
    }

    // --- line-structural commands (comment / move-line / join) --------------------------------

    /// Select every line of `src` so a line command sees the whole buffer.
    fn select_all_lines(v: &mut EditorView, b: &Buffer) {
        v.selections = Selections {
            ranges: vec![Selection { anchor: 0, head: b.rope().len_bytes(), goal_col: None }],
            primary: 0,
        };
    }

    #[test]
    fn comment_toggle_round_trips_and_aligns_to_min_indent() {
        // A C file (setup uses t.c → line comment "//"), two lines at different indents.
        let (mut v, mut b) = setup("  int a;\n    int b;\n");
        select_all_lines(&mut v, &b);
        v.toggle_line_comment(&mut b, 0.0);
        // Tokens inserted at the LEAST indent (2 spaces), so the block stays aligned.
        assert_eq!(b.rope().to_string(), "  // int a;\n  //   int b;\n");
        // Toggling again removes exactly what was added — an involution.
        select_all_lines(&mut v, &b);
        v.toggle_line_comment(&mut b, 1.0);
        assert_eq!(b.rope().to_string(), "  int a;\n    int b;\n");
    }

    #[test]
    fn comment_toggle_comments_a_mixed_block_then_uncomments_when_all_commented() {
        // One line already commented, one not → toggling COMMENTS (not all were commented).
        let (mut v, mut b) = setup("// a\nb\n");
        select_all_lines(&mut v, &b);
        v.toggle_line_comment(&mut b, 0.0);
        assert_eq!(b.rope().to_string(), "// // a\n// b\n", "mixed block → comment");
        // Now every line is commented → toggle uncomments one level.
        select_all_lines(&mut v, &b);
        v.toggle_line_comment(&mut b, 1.0);
        assert_eq!(b.rope().to_string(), "// a\nb\n");
    }

    #[test]
    fn comment_toggle_skips_blank_lines() {
        let (mut v, mut b) = setup("a\n\nb\n");
        select_all_lines(&mut v, &b);
        v.toggle_line_comment(&mut b, 0.0);
        assert_eq!(b.rope().to_string(), "// a\n\n// b\n", "blank line untouched");
    }

    #[test]
    fn block_comment_wraps_and_unwraps_selection() {
        let buffer = Buffer::from_text("h1 { color: red }\n");
        let mut v = EditorView::new(&buffer, "t.css"); // CSS → block comment /* */
        let mut b = buffer;
        v.selections =
            Selections { ranges: vec![Selection { anchor: 0, head: 17, goal_col: None }], primary: 0 };
        v.toggle_block_comment(&mut b, 0.0);
        assert_eq!(b.rope().to_string(), "/* h1 { color: red } */\n");
        v.selections =
            Selections { ranges: vec![Selection { anchor: 0, head: 23, goal_col: None }], primary: 0 };
        v.toggle_block_comment(&mut b, 1.0);
        assert_eq!(b.rope().to_string(), "h1 { color: red }\n");
    }

    #[test]
    fn move_line_down_and_back_up_is_identity() {
        let (mut v, mut b) = setup("one\ntwo\nthree\n");
        v.selections = Selections::single(0); // on "one"
        v.move_lines(&mut b, true, 0.0);
        assert_eq!(b.rope().to_string(), "two\none\nthree\n");
        // The caret rode along with the moved line, so moving it up returns to the start state.
        v.move_lines(&mut b, false, 1.0);
        assert_eq!(b.rope().to_string(), "one\ntwo\nthree\n");
    }

    #[test]
    fn move_line_is_a_noop_at_the_edges() {
        let (mut v, mut b) = setup("a\nb\n");
        v.selections = Selections::single(0);
        v.move_lines(&mut b, false, 0.0); // already at top
        assert_eq!(b.rope().to_string(), "a\nb\n");
        v.selections = Selections::single(2); // on "b", the last real line
        v.move_lines(&mut b, true, 1.0); // nothing below
        assert_eq!(b.rope().to_string(), "a\nb\n");
    }

    #[test]
    fn move_line_handles_the_last_line_without_a_trailing_newline() {
        let (mut v, mut b) = setup("a\nb"); // no final newline
        v.selections = Selections::single(0);
        v.move_lines(&mut b, true, 0.0);
        assert_eq!(b.rope().to_string(), "b\na", "newline boundary preserved, no dup/loss");
        // …and the caret rode the moved line (byte 2, on 'a'), not the separator (regression: the
        // shift used the neighbor's original length, landing one byte short).
        assert_eq!(v.selections.primary().head, 2, "caret follows the moved 'a'");
    }

    #[test]
    fn move_line_preserves_crlf_at_the_unterminated_last_line() {
        // "A\r\nB" — CRLF after A, nothing after B. Moving B up must not lose the \r or leave a
        // stray one (regression: strip('\n')/rejoin('\n') fabricated a bare LF).
        let (mut v, mut b) = setup("A\r\nB");
        v.selections = Selections::single(3); // on B
        v.move_lines(&mut b, false, 0.0);
        assert_eq!(b.rope().to_string(), "B\r\nA", "CRLF preserved, no stray \\r");
    }

    #[test]
    fn move_block_of_lines_carries_the_whole_selection() {
        let (mut v, mut b) = setup("1\n2\n3\n4\n");
        // Select lines "2" and "3".
        v.selections =
            Selections { ranges: vec![Selection { anchor: 2, head: 6, goal_col: None }], primary: 0 };
        v.move_lines(&mut b, true, 0.0);
        assert_eq!(b.rope().to_string(), "1\n4\n2\n3\n", "the 2-line block jumped past 4");
    }

    #[test]
    fn join_lines_collapses_to_single_space() {
        let (mut v, mut b) = setup("foo\n    bar\n");
        v.selections = Selections::single(0);
        v.join_lines(&mut b, 0.0);
        assert_eq!(b.rope().to_string(), "foo bar\n", "newline + next indent → one space");
    }

    #[test]
    fn join_on_the_last_text_line_is_a_noop_and_keeps_the_trailing_newline() {
        // HIGH regression: the last text line has no line below, so Ctrl+Shift+J there must do
        // NOTHING — not delete the file's final newline (which fired on essentially every file).
        let (mut v, mut b) = setup("a\nb\n");
        v.selections = Selections::single(2); // on "b", the last text line
        v.join_lines(&mut b, 0.0);
        assert_eq!(b.rope().to_string(), "a\nb\n", "trailing newline preserved, no edit");
        // A single-line buffer with no newline: also a no-op.
        let (mut v, mut b) = setup("only");
        v.selections = Selections::single(0);
        v.join_lines(&mut b, 0.0);
        assert_eq!(b.rope().to_string(), "only");
    }

    #[test]
    fn join_lines_over_a_selection_joins_all_spanned() {
        let (mut v, mut b) = setup("a\nb\nc\nd\n");
        v.selections =
            Selections { ranges: vec![Selection { anchor: 0, head: 5, goal_col: None }], primary: 0 };
        v.join_lines(&mut b, 0.0);
        assert_eq!(b.rope().to_string(), "a b c\nd\n", "three spanned lines joined, d untouched");
    }

    #[test]
    fn structural_edits_are_undoable_in_one_step() {
        let (mut v, mut b) = setup("x\ny\n");
        select_all_lines(&mut v, &b);
        v.toggle_line_comment(&mut b, 0.0);
        assert_eq!(b.rope().to_string(), "// x\n// y\n");
        v.undo(&mut b);
        assert_eq!(b.rope().to_string(), "x\ny\n", "one undo reverts the whole multi-line comment");
    }

    // --- auto-pair / surround -----------------------------------------------------------------

    #[test]
    fn typing_an_opener_inserts_the_pair_with_caret_between() {
        let (mut v, mut b) = setup("");
        v.selections = Selections::single(0);
        assert!(v.typed_char(&mut b, '(', 0.0), "opener is handled");
        assert_eq!(b.rope().to_string(), "()");
        assert_eq!(v.selections.primary().head, 1, "caret sits between the pair");
    }

    #[test]
    fn typing_an_opener_around_a_selection_surrounds_it() {
        let (mut v, mut b) = setup("value");
        v.selections =
            Selections { ranges: vec![Selection { anchor: 0, head: 5, goal_col: None }], primary: 0 };
        assert!(v.typed_char(&mut b, '{', 0.0));
        assert_eq!(b.rope().to_string(), "{value}");
        let p = v.selections.primary();
        assert_eq!((p.range().start, p.range().end), (1, 6), "inner text stays selected");
    }

    #[test]
    fn typing_a_closer_over_an_auto_closer_skips_it() {
        let (mut v, mut b) = setup("()");
        v.selections = Selections::single(1); // between ( and )
        assert!(v.typed_char(&mut b, ')', 0.0), "skip is handled");
        assert_eq!(b.rope().to_string(), "()", "no second ) inserted");
        assert_eq!(v.selections.primary().head, 2, "caret moved past the existing )");
    }

    #[test]
    fn backspace_inside_an_empty_pair_deletes_both() {
        let (mut v, mut b) = setup("()");
        v.selections = Selections::single(1);
        assert!(v.try_delete_pair(&mut b, 0.0));
        assert_eq!(b.rope().to_string(), "");
        // A non-empty pair is left to the normal backspace (only the opener would go).
        let (mut v, mut b) = setup("(x)");
        v.selections = Selections::single(1);
        assert!(!v.try_delete_pair(&mut b, 0.0), "( x ) is not an empty pair");
    }

    #[test]
    fn quotes_do_not_auto_close_against_word_characters() {
        // An apostrophe after a letter (don't, a Rust lifetime 'a, a char literal) must insert
        // literally, not become ''.
        let (mut v, mut b) = setup("don");
        v.selections = Selections::single(3);
        assert!(!v.typed_char(&mut b, '\'', 0.0), "not auto-closed after a word char");
        assert_eq!(b.rope().to_string(), "don", "typed_char left it for the literal insert");

        // In empty/neutral context a quote DOES pair.
        let (mut v, mut b) = setup("");
        v.selections = Selections::single(0);
        assert!(v.typed_char(&mut b, '"', 0.0));
        assert_eq!(b.rope().to_string(), "\"\"");
    }

    #[test]
    fn auto_pair_across_multiple_carets() {
        let (mut v, mut b) = setup("a\nb\n");
        v.selections = Selections {
            ranges: vec![Selection::at(1), Selection::at(3)],
            primary: 1,
        };
        assert!(v.typed_char(&mut b, '(', 0.0));
        assert_eq!(b.rope().to_string(), "a()\nb()\n", "both carets got a pair");
    }

    #[test]
    fn auto_pair_is_one_undo_step() {
        let (mut v, mut b) = setup("");
        v.selections = Selections::single(0);
        v.typed_char(&mut b, '[', 0.0);
        assert_eq!(b.rope().to_string(), "[]");
        v.undo(&mut b);
        assert_eq!(b.rope().to_string(), "", "the whole pair undoes at once");
    }

    // --- matching bracket -----------------------------------------------------------------------

    #[test]
    fn match_pair_lights_the_bracket_at_or_before_the_caret() {
        let (mut v, b) = setup("foo(bar)\n");
        v.brackets.refresh(b.rope(), b.generation);
        // Caret ON the opener.
        v.selections = Selections::single(3);
        assert_eq!(v.compute_match_pair(b.rope()), Some((3, 7)));
        // Caret just PAST the closer still lights the pair.
        v.selections = Selections::single(8);
        assert_eq!(v.compute_match_pair(b.rope()), Some((7, 3)));
        // Caret in open text lights nothing.
        v.selections = Selections::single(1);
        assert_eq!(v.compute_match_pair(b.rope()), None);
        // A selection (not a bare caret) suppresses the highlight.
        v.selections =
            Selections { ranges: vec![Selection { anchor: 3, head: 8, goal_col: None }], primary: 0 };
        assert_eq!(v.compute_match_pair(b.rope()), None);
    }

    #[test]
    fn jump_to_matching_bracket_bounces_between_the_pair() {
        let (mut v, mut b) = setup("foo(bar)\n");
        v.brackets.refresh(b.rope(), b.generation);
        v.selections = Selections::single(3); // on '('
        v.jump_to_matching_bracket(&mut b);
        assert_eq!(v.selections.primary().head, 7, "jumped onto the matching ')'");
        v.brackets.refresh(b.rope(), b.generation);
        v.jump_to_matching_bracket(&mut b);
        assert_eq!(v.selections.primary().head, 3, "and back onto the '('");
    }

    #[test]
    fn jump_is_involutive_even_for_an_empty_pair() {
        // "()" — landing just PAST the match would absorb the caret between the two; landing ON it
        // keeps the jump reversible.
        let (mut v, mut b) = setup("()");
        v.brackets.refresh(b.rope(), b.generation);
        v.selections = Selections::single(0); // on '('
        v.jump_to_matching_bracket(&mut b);
        assert_eq!(v.selections.primary().head, 1, "onto ')'");
        v.brackets.refresh(b.rope(), b.generation);
        v.jump_to_matching_bracket(&mut b);
        assert_eq!(v.selections.primary().head, 0, "back onto '(' — reversible");
    }

    /// Enter is the popup's ONLY once the user navigated it (`suppress_enter`); an un-navigated
    /// auto-popup must let Enter make a newline. Arrows/Tab/Esc are always the popup's while it is
    /// up (`suppress_nav_keys`). This is the "press Enter to make a new line does nothing" fix.
    #[test]
    fn multi_caret_word_delete_in_one_word_does_not_panic() {
        // Two bare carets inside "world" (bytes 8 and 10). Ctrl+Backspace expands each to a
        // prev-word deletion (6..8 and 6..10) — overlapping ranges that used to build a malformed
        // Transaction and panic in apply_inner. They must coalesce to a single 6..10 delete.
        let (mut v, mut b) = setup("hello world");
        v.selections = Selections { ranges: vec![Selection::at(8), Selection::at(10)], primary: 1 };
        v.delete_side(&mut b, false, true, 0.0);
        assert_eq!(b.rope().to_string(), "hello d");
        assert_eq!(carets(&v), vec![6], "the two carets collapse onto the word start");

        // Forward (Ctrl+Delete) variant — next-word ranges also overlap.
        let (mut v, mut b) = setup("hello world");
        v.selections = Selections { ranges: vec![Selection::at(6), Selection::at(8)], primary: 1 };
        v.delete_side(&mut b, true, true, 0.0);
        assert_eq!(b.rope().to_string(), "hello ");
    }

    #[test]
    fn enter_always_makes_a_newline_regardless_of_completion_flags() {
        let none = egui::Modifiers::default();
        // Enter is NEVER stolen by the completion popup any more — it always makes a newline.
        // Completions accept on Tab/click. This kills the cross-frame suppress_enter race that ate
        // newlines and dropped the caret on a random line.
        for (nav, enter_flag) in [(false, false), (true, false), (true, true)] {
            let (mut v, mut b) = setup("ab");
            v.selections = Selections::single(2);
            v.suppress_nav_keys = nav;
            v.suppress_enter = enter_flag;
            v.handle_key(&mut b, egui::Key::Enter, none, 0.0);
            assert_eq!(
                b.rope().to_string(),
                "ab\n",
                "Enter must be a newline (suppress_nav={nav}, suppress_enter={enter_flag})",
            );
        }
    }

    /// A click focuses the editor and PAINTS the caret at the click — the "clicking does nothing /
    /// the caret never appears" report. The caret paints only while focused, and focus lands the
    /// frame after the click, so this drives two frames and asserts caret_pos is then set.
    #[test]
    fn clicking_focuses_and_paints_the_caret() {
        let ctx = egui::Context::default();
        let mut buffer = Buffer::from_text("alpha\nbravo\ncharlie\ndelta\n");
        let mut view = EditorView::new(&buffer, "t.txt");
        let frame = |ctx: &egui::Context, v: &mut EditorView, b: &mut Buffer, ev: Vec<egui::Event>| {
            let raw = egui::RawInput { events: ev, ..Default::default() };
            let _ = ctx.run(raw, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    v.ui(ui, b);
                });
            });
        };
        let click = |pos: egui::Pos2, pressed: bool| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        };
        // Lay out once, then click into the text.
        frame(&ctx, &mut view, &mut buffer, vec![]);
        let at = egui::pos2(60.0, 40.0);
        frame(&ctx, &mut view, &mut buffer, vec![click(at, true), click(at, false)]);
        // Focus applies next frame — render it (no events) and the caret must now be painted.
        frame(&ctx, &mut view, &mut buffer, vec![]);
        assert!(view.caret_pos.is_some(), "the caret must paint after a click focuses the editor");
    }

    /// Widgets ABOVE the editor that come and go (breadcrumbs emptying when the caret leaves a
    /// scope, the find bar) must not cost the editor its keyboard focus. egui auto-ids bake in an
    /// allocation counter, so before the stable `widget_id` a vanishing crumb row changed the
    /// editor's id and egui dropped focus — "click on a lone } and the caret disappears".
    #[test]
    fn focus_survives_widgets_above_appearing_and_vanishing() {
        let ctx = egui::Context::default();
        let mut buffer = Buffer::from_text("alpha\nbravo\ncharlie\ndelta\n");
        let mut view = EditorView::new(&buffer, "t.txt");
        let frame = |ctx: &egui::Context,
                     v: &mut EditorView,
                     b: &mut Buffer,
                     ev: Vec<egui::Event>,
                     crumb_row: bool| {
            let raw = egui::RawInput { events: ev, ..Default::default() };
            let _ = ctx.run(raw, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    if crumb_row {
                        // Stand-in for the breadcrumb row: a clickable label allocated first.
                        ui.horizontal(|ui| {
                            let _ = ui.add(egui::Label::new("fn foo").sense(egui::Sense::click()));
                        });
                    }
                    v.ui(ui, b);
                });
            });
        };
        let click = |pos: egui::Pos2, pressed: bool| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        };
        // Focus the editor while the crumb row is visible.
        frame(&ctx, &mut view, &mut buffer, vec![], true);
        let at = egui::pos2(60.0, 70.0);
        frame(&ctx, &mut view, &mut buffer, vec![click(at, true), click(at, false)], true);
        frame(&ctx, &mut view, &mut buffer, vec![], true);
        assert!(view.caret_pos.is_some(), "sanity: caret paints while focused");
        // The crumb row vanishes (caret left the scope) — focus, and the caret, must survive.
        view.caret_pos = None;
        frame(&ctx, &mut view, &mut buffer, vec![], false);
        frame(&ctx, &mut view, &mut buffer, vec![], false);
        assert!(
            view.has_focus(),
            "editor focus must survive a widget above it vanishing (auto-id shift)"
        );
        assert!(view.caret_pos.is_some(), "the caret must stay painted");
        // And the row coming BACK must not drop it either.
        view.caret_pos = None;
        frame(&ctx, &mut view, &mut buffer, vec![], true);
        frame(&ctx, &mut view, &mut buffer, vec![], true);
        assert!(view.has_focus() && view.caret_pos.is_some(), "focus must survive the row returning");
    }

    /// Real-galley repro: drive `EditorView::ui` through a HEADLESS egui context (real font
    /// layout, real caret_x / pos_from_ccursor / paint_match_pair) while typing into and moving
    /// around a C file. This covers the paint path the pure-logic sweep cannot. A panic here — an
    /// out-of-range galley cursor, a stale match-pair offset — reproduces the "typing crashes" bug.
    #[test]
    fn headless_enter_moves_caret_to_new_line() {
        let ctx = egui::Context::default();
        let src = "abc\ndef\nghi\n";
        let mut buffer = Buffer::from_text(src);
        let mut view = EditorView::new(&buffer, "main.rs");

        let frame = |ctx: &egui::Context, v: &mut EditorView, b: &mut Buffer, events: Vec<egui::Event>| {
            let raw = egui::RawInput { events, ..Default::default() };
            let _ = ctx.run(raw, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    v.ui(ui, b);
                });
            });
        };
        let click = |pos: egui::Pos2, pressed: bool| egui::Event::PointerButton {
            pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::default(),
        };
        let none = egui::Modifiers::default();
        let key = |k: egui::Key| egui::Event::Key {
            key: k, physical_key: None, pressed: true, repeat: false, modifiers: none,
        };
        // Settle layout, then focus exactly like the passing headless test (click at 80,80).
        frame(&ctx, &mut view, &mut buffer, vec![]);
        let at = egui::pos2(80.0, 80.0);
        frame(&ctx, &mut view, &mut buffer, vec![click(at, true), click(at, false)]);
        frame(&ctx, &mut view, &mut buffer, vec![]);
        // Sanity: input reaches the editor (type a char).
        frame(&ctx, &mut view, &mut buffer, vec![egui::Event::Text("Z".into())]);
        assert!(buffer.rope().to_string().contains('Z'), "focus/input not wired in harness");
        // Send Enter through the FULL egui event path.
        let rope_before = buffer.rope().to_string();
        frame(&ctx, &mut view, &mut buffer, vec![key(egui::Key::Enter)]);
        let inserted_newline = buffer.rope().to_string() != rope_before;
        // Prove focus is still alive on a following frame by typing a char.
        frame(&ctx, &mut view, &mut buffer, vec![egui::Event::Text("Y".into())]);
        let focus_alive = buffer.rope().to_string().contains('Y');
        assert!(focus_alive, "focus should persist across frames");
        assert!(inserted_newline, "Enter via the egui event path must insert a newline");

        // Enter must STILL make a newline even when the app has flagged suppress_enter/nav (the
        // old completion-popup race that ate newlines). Enter is never stolen now.
        view.suppress_enter = true;
        view.suppress_nav_keys = true;
        let before = buffer.rope().to_string();
        frame(&ctx, &mut view, &mut buffer, vec![key(egui::Key::Enter)]);
        assert!(
            buffer.rope().to_string() != before,
            "Enter must insert a newline even with suppress_enter set",
        );
    }

    /// egui fires a FAKE primary click on any focused click-sensing widget when Space or Enter is
    /// pressed (context.rs "Space/enter works like a primary click"). The editor is exactly such a
    /// widget, so pressing Space used to re-run the click handler and teleport the caret to
    /// wherever the MOUSE was parked — "hit space and it went to a super random line in the middle
    /// of a word". Enter likewise yanked the caret off its new line back to the hover position.
    /// The pointer branches must only react to REAL pointer clicks.
    #[test]
    fn headless_space_and_enter_do_not_teleport_caret_to_mouse() {
        let ctx = egui::Context::default();
        let src = "alpha\nbravo\ncharlie\ndelta\necho\nfoxtrot\n";
        let mut buffer = Buffer::from_text(src);
        let mut view = EditorView::new(&buffer, "t.txt");

        let frame = |ctx: &egui::Context, v: &mut EditorView, b: &mut Buffer, events: Vec<egui::Event>| {
            let raw = egui::RawInput { events, ..Default::default() };
            let _ = ctx.run(raw, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    v.ui(ui, b);
                });
            });
        };
        let click = |pos: egui::Pos2, pressed: bool| egui::Event::PointerButton {
            pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::default(),
        };
        let none = egui::Modifiers::default();
        let key = |k: egui::Key| egui::Event::Key {
            key: k, physical_key: None, pressed: true, repeat: false, modifiers: none,
        };

        // Focus + place the caret on the FIRST line.
        frame(&ctx, &mut view, &mut buffer, vec![]);
        let first_line = egui::pos2(80.0, 26.0);
        frame(&ctx, &mut view, &mut buffer, vec![click(first_line, true), click(first_line, false)]);
        frame(&ctx, &mut view, &mut buffer, vec![]);
        let caret0 = view.caret_byte();
        let line0 = buffer.rope().byte_to_line(caret0);

        // Park the mouse over a DIFFERENT line, far below — no buttons pressed.
        let parked = egui::pos2(90.0, 120.0);
        frame(&ctx, &mut view, &mut buffer, vec![egui::Event::PointerMoved(parked)]);

        // Space: types a space at the caret. The fake click must NOT move the caret to `parked`.
        frame(&ctx, &mut view, &mut buffer, vec![key(egui::Key::Space), egui::Event::Text(" ".into())]);
        let after_space = view.caret_byte();
        assert_eq!(
            buffer.rope().byte_to_line(after_space),
            line0,
            "Space must not teleport the caret to the mouse position",
        );
        assert_eq!(after_space, caret0 + 1, "the space itself was typed at the caret");

        // Enter: the caret must land on the newly created next line, not at the mouse.
        frame(&ctx, &mut view, &mut buffer, vec![key(egui::Key::Enter)]);
        let after_enter = view.caret_byte();
        assert_eq!(
            buffer.rope().byte_to_line(after_enter),
            line0 + 1,
            "Enter must land the caret on the new line, not at the mouse hover position",
        );
    }

    #[test]
    fn headless_click_does_not_scroll_the_view() {
        let ctx = egui::Context::default();
        // Match the app: crisp, non-animated scrolling.
        ctx.all_styles_mut(|s| s.scroll_animation = egui::style::ScrollAnimation::none());
        let src: String = (0..200).map(|i| format!("line number {i}\n")).collect();
        let mut buffer = Buffer::from_text(&src);
        let mut view = EditorView::new(&buffer, "main.rs");

        let frame = |ctx: &egui::Context, v: &mut EditorView, b: &mut Buffer, events: Vec<egui::Event>| {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 400.0))),
                events,
                ..Default::default()
            };
            let _ = ctx.run(raw, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    v.ui(ui, b);
                });
            });
        };
        let wheel = |dy: f32| egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, dy),
            modifiers: egui::Modifiers::default(),
        };
        let click = |pos: egui::Pos2, pressed: bool| egui::Event::PointerButton {
            pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::default(),
        };
        // Put the pointer in the editor, then scroll well down.
        let at = egui::pos2(200.0, 200.0);
        frame(&ctx, &mut view, &mut buffer, vec![egui::Event::PointerMoved(at)]);
        for _ in 0..8 {
            frame(&ctx, &mut view, &mut buffer, vec![egui::Event::PointerMoved(at), wheel(-300.0)]);
        }
        // Let residual scroll (egui input smoothing) fully settle before measuring.
        for _ in 0..40 {
            frame(&ctx, &mut view, &mut buffer, vec![]);
        }
        let before = view.last_scroll_y;
        assert!(before > 100.0, "precondition: the view scrolled down (got {before})");
        // Click on a visible line — this must NOT move the scroll.
        frame(&ctx, &mut view, &mut buffer, vec![click(at, true), click(at, false)]);
        frame(&ctx, &mut view, &mut buffer, vec![]);
        let after = view.last_scroll_y;
        assert!(
            (after - before).abs() < 1.0,
            "clicking scrolled the view: {before} -> {after}",
        );
        // Typing on the now-visible caret line must not scroll either.
        for ch in "hello".chars() {
            frame(&ctx, &mut view, &mut buffer, vec![egui::Event::Text(ch.to_string())]);
        }
        frame(&ctx, &mut view, &mut buffer, vec![]);
        let after_typing = view.last_scroll_y;
        assert!(
            (after_typing - before).abs() < 1.0,
            "typing on a visible line scrolled the view: {before} -> {after_typing}",
        );
    }

    #[test]
    fn headless_soft_wrap_paint_and_click_does_not_panic() {
        let ctx = egui::Context::default();
        // A very long single line that will wrap onto many visual rows.
        let long = "x".repeat(400);
        let src = format!("fn f() {{\n    let s = \"{long}\";\n    s\n}}\n");
        let mut buffer = Buffer::from_text(&src);
        let mut view = EditorView::new(&buffer, "main.rs");
        view.toggle_wrap(); // wrap ON

        let frame = |ctx: &egui::Context, v: &mut EditorView, b: &mut Buffer, events: Vec<egui::Event>| {
            let raw = egui::RawInput { events, ..Default::default() };
            let _ = ctx.run(raw, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    v.ui(ui, b);
                });
            });
        };
        let click = |pos: egui::Pos2, pressed: bool| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        };
        // Build the row index + wrapped galleys.
        frame(&ctx, &mut view, &mut buffer, vec![]);
        // Click at several points, including deep into the wrapped region, then type + move.
        for y in [40.0f32, 90.0, 140.0, 200.0] {
            let at = egui::pos2(120.0, y);
            frame(&ctx, &mut view, &mut buffer, vec![click(at, true), click(at, false)]);
        }
        for ch in "abcdef".chars() {
            frame(&ctx, &mut view, &mut buffer, vec![egui::Event::Text(ch.to_string())]);
        }
        let none = egui::Modifiers::default();
        for k in [egui::Key::ArrowDown, egui::Key::ArrowUp, egui::Key::End, egui::Key::Home] {
            frame(&ctx, &mut view, &mut buffer, vec![
                egui::Event::Key { key: k, physical_key: None, pressed: true, repeat: false, modifiers: none },
            ]);
        }
        // Toggle wrap back off and paint once more — the mode switch must not panic either.
        view.toggle_wrap();
        frame(&ctx, &mut view, &mut buffer, vec![]);
    }

    #[test]
    fn headless_ui_typing_and_moving_in_a_c_file_does_not_panic() {
        let ctx = egui::Context::default();
        let src = "#include <stdio.h>\n\nint main(void)\n{\n    printf(\"hi\\n\");\n    return 0;\n}\n";
        let mut buffer = Buffer::from_text(src);
        let mut view = EditorView::new(&buffer, "main.c");

        // One frame with a click at the editor to take focus, then frames that inject keystrokes.
        let frame = |ctx: &egui::Context, view: &mut EditorView, buf: &mut Buffer, events: Vec<egui::Event>| {
            let raw = egui::RawInput { events, ..Default::default() };
            let _ = ctx.run(raw, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    view.ui(ui, buf);
                });
            });
        };

        let click = |pos: egui::Pos2, pressed: bool| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        };
        let key = |k: egui::Key, m: egui::Modifiers| egui::Event::Key {
            key: k,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: m,
        };

        // Focus the editor: press+release inside its area (top-left of the central panel).
        let at = egui::pos2(80.0, 80.0);
        frame(&ctx, &mut view, &mut buffer, vec![click(at, true), click(at, false)]);

        // Type text (including brackets/quotes that hit auto-pair) as Text + Key events.
        for ch in "int q = arr[idx](x);".chars() {
            frame(&ctx, &mut view, &mut buffer, vec![egui::Event::Text(ch.to_string())]);
        }
        // Move around with arrows, Home/End, and word motion — the "moving around" the user did.
        let none = egui::Modifiers::default();
        let ctrlm = egui::Modifiers { command: true, ..Default::default() };
        for k in [egui::Key::ArrowLeft, egui::Key::ArrowUp, egui::Key::ArrowDown, egui::Key::ArrowRight, egui::Key::Home, egui::Key::End] {
            frame(&ctx, &mut view, &mut buffer, vec![key(k, none)]);
            frame(&ctx, &mut view, &mut buffer, vec![key(k, ctrlm)]);
        }
        // Backspace through some of it (exercises delete-pair + reflow paint).
        for _ in 0..8 {
            frame(&ctx, &mut view, &mut buffer, vec![key(egui::Key::Backspace, none)]);
        }
        // A couple of idle repaints so the final match-pair state paints.
        frame(&ctx, &mut view, &mut buffer, vec![]);
        frame(&ctx, &mut view, &mut buffer, vec![]);
    }

    /// Repro harness for "typing/moving in a new C file crashes": replay typing an entire C
    /// program through the app's real Text path (typed_char → else insert), recomputing the
    /// per-frame bracket match each step, then sweep the caret across every char boundary
    /// recomputing again. Any index/slice/unwrap panic surfaces here with a backtrace.
    #[test]
    fn typing_and_moving_through_a_c_program_never_panics() {
        let program =
            "#include <stdio.h>\n\nint main(void)\n{\n    printf(\"hello\\n\");\n    return 0;\n}\n";
        let (mut v, mut b) = setup("");
        v.selections = Selections::single(0);
        let mut t = 0.0;
        for ch in program.chars() {
            v.brackets.refresh(b.rope(), b.generation);
            let _ = v.compute_match_pair(b.rope()); // the per-frame paint read
            if !v.typed_char(&mut b, ch, t) {
                v.insert(&mut b, &ch.to_string(), EditKind::InsertText, t);
            }
            t += 0.1;
        }
        // Sweep the caret to every char boundary; recompute match + probe delete-pair each stop.
        let rope = b.rope().clone();
        for ci in 0..=rope.len_chars() {
            let pos = rope.char_to_byte(ci);
            v.selections = Selections::single(pos);
            v.brackets.refresh(b.rope(), b.generation);
            let _ = v.compute_match_pair(b.rope());
            let _ = v.jump_to_matching_bracket(&mut b); // moves caret; must never panic
        }
    }

    #[test]
    fn quote_pairing_reads_whole_chars_next_to_non_ascii() {
        // After a multi-byte word char (é = C3 A9), a quote must NOT auto-close. The old byte read
        // saw the continuation byte 0xA9 → '©' (not a word char) and wrongly paired, giving `café''`;
        // reading the whole char é (alphanumeric) correctly suppresses it.
        let (mut v, mut b) = setup("café");
        v.selections = Selections::single(b.rope().len_bytes()); // caret after é
        assert!(!v.typed_char(&mut b, '\'', 0.0), "no pairing after a non-ASCII word char");
        assert_eq!(b.rope().to_string(), "café");
        // A bracket still pairs there (brackets ignore the word heuristic entirely).
        let (mut v, mut b) = setup("café");
        v.selections = Selections::single(b.rope().len_bytes());
        assert!(v.typed_char(&mut b, '(', 0.0));
        assert_eq!(b.rope().to_string(), "café()");
    }
}
