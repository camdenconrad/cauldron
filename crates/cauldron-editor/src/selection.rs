//! Caret + selection model and cursor motion — mirrors JetBrains/IntelliJ editor semantics.
//!
//! A [`Selection`] is an `anchor`→`head` pair of BYTE offsets (head = the moving caret); a
//! collapsed selection (anchor == head) is a bare caret. [`Selections`] holds a multi-caret set
//! with one primary. All motion is expressed as a [`Motion`] applied to every selection, matching
//! IntelliJ's rules: grapheme-granular left/right, camel-agnostic word stops, a smart Home that
//! toggles between first-non-blank and column 0, vertical motion that preserves a goal column, and
//! collapse-to-near-edge when an unextended arrow lands on a range.
//!
//! Offsets are always on char boundaries (the widget only ever hands us caret positions it got
//! from here or from a galley hit-test, both boundary-safe). Nothing here mutates text — the view
//! turns these selections into [`crate::buffer::Transaction`]s through the one apply() chokepoint.

use std::ops::Range;

use ropey::Rope;
use unicode_segmentation::UnicodeSegmentation;

use crate::buffer::SelectionSnapshot;

/// A directed selection in byte offsets. `head` is the caret; `anchor` is the fixed end.
/// `goal_col` remembers the grapheme column across a run of vertical moves (JetBrains keeps the
/// caret's "sticky" column when you arrow up/down through short lines).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: usize,
    pub head: usize,
    pub goal_col: Option<usize>,
}

impl Selection {
    /// A collapsed caret at `pos`.
    pub fn at(pos: usize) -> Self {
        Self { anchor: pos, head: pos, goal_col: None }
    }

    /// The covered byte range, ordered low..high.
    pub fn range(&self) -> Range<usize> {
        self.anchor.min(self.head)..self.anchor.max(self.head)
    }

    /// True when nothing is selected (a bare caret).
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }
}

/// One cursor motion. Direction/granularity only — extend-vs-collapse is a separate flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    Left,
    Right,
    WordLeft,
    WordRight,
    /// Smart Home: first non-blank ⇄ column 0.
    LineStart,
    LineEnd,
    Up,
    Down,
    BufStart,
    BufEnd,
}

/// A multi-caret selection set. Always non-empty; `primary` indexes the caret the viewport follows.
#[derive(Debug, Clone)]
pub struct Selections {
    pub ranges: Vec<Selection>,
    pub primary: usize,
}

impl Default for Selections {
    fn default() -> Self {
        Self::single(0)
    }
}

impl Selections {
    /// A single caret at `pos`.
    pub fn single(pos: usize) -> Self {
        Self { ranges: vec![Selection::at(pos)], primary: 0 }
    }

    /// The primary selection (the one the viewport scrolls to keep visible).
    pub fn primary(&self) -> Selection {
        self.ranges[self.primary]
    }

    /// Replace everything with one caret at `pos`.
    pub fn set_single(&mut self, pos: usize) {
        self.ranges.clear();
        self.ranges.push(Selection::at(pos));
        self.primary = 0;
    }

    /// Select the whole buffer with a single selection (Ctrl+A).
    pub fn select_all(&mut self, rope: &Rope) {
        self.ranges.clear();
        self.ranges.push(Selection { anchor: 0, head: rope.len_bytes(), goal_col: None });
        self.primary = 0;
    }

    /// Apply `motion` to every selection. `extend` keeps each anchor fixed (Shift+arrow); otherwise
    /// each selection collapses onto its new caret. Overlapping selections are merged afterward.
    pub fn move_all(&mut self, rope: &Rope, motion: Motion, extend: bool) {
        for sel in &mut self.ranges {
            apply_motion(sel, rope, motion, extend);
        }
        self.merge_overlaps();
    }

    /// Snapshot the caret set as opaque `(anchor, head)` byte pairs for the undo history.
    pub fn snapshot(&self) -> SelectionSnapshot {
        SelectionSnapshot {
            ranges: self.ranges.iter().map(|s| (s.anchor, s.head)).collect(),
            primary: self.primary,
        }
    }

    /// Rebuild a selection set from an undo snapshot (goal columns reset; primary clamped).
    pub fn from_snapshot(snap: &SelectionSnapshot) -> Self {
        if snap.ranges.is_empty() {
            return Self::single(0);
        }
        let ranges: Vec<Selection> =
            snap.ranges.iter().map(|&(anchor, head)| Selection { anchor, head, goal_col: None }).collect();
        let primary = snap.primary.min(ranges.len() - 1);
        Self { ranges, primary }
    }

    /// Alt+Click: add a bare caret at `byte`, or remove it if one is already exactly there (never
    /// drops below one caret). The added caret becomes primary.
    pub fn toggle_caret(&mut self, byte: usize) {
        if let Some(i) = self.ranges.iter().position(|s| s.is_empty() && s.head == byte) {
            if self.ranges.len() > 1 {
                self.ranges.remove(i);
                // Keep `primary` pointing at the SAME caret across the index shift.
                if self.primary > i {
                    self.primary -= 1;
                } else {
                    self.primary = self.primary.min(self.ranges.len() - 1);
                }
            }
            return;
        }
        self.ranges.push(Selection::at(byte));
        self.primary = self.ranges.len() - 1;
        self.merge_overlaps();
    }

    /// Clone Caret Above/Below: add a caret on the adjacent line at the primary caret's grapheme
    /// column. No-op at the buffer edge. The clone becomes primary (so repeats keep extending).
    pub fn add_caret_vertical(&mut self, rope: &Rope, down: bool) {
        let p = self.ranges[self.primary];
        let line = rope.byte_to_line(p.head);
        let last = rope.len_lines().saturating_sub(1);
        let target = if down {
            if line >= last {
                return;
            }
            line + 1
        } else {
            if line == 0 {
                return;
            }
            line - 1
        };
        let col = grapheme_col(rope, p.head);
        let byte = byte_at_col(rope, target, col);
        self.ranges.push(Selection::at(byte));
        self.primary = self.ranges.len() - 1;
        self.merge_overlaps();
    }

    /// Add Selection for Next Occurrence (Alt+J). A bare primary caret first selects the word under
    /// it; otherwise the next literal match of the primary's text (searching forward, wrapping) is
    /// added as a new selection. Returns false when there is nothing to add (all occurrences chosen).
    pub fn add_next_occurrence(&mut self, rope: &Rope) -> bool {
        let p = self.ranges[self.primary];
        if p.is_empty() {
            let r = word_range(rope, p.head);
            if r.start == r.end {
                return false;
            }
            self.ranges[self.primary] = Selection { anchor: r.start, head: r.end, goal_col: None };
            return true;
        }
        let needle: String = rope.byte_slice(p.range()).into();
        if needle.is_empty() {
            return false;
        }
        // Walk forward from the PRIMARY (most-recently-added) selection, skipping matches that are
        // already selected — so wrapping still reaches unselected occurrences that sit between
        // earlier selections. Each skipped match is a distinct existing selection, so at most
        // ranges.len() skips can happen before we either find a fresh match or have seen them all.
        let mut from = p.range().end;
        for _ in 0..=self.ranges.len() {
            let Some(r) = find_next(rope, &needle, from) else { return false };
            if !self.ranges.iter().any(|s| s.range() == r) {
                self.ranges.push(Selection { anchor: r.start, head: r.end, goal_col: None });
                self.primary = self.ranges.len() - 1;
                self.merge_overlaps();
                return true;
            }
            from = r.end; // already selected — continue past it
        }
        false // every occurrence is already selected
    }

    /// Select All Occurrences (Ctrl+Alt+Shift+J) of the primary's text (word under a bare caret).
    pub fn select_all_occurrences(&mut self, rope: &Rope) {
        let p = self.ranges[self.primary];
        let (needle, anchor_range) = if p.is_empty() {
            let r = word_range(rope, p.head);
            if r.start == r.end {
                return;
            }
            (rope.byte_slice(r.clone()).to_string(), r)
        } else {
            (rope.byte_slice(p.range()).to_string(), p.range())
        };
        if needle.is_empty() {
            return;
        }
        let text = rope.to_string();
        let mut ranges = Vec::new();
        let mut idx = 0;
        while let Some(rel) = text.get(idx..).and_then(|s| s.find(&needle)) {
            let b = idx + rel;
            ranges.push(Selection { anchor: b, head: b + needle.len(), goal_col: None });
            idx = b + needle.len();
        }
        if !ranges.is_empty() {
            self.primary = ranges.iter().position(|s| s.range() == anchor_range).unwrap_or(0);
            self.ranges = ranges;
        }
    }

    /// Alt+drag column (rectangular) selection: one selection per line between the anchor's and
    /// head's lines, spanning the anchor's grapheme column to the head's, each clamped to its
    /// line's content end (a line shorter than the anchor column contributes a bare caret at its
    /// end — JetBrains behavior). The primary is the selection on the head's line so continued
    /// dragging extends from the pointer.
    pub fn set_column_selection(&mut self, rope: &Rope, anchor: usize, head: usize) {
        let a_line = rope.byte_to_line(anchor);
        let h_line = rope.byte_to_line(head);
        let a_col = grapheme_col(rope, anchor);
        let h_col = grapheme_col(rope, head);
        let (top, bot) = (a_line.min(h_line), a_line.max(h_line));
        let mut ranges = Vec::with_capacity(bot - top + 1);
        for line in top..=bot {
            let a = byte_at_col(rope, line, a_col);
            let h = byte_at_col(rope, line, h_col);
            ranges.push(Selection { anchor: a, head: h, goal_col: None });
        }
        let primary = if h_line >= a_line { ranges.len() - 1 } else { 0 };
        self.ranges = ranges;
        self.primary = primary;
        self.merge_overlaps();
    }

    /// Unselect Occurrence (Alt+Shift+J): drop the most-recently added selection (keeps ≥1).
    pub fn unselect_last_occurrence(&mut self) {
        if self.ranges.len() > 1 {
            self.ranges.pop();
            self.primary = self.primary.min(self.ranges.len() - 1);
        }
    }

    /// Clamp every offset to `[0, len]` on a grapheme boundary and merge — call after an external
    /// edit (undo/redo) that may have shrunk the buffer under the carets.
    pub fn clamp(&mut self, rope: &Rope) {
        let len = rope.len_bytes();
        for sel in &mut self.ranges {
            sel.anchor = snap_boundary(rope, sel.anchor.min(len));
            sel.head = snap_boundary(rope, sel.head.min(len));
            sel.goal_col = None;
        }
        self.merge_overlaps();
    }

    /// Merge selections that strictly overlap — plus bare carets sitting on another selection's
    /// boundary (so duplicate carets collapse). TOUCHING non-empty selections stay separate, the
    /// JetBrains behavior (adjacent occurrence selections in "abab" must remain two). Keeps the
    /// primary caret alive; a merged range spans both, oriented like the later NON-EMPTY one.
    pub(crate) fn merge_overlaps(&mut self) {
        if self.ranges.len() < 2 {
            return;
        }
        // Track the primary by IDENTITY (its original index), not by head value: when the
        // primary caret is absorbed into another selection its head vanishes from the merged
        // set, and matching on head then fell back to an arbitrary (last) selection — the
        // viewport silently followed the wrong caret. Whichever merged entry the primary's
        // original index lands in becomes the new primary.
        let old_primary = self.primary;
        let mut new_primary = 0;
        let mut idx: Vec<usize> = (0..self.ranges.len()).collect();
        idx.sort_by_key(|&i| self.ranges[i].range().start);
        let mut merged: Vec<Selection> = Vec::with_capacity(self.ranges.len());
        for &i in &idx {
            let cur = self.ranges[i];
            let absorb = merged.last().is_some_and(|prev| {
                let touching = cur.range().start == prev.range().end;
                cur.range().start < prev.range().end
                    || (touching && (cur.is_empty() || prev.is_empty()))
            });
            match merged.last_mut() {
                Some(prev) if absorb => {
                    let lo = prev.range().start.min(cur.range().start);
                    let hi = prev.range().end.max(cur.range().end);
                    // An empty `cur` (bare caret) adds no span — keep `prev`'s orientation.
                    let forward = if cur.is_empty() { prev.head >= prev.anchor } else { cur.head >= cur.anchor };
                    *prev = Selection {
                        anchor: if forward { lo } else { hi },
                        head: if forward { hi } else { lo },
                        goal_col: None,
                    };
                }
                _ => merged.push(cur),
            }
            // After handling `i` it occupies the last merged entry (whether pushed or absorbed).
            if i == old_primary {
                new_primary = merged.len() - 1;
            }
        }
        self.primary = new_primary;
        self.ranges = merged;
    }
}

/// Move one selection's caret by `motion`; collapse onto it unless `extend`.
fn apply_motion(sel: &mut Selection, rope: &Rope, motion: Motion, extend: bool) {
    let mut goal = None;
    let head = match motion {
        // Unextended horizontal on a range collapses to the near edge (IntelliJ behavior).
        Motion::Left if !extend && !sel.is_empty() => sel.range().start,
        Motion::Right if !extend && !sel.is_empty() => sel.range().end,
        Motion::Left => prev_grapheme(rope, sel.head),
        Motion::Right => next_grapheme(rope, sel.head),
        Motion::WordLeft => prev_word(rope, sel.head),
        Motion::WordRight => next_word(rope, sel.head),
        Motion::LineStart => smart_home(rope, sel.head),
        Motion::LineEnd => line_content_end(rope, sel.head),
        Motion::Up | Motion::Down => {
            let col = sel.goal_col.unwrap_or_else(|| grapheme_col(rope, sel.head));
            goal = Some(col);
            vertical(rope, sel.head, col, matches!(motion, Motion::Down))
        }
        Motion::BufStart => 0,
        Motion::BufEnd => rope.len_bytes(),
    };
    sel.head = head;
    sel.goal_col = goal;
    if !extend {
        sel.anchor = head;
    }
}

// ---------------------------------------------------------------------------------------------
// grapheme boundaries (a caret always sits on one)
// ---------------------------------------------------------------------------------------------

/// A window (in bytes) large enough to contain any realistic grapheme cluster in source code.
const GRAPHEME_WINDOW: usize = 32;

/// The next grapheme-cluster boundary at or after `byte` (returns `len` at the end).
pub fn next_grapheme(rope: &Rope, byte: usize) -> usize {
    let len = rope.len_bytes();
    if byte >= len {
        return len;
    }
    let end = rope.byte_to_char(byte + GRAPHEME_WINDOW.min(len - byte));
    let s: String = rope.slice(rope.byte_to_char(byte)..end).into();
    byte + s.graphemes(true).next().map_or(1, str::len)
}

/// The previous grapheme-cluster boundary strictly before `byte` (returns 0 at the start).
pub fn prev_grapheme(rope: &Rope, byte: usize) -> usize {
    if byte == 0 {
        return 0;
    }
    let start = byte.saturating_sub(GRAPHEME_WINDOW);
    let s: String = rope.slice(rope.byte_to_char(start)..rope.byte_to_char(byte)).into();
    byte - s.graphemes(true).next_back().map_or(1, str::len)
}

/// Snap an arbitrary byte offset onto the nearest grapheme boundary at or before it.
fn snap_boundary(rope: &Rope, byte: usize) -> usize {
    let len = rope.len_bytes();
    if byte >= len {
        return len;
    }
    // char boundary first (cheap), then it is already a valid caret spot for our purposes.
    rope.char_to_byte(rope.byte_to_char(byte))
}

// ---------------------------------------------------------------------------------------------
// word motion (IntelliJ-style char-class runs)
// ---------------------------------------------------------------------------------------------

#[derive(PartialEq, Eq, Clone, Copy)]
enum Class {
    Newline,
    Space,
    Word,
    Punct,
}

fn classify(c: char) -> Class {
    if c == '\n' || c == '\r' {
        Class::Newline
    } else if c.is_whitespace() {
        Class::Space
    } else if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else {
        Class::Punct
    }
}

/// Word boundary to the right: consume the run of the class under the caret (word or punctuation),
/// then trailing spaces — stopping at a line break, which is its own single step.
pub fn next_word(rope: &Rope, byte: usize) -> usize {
    let total = rope.len_chars();
    let mut c = rope.byte_to_char(byte);
    if c >= total {
        return rope.len_bytes();
    }
    let char_at = |c: usize| rope.char(c);
    match classify(char_at(c)) {
        Class::Newline => c += 1, // a blank line: one step past the break
        cat @ (Class::Word | Class::Punct) => {
            while c < total && classify(char_at(c)) == cat {
                c += 1;
            }
            while c < total && classify(char_at(c)) == Class::Space {
                c += 1;
            }
        }
        Class::Space => {
            while c < total && classify(char_at(c)) == Class::Space {
                c += 1;
            }
        }
    }
    rope.char_to_byte(c)
}

/// Word boundary to the left: skip spaces, then consume the run of the class just before the caret
/// — a line break counts as one step.
pub fn prev_word(rope: &Rope, byte: usize) -> usize {
    let mut c = rope.byte_to_char(byte);
    if c == 0 {
        return 0;
    }
    let char_at = |c: usize| rope.char(c);
    while c > 0 && classify(char_at(c - 1)) == Class::Space {
        c -= 1;
    }
    if c > 0 {
        match classify(char_at(c - 1)) {
            Class::Newline => c -= 1,
            cat => {
                while c > 0 && classify(char_at(c - 1)) == cat {
                    c -= 1;
                }
            }
        }
    }
    rope.char_to_byte(c)
}

/// The word (or run of identical punctuation) surrounding `byte` — for double-click select. On a
/// space run, selects the whitespace; at a hard boundary, the zero-width point.
pub fn word_range(rope: &Rope, byte: usize) -> Range<usize> {
    let total = rope.len_chars();
    let c = rope.byte_to_char(byte);
    if c >= total {
        return byte..byte;
    }
    let char_at = |c: usize| rope.char(c);
    let cat = classify(char_at(c));
    if cat == Class::Newline {
        return byte..byte;
    }
    let mut lo = c;
    while lo > 0 && classify(char_at(lo - 1)) == cat {
        lo -= 1;
    }
    let mut hi = c;
    while hi < total && classify(char_at(hi)) == cat {
        hi += 1;
    }
    rope.char_to_byte(lo)..rope.char_to_byte(hi)
}

/// Next literal occurrence of `needle` at or after byte `from`, wrapping to the start of the
/// buffer. `None` only when `needle` never occurs. (Materializes the rope once — occurrence search
/// is a user gesture, not a per-frame path; a rope-native searcher can replace this later.)
fn find_next(rope: &Rope, needle: &str, from: usize) -> Option<Range<usize>> {
    let text = rope.to_string();
    let start = from.min(text.len());
    if let Some(rel) = text.get(start..).and_then(|s| s.find(needle)) {
        let b = start + rel;
        return Some(b..b + needle.len());
    }
    // wrap
    text.find(needle).map(|b| b..b + needle.len())
}

/// The whole line containing `byte`, including its trailing newline — for triple-click select.
pub fn line_range(rope: &Rope, byte: usize) -> Range<usize> {
    let line = rope.byte_to_line(byte);
    let start = rope.line_to_byte(line);
    let end = if line + 1 < rope.len_lines() {
        rope.line_to_byte(line + 1)
    } else {
        rope.len_bytes()
    };
    start..end
}

// ---------------------------------------------------------------------------------------------
// line + vertical motion
// ---------------------------------------------------------------------------------------------

/// Smart Home: the first non-blank column, unless the caret is already there (or before it), in
/// which case column 0 — JetBrains' Home toggle.
fn smart_home(rope: &Rope, byte: usize) -> usize {
    let line = rope.byte_to_line(byte);
    let line_start = rope.line_to_byte(line);
    let first_non_blank = {
        let mut b = line_start;
        for ch in rope.line(line).chars() {
            if ch == '\n' || ch == '\r' || !ch.is_whitespace() {
                break;
            }
            b += ch.len_utf8();
        }
        b
    };
    if byte > first_non_blank {
        first_non_blank
    } else {
        line_start
    }
}

/// End of the line's CONTENT (before the trailing `\n`/`\r`).
fn line_content_end(rope: &Rope, byte: usize) -> usize {
    let line = rope.byte_to_line(byte);
    let start = rope.line_to_byte(line);
    let slice = rope.line(line);
    let mut end = start + slice.len_bytes();
    let mut chars = slice.chars_at(slice.len_chars());
    while let Some(c) = chars.prev() {
        if c == '\n' || c == '\r' {
            end -= c.len_utf8();
        } else {
            break;
        }
    }
    end
}

/// Grapheme column of `byte` within its line (0-based).
fn grapheme_col(rope: &Rope, byte: usize) -> usize {
    let line = rope.byte_to_line(byte);
    let start = rope.line_to_byte(line);
    let s: String = rope.slice(rope.byte_to_char(start)..rope.byte_to_char(byte)).into();
    s.graphemes(true).count()
}

/// Byte offset of grapheme column `col` on `line`, clamped to the line's content end.
fn byte_at_col(rope: &Rope, line: usize, col: usize) -> usize {
    let start = rope.line_to_byte(line);
    let content_end = line_content_end(rope, start);
    let mut b = start;
    let mut seen = 0;
    while seen < col && b < content_end {
        b = next_grapheme(rope, b);
        seen += 1;
    }
    b.min(content_end)
}

/// Move up/down one line, landing at `goal` column (clamped to that line's length).
fn vertical(rope: &Rope, byte: usize, goal: usize, down: bool) -> usize {
    let line = rope.byte_to_line(byte);
    let last = rope.len_lines().saturating_sub(1);
    let target = if down {
        if line >= last {
            return rope.len_bytes(); // already on the last line: go to buffer end
        }
        line + 1
    } else {
        if line == 0 {
            return 0; // already on the first line: go to buffer start
        }
        line - 1
    };
    byte_at_col(rope, target, goal)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rope(s: &str) -> Rope {
        Rope::from_str(s)
    }

    /// When the primary caret is absorbed by an overlapping selection, `primary` must follow
    /// it into the merged entry — not silently jump to an arbitrary (last) selection.
    #[test]
    fn merge_overlaps_keeps_primary_by_identity() {
        // Three selections; the PRIMARY (index 1) overlaps selection 0 and gets absorbed.
        let mut sels = Selections {
            ranges: vec![
                Selection { anchor: 0, head: 6, goal_col: None },   // 0..6
                Selection { anchor: 3, head: 8, goal_col: None },   // 3..8 (PRIMARY, overlaps 0)
                Selection { anchor: 20, head: 22, goal_col: None }, // far away, untouched
            ],
            primary: 1,
        };
        sels.merge_overlaps();
        // 0..6 and 3..8 merge to 0..8; the far one stays. Primary must point at the MERGED
        // 0..8 entry (which contains the old primary), not the far 20..22.
        assert_eq!(sels.ranges.len(), 2);
        let p = sels.primary();
        assert_eq!((p.range().start, p.range().end), (0, 8), "primary followed into the merged span");
    }

    /// Byte offset of the first occurrence of `needle`.
    fn at(s: &str, needle: &str) -> usize {
        s.find(needle).unwrap()
    }

    #[test]
    fn grapheme_motion_handles_multibyte() {
        let r = rope("aé𐍈b"); // a=1, é=2, 𐍈=4, b=1 bytes
        assert_eq!(next_grapheme(&r, 0), 1); // a
        assert_eq!(next_grapheme(&r, 1), 3); // é
        assert_eq!(next_grapheme(&r, 3), 7); // 𐍈
        assert_eq!(next_grapheme(&r, 7), 8); // b
        assert_eq!(next_grapheme(&r, 8), 8); // end
        assert_eq!(prev_grapheme(&r, 8), 7);
        assert_eq!(prev_grapheme(&r, 7), 3);
        assert_eq!(prev_grapheme(&r, 3), 1);
        assert_eq!(prev_grapheme(&r, 1), 0);
        assert_eq!(prev_grapheme(&r, 0), 0);
    }

    #[test]
    fn word_motion_stops_at_class_runs() {
        let s = "foo_bar + baz();\n";
        let r = rope(s);
        // from start: over the identifier, then trailing space → at '+'
        assert_eq!(next_word(&r, 0), at(s, "+"));
        // over '+', then space → at 'baz'
        assert_eq!(next_word(&r, at(s, "+")), at(s, "baz"));
        // over 'baz' → at '(' (punct run)
        assert_eq!(next_word(&r, at(s, "baz")), at(s, "("));
        // backward from 'baz' → back to '+'
        assert_eq!(prev_word(&r, at(s, "baz")), at(s, "+"));
        // backward from '+': skip space then over the identifier → start
        assert_eq!(prev_word(&r, at(s, "+")), 0);
    }

    #[test]
    fn word_motion_treats_newline_as_a_step() {
        let s = "ab\ncd";
        let r = rope(s);
        assert_eq!(next_word(&r, at(s, "ab")), 2); // over 'ab' → before '\n'
        assert_eq!(next_word(&r, 2), 3); // the '\n' itself → line 2 start
        assert_eq!(prev_word(&r, 3), 2); // back over the '\n'
    }

    #[test]
    fn smart_home_toggles_first_nonblank_and_col0() {
        let s = "    code\n";
        let r = rope(s);
        let eol = at(s, "code") + 2; // somewhere inside 'code'
        assert_eq!(smart_home(&r, eol), at(s, "code")); // → first non-blank
        assert_eq!(smart_home(&r, at(s, "code")), 0); // already there → col 0
        assert_eq!(smart_home(&r, 0), 0);
    }

    #[test]
    fn line_end_stops_before_newline() {
        let s = "hello\nx";
        let r = rope(s);
        assert_eq!(line_content_end(&r, 0), 5);
    }

    #[test]
    fn vertical_preserves_goal_column_over_short_line() {
        // caret at col 6 of line 0; line 1 is short (col clamps), line 2 long again.
        let s = "abcdefgh\nxy\nZZZZZZZZ\n";
        let r = rope(s);
        let mut sel = Selection::at(6); // 'g' on line 0
        apply_motion(&mut sel, &r, Motion::Down, false); // → line 1, clamped to end (col 2)
        assert_eq!(sel.head, at(s, "xy") + 2);
        assert_eq!(sel.goal_col, Some(6));
        apply_motion(&mut sel, &r, Motion::Down, false); // → line 2, back to col 6
        assert_eq!(sel.head, at(s, "ZZZZZZZZ") + 6);
    }

    #[test]
    fn unextended_arrow_collapses_selection_to_edge() {
        let r = rope("hello world");
        let mut s = Selections::single(0);
        s.move_all(&r, Motion::WordRight, true); // select "hello "
        assert!(!s.primary().is_empty());
        s.move_all(&r, Motion::Left, false); // collapse to LOW edge, don't step past
        assert_eq!(s.primary().head, 0);
        assert!(s.primary().is_empty());
    }

    #[test]
    fn overlapping_selections_merge() {
        let mut s = Selections {
            ranges: vec![
                Selection { anchor: 0, head: 4, goal_col: None },
                Selection { anchor: 3, head: 7, goal_col: None },
                Selection { anchor: 9, head: 10, goal_col: None },
            ],
            primary: 1,
        };
        s.merge_overlaps();
        assert_eq!(s.ranges.len(), 2); // [0,7) merged, [9,10) separate
        assert_eq!(s.ranges[0].range(), 0..7);
        assert_eq!(s.ranges[1].range(), 9..10);
    }

    #[test]
    fn word_and_line_range() {
        let s = "foo bar\nbaz\n";
        let r = rope(s);
        assert_eq!(word_range(&r, at(s, "bar") + 1), at(s, "bar")..at(s, "bar") + 3);
        assert_eq!(line_range(&r, at(s, "bar")), 0..at(s, "baz")); // whole line 0 incl '\n'
        assert_eq!(line_range(&r, at(s, "baz")), at(s, "baz")..s.len());
    }

    #[test]
    fn snapshot_roundtrips() {
        let s = Selections {
            ranges: vec![
                Selection { anchor: 1, head: 4, goal_col: Some(9) },
                Selection { anchor: 6, head: 6, goal_col: None },
            ],
            primary: 1,
        };
        let snap = s.snapshot();
        assert_eq!(snap.ranges, vec![(1, 4), (6, 6)]);
        assert_eq!(snap.primary, 1);
        let back = Selections::from_snapshot(&snap);
        assert_eq!(back.ranges.len(), 2);
        assert_eq!(back.primary, 1);
        assert_eq!(back.ranges[0].anchor, 1);
        assert_eq!(back.ranges[0].head, 4);
        assert_eq!(back.ranges[0].goal_col, None); // goal col is dropped on restore
        assert_eq!(Selections::from_snapshot(&SelectionSnapshot::default()).ranges.len(), 1);
    }

    #[test]
    fn toggle_caret_adds_and_removes() {
        let mut s = Selections::single(0);
        s.toggle_caret(5);
        assert_eq!(s.ranges.len(), 2);
        assert_eq!(s.primary().head, 5);
        s.toggle_caret(5); // toggling the same spot removes it
        assert_eq!(s.ranges.len(), 1);
        s.toggle_caret(0); // never drops below one caret
        assert_eq!(s.ranges.len(), 1);
    }

    #[test]
    fn clone_caret_above_below() {
        let s_txt = "abcdef\nghijkl\nmnopqr\n";
        let r = rope(s_txt);
        let mut s = Selections::single(at(s_txt, "ijkl")); // line 1, col 2
        s.add_caret_vertical(&r, true); // clone below → line 2 col 2 ('o')
        assert_eq!(s.ranges.len(), 2);
        assert_eq!(s.primary().head, at(s_txt, "opqr"));
        s.add_caret_vertical(&r, false); // clone above the new primary → back to line 1 col 2
        assert_eq!(s.ranges.len(), 2); // merged with the existing line-1 caret
    }

    #[test]
    fn clone_caret_noop_at_edges() {
        let r = rope("only\n");
        let mut s = Selections::single(2); // line 0
        s.add_caret_vertical(&r, false); // up from first line → no-op
        assert_eq!(s.ranges.len(), 1);
    }

    #[test]
    fn add_next_occurrence_selects_word_then_adds_and_wraps() {
        let s_txt = "foo x foo y foo";
        let r = rope(s_txt);
        let mut s = Selections::single(1); // bare caret inside the first "foo"
        assert!(s.add_next_occurrence(&r)); // → selects word "foo" #1
        assert_eq!(s.ranges.len(), 1);
        assert_eq!(s.primary().range(), 0..3);
        assert!(s.add_next_occurrence(&r)); // → adds "foo" #2
        assert_eq!(s.ranges.len(), 2);
        assert_eq!(s.primary().range(), 6..9);
        assert!(s.add_next_occurrence(&r)); // → adds "foo" #3
        assert_eq!(s.ranges.len(), 3);
        assert_eq!(s.primary().range(), 12..15);
        assert!(!s.add_next_occurrence(&r)); // all chosen → wraps onto an existing selection → false
        assert_eq!(s.ranges.len(), 3);
    }

    #[test]
    fn add_next_occurrence_reaches_skipped_earlier_matches_after_wrap() {
        // Start from the THIRD of four matches: wrap must still reach #1 AND #2.
        let s_txt = "foo foo foo foo";
        let r = rope(s_txt);
        let mut s = Selections::single(9); // bare caret in the 3rd "foo" (8..11)
        assert!(s.add_next_occurrence(&r)); // selects 8..11
        assert!(s.add_next_occurrence(&r)); // adds 12..15
        assert!(s.add_next_occurrence(&r)); // wraps → adds 0..3
        assert_eq!(s.primary().range(), 0..3);
        assert!(s.add_next_occurrence(&r), "must reach the skipped 2nd match"); // adds 4..7
        assert_eq!(s.primary().range(), 4..7);
        assert!(!s.add_next_occurrence(&r)); // all four selected
        assert_eq!(s.ranges.len(), 4);
    }

    #[test]
    fn adjacent_occurrences_stay_separate_selections() {
        // "abab": two touching "ab" matches must remain TWO selections (JetBrains keeps them
        // distinct); only strictly-overlapping ranges — or bare carets on a boundary — merge.
        let s_txt = "abab";
        let r = rope(s_txt);
        let mut s = Selections {
            ranges: vec![Selection { anchor: 0, head: 2, goal_col: None }],
            primary: 0,
        };
        assert!(s.add_next_occurrence(&r));
        assert_eq!(s.ranges.len(), 2, "touching matches must not merge: {:?}", s.ranges);
        assert_eq!(s.ranges[0].range(), 0..2);
        assert_eq!(s.ranges[1].range(), 2..4);
        // Duplicate carets at one offset still merge.
        let mut c = Selections { ranges: vec![Selection::at(3), Selection::at(3)], primary: 0 };
        c.merge_overlaps();
        assert_eq!(c.ranges.len(), 1);
    }

    #[test]
    fn toggle_caret_removal_keeps_primary_on_same_caret() {
        let mut s = Selections {
            ranges: vec![Selection::at(0), Selection::at(2), Selection::at(4), Selection::at(6)],
            primary: 2, // points at the caret at byte 4
        };
        s.toggle_caret(2); // remove the caret BELOW primary
        assert_eq!(s.ranges.len(), 3);
        assert_eq!(s.primary().head, 4, "primary must follow its caret after the removal shift");
    }

    #[test]
    fn column_selection_spans_lines_and_clamps_short_ones() {
        let s_txt = "abcdef\nxy\nlmnopq\n";
        let r = rope(s_txt);
        let mut s = Selections::single(0);
        // Anchor at line 0 col 2, head at line 2 col 5.
        s.set_column_selection(&r, 2, at(s_txt, "lmnopq") + 5);
        assert_eq!(s.ranges.len(), 3);
        assert_eq!(s.ranges[0].range(), 2..5); // "cde"
        // line 1 ("xy") is shorter than col 2 → bare caret at its content end
        let xy_end = at(s_txt, "xy") + 2;
        assert_eq!(s.ranges[1].range(), xy_end..xy_end);
        assert!(s.ranges[1].is_empty());
        let l2 = at(s_txt, "lmnopq");
        assert_eq!(s.ranges[2].range(), l2 + 2..l2 + 5); // "nop"
        assert_eq!(s.primary, 2, "primary rides the head line");
        // Dragging upward flips the primary to the top line.
        let mut up = Selections::single(0);
        up.set_column_selection(&r, l2 + 5, 2);
        assert_eq!(up.primary, 0);
        assert_eq!(up.ranges[0].range(), 2..5);
    }

    #[test]
    fn select_all_occurrences_picks_every_match() {
        let s_txt = "foo x foo y foo";
        let r = rope(s_txt);
        let mut s = Selections::single(1);
        s.select_all_occurrences(&r);
        assert_eq!(s.ranges.len(), 3);
        assert_eq!(s.ranges.iter().map(|x| x.range()).collect::<Vec<_>>(), vec![0..3, 6..9, 12..15]);
        s.unselect_last_occurrence();
        assert_eq!(s.ranges.len(), 2);
    }

    #[test]
    fn select_all_and_clamp() {
        let r = rope("abc\ndef");
        let mut s = Selections::single(5);
        s.select_all(&r);
        assert_eq!(s.primary().range(), 0..7);
        // Simulate the buffer shrinking under a stale caret.
        let smaller = rope("ab");
        let mut s2 = Selections::single(6);
        s2.clamp(&smaller);
        assert_eq!(s2.primary().head, 2);
    }
}
