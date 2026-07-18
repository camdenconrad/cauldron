//! The buffer: a rope mutated only through [`Buffer::apply`].
//!
//! Every mutation is a [`Transaction`] (a batch of byte-range replacements). Routing all edits
//! through one chokepoint gives us, for free and always in sync:
//! - the undo stack (inverse transactions),
//! - tree-sitter `InputEdit`s for incremental reparse,
//! - LSP incremental `didChange` events derived from the same deltas.

use ropey::Rope;

/// One byte-range replacement. Ranges are byte offsets into the buffer BEFORE the transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub start: usize,
    pub end: usize,
    pub text: String,
}

/// An atomic batch of changes. Changes must be non-overlapping and sorted ascending by `start`;
/// they are applied back-to-front so earlier offsets stay valid.
#[derive(Debug, Clone, Default)]
pub struct Transaction {
    pub changes: Vec<Change>,
}

impl Transaction {
    pub fn insert(at: usize, text: impl Into<String>) -> Self {
        Self { changes: vec![Change { start: at, end: at, text: text.into() }] }
    }
    pub fn replace(start: usize, end: usize, text: impl Into<String>) -> Self {
        Self { changes: vec![Change { start, end, text: text.into() }] }
    }
    pub fn delete(start: usize, end: usize) -> Self {
        Self::replace(start, end, "")
    }
}

/// What kind of edit produced a revision — decides undo coalescing. Only the three "typing"
/// kinds coalesce into a running group; everything else is its own undo step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKind {
    InsertText,
    DeleteBack,
    DeleteFwd,
    Newline,
    Paste,
    Cut,
    Duplicate,
    Other,
}

impl EditKind {
    fn coalescible(self) -> bool {
        matches!(self, EditKind::InsertText | EditKind::DeleteBack | EditKind::DeleteFwd)
    }
}

/// An OPAQUE caret snapshot the buffer stores with each revision and hands back on undo/redo but
/// never inspects. Plain data — references no `Selection` type — so the buffer stays caret-agnostic
/// while the view still gets JetBrains-style caret restore. `(anchor, head)` byte pairs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SelectionSnapshot {
    pub ranges: Vec<(usize, usize)>,
    pub primary: usize,
}

/// Everything `record` needs about an edit that isn't in the transaction itself.
#[derive(Debug, Clone)]
pub struct EditMeta {
    pub kind: EditKind,
    /// Number of carets (changes) in the edit; any multi-caret edit is its own undo step.
    pub carets: usize,
    /// Timestamp in seconds (egui `input.time`) for the coalescing timeout.
    pub time: f64,
    pub before: SelectionSnapshot,
    pub after: SelectionSnapshot,
}

/// One undo step: the inverse transaction to reach the prior state, plus the caret snapshots and
/// the coalescing bookkeeping. For a coalesced typing run, `tx` is a single MERGED transaction
/// (so undo/redo is always exactly one `apply_inner`).
#[derive(Debug, Clone)]
struct Revision {
    tx: Transaction,
    before: SelectionSnapshot,
    after: SelectionSnapshot,
    kind: EditKind,
    carets: usize,
    last_time: f64,
}

/// Undo/redo stacks. `open` is the "still coalescing" flag — cleared by [`Buffer::seal`] (a caret
/// move) and by any undo/redo, so the next edit starts a fresh group.
#[derive(Default)]
struct History {
    undo: Vec<Revision>,
    redo: Vec<Revision>,
    open: bool,
}

/// Max idle gap (seconds) within which consecutive same-kind typing coalesces into one undo step.
const COALESCE_TIMEOUT: f64 = 0.5;

pub struct Buffer {
    rope: Rope,
    history: History,
    /// Monotonic edit generation — stamps LSP requests so stale results are dropped.
    pub generation: u64,
}

impl Buffer {
    pub fn from_text(text: &str) -> Self {
        Self { rope: Rope::from_str(text), history: History::default(), generation: 0 }
    }

    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    pub fn len_bytes(&self) -> usize {
        self.rope.len_bytes()
    }

    /// Apply a transaction and file it in history, coalescing with the previous revision when the
    /// policy allows (same coalescible kind, single caret, adjacent, within the timeout, not
    /// sealed). Returns the inverse. This is THE edit entry point (the old `apply` is a test shim).
    pub fn record(&mut self, tx: &Transaction, meta: EditMeta) -> Transaction {
        let inverse = self.apply_inner(tx);
        self.history.redo.clear();
        if !self.try_coalesce(tx, &inverse, &meta) {
            self.history.undo.push(Revision {
                tx: inverse.clone(),
                before: meta.before,
                after: meta.after,
                kind: meta.kind,
                carets: meta.carets,
                last_time: meta.time,
            });
            self.history.open = true;
        }
        inverse
    }

    /// Try to fold this edit into the top undo revision (see coalescing policy). On success the
    /// group's stored inverse `tx` is rewritten into the current coordinate space and its `after`
    /// snapshot + timestamp advance; `before` (the group's original carets) is preserved.
    fn try_coalesce(&mut self, fwd: &Transaction, inverse: &Transaction, meta: &EditMeta) -> bool {
        if !self.history.open
            || !meta.kind.coalescible()
            || meta.carets != 1
            || fwd.changes.len() != 1
            || inverse.changes.len() != 1
        {
            return false;
        }
        let Some(top) = self.history.undo.last_mut() else { return false };
        if top.kind != meta.kind || top.carets != 1 || top.tx.changes.len() != 1 {
            return false;
        }
        if meta.time - top.last_time > COALESCE_TIMEOUT {
            return false;
        }
        let nc = &fwd.changes[0]; // new forward change
        let inv0 = &inverse.changes[0]; // its inverse
        let tc = &mut top.tx.changes[0]; // the group's stored inverse change
        match meta.kind {
            // Typing forward: the stored inverse is a pure deletion [start, end) with EMPTY text;
            // grow its end. `tc.text.is_empty()` keeps a replace-over-selection (whose inverse
            // re-inserts the replaced text) from absorbing the following keystrokes.
            EditKind::InsertText => {
                if !(nc.start == nc.end && nc.start == tc.end && tc.text.is_empty()) {
                    return false;
                }
                tc.end += inv0.end - inv0.start;
            }
            // Backspace: the stored inverse is an insertion at tc.start; move the insert point left
            // and PREPEND the freshly deleted text. (Both start AND end move — the design synthesis
            // omitted `tc.end`, which would corrupt the pure-insertion invariant.)
            EditKind::DeleteBack => {
                if !(nc.text.is_empty() && nc.end == tc.start) {
                    return false;
                }
                tc.start = inv0.start;
                tc.end = inv0.end;
                tc.text = format!("{}{}", inv0.text, tc.text);
            }
            // Forward-delete: insertion point stays; APPEND the freshly deleted text.
            EditKind::DeleteFwd => {
                if !(nc.text.is_empty() && nc.start == tc.start) {
                    return false;
                }
                tc.text.push_str(&inv0.text);
            }
            _ => return false,
        }
        top.after = meta.after.clone();
        top.last_time = meta.time;
        true
    }

    /// Close the current coalescing group — the next edit starts a fresh undo step. Call on any
    /// caret move / selection change (the PRIMARY break mechanism; adjacency is only a backstop).
    pub fn seal(&mut self) {
        self.history.open = false;
    }

    /// Undo one step, driving `on_edit(rope_after, changes)` so the caller can reparse incrementally.
    /// Returns the caret snapshot to restore (the state before the undone group), or `None` if the
    /// undo stack is empty.
    pub fn undo_with(&mut self, mut on_edit: impl FnMut(&Rope, &[Change])) -> Option<SelectionSnapshot> {
        let rev = self.history.undo.pop()?;
        let redo_tx = self.apply_inner(&rev.tx);
        on_edit(&self.rope, &rev.tx.changes);
        let before = rev.before.clone();
        self.history.redo.push(Revision {
            tx: redo_tx,
            before: rev.before,
            after: rev.after,
            kind: rev.kind,
            carets: rev.carets,
            last_time: rev.last_time,
        });
        self.history.open = false;
        Some(before)
    }

    /// Redo one step (symmetric to [`Buffer::undo_with`]). Returns the caret snapshot to restore
    /// (the state after the redone group).
    pub fn redo_with(&mut self, mut on_edit: impl FnMut(&Rope, &[Change])) -> Option<SelectionSnapshot> {
        let rev = self.history.redo.pop()?;
        let undo_tx = self.apply_inner(&rev.tx);
        on_edit(&self.rope, &rev.tx.changes);
        let after = rev.after.clone();
        self.history.undo.push(Revision {
            tx: undo_tx,
            before: rev.before,
            after: rev.after,
            kind: rev.kind,
            carets: rev.carets,
            last_time: rev.last_time,
        });
        self.history.open = false;
        Some(after)
    }

    /// Compat shim (tests + Gate-A bench + simple callers): apply as a standalone `Other`-kind
    /// revision with no caret snapshot — never coalesces. The interactive editor uses [`record`].
    pub fn apply(&mut self, tx: &Transaction) -> Transaction {
        self.record(
            tx,
            EditMeta {
                kind: EditKind::Other,
                carets: 1,
                time: 0.0,
                before: SelectionSnapshot::default(),
                after: SelectionSnapshot::default(),
            },
        )
    }

    /// Compat shim: undo one step, discarding the caret snapshot.
    pub fn undo(&mut self) -> bool {
        self.undo_with(|_, _| {}).is_some()
    }

    /// Compat shim: redo one step, discarding the caret snapshot.
    pub fn redo(&mut self) -> bool {
        self.redo_with(|_, _| {}).is_some()
    }

    fn apply_inner(&mut self, tx: &Transaction) -> Transaction {
        debug_assert!(tx.changes.windows(2).all(|w| w[0].end <= w[1].start), "changes must be sorted + disjoint");
        let n = tx.changes.len();
        // Splice back-to-front (so earlier byte offsets stay valid) and capture the removed text.
        let mut removed: Vec<String> = vec![String::new(); n];
        for i in (0..n).rev() {
            let ch = &tx.changes[i];
            let start_c = self.rope.byte_to_char(ch.start);
            let end_c = self.rope.byte_to_char(ch.end);
            removed[i] = self.rope.slice(start_c..end_c).into();
            self.rope.remove(start_c..end_c);
            self.rope.insert(start_c, &ch.text);
        }
        // Build the inverse in POST-edit coordinates: each change's inverse is shifted right by the
        // cumulative length delta of all earlier (lower-offset) changes. For a single change delta
        // is 0 (identical to the naive inverse); for multi-caret edits this is what makes undo
        // correct — the naive `ch.start` inverse only works for one change.
        let mut inverse = Vec::with_capacity(n);
        let mut delta: isize = 0;
        for (i, ch) in tx.changes.iter().enumerate() {
            let start = (ch.start as isize + delta) as usize;
            let end = start + ch.text.len();
            inverse.push(Change { start, end, text: std::mem::take(&mut removed[i]) });
            delta += ch.text.len() as isize - (ch.end - ch.start) as isize;
        }
        self.generation += 1;
        Transaction { changes: inverse }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_undo_redo_roundtrip() {
        let mut b = Buffer::from_text("hello world");
        b.apply(&Transaction::replace(6, 11, "cauldron"));
        assert_eq!(b.rope().to_string(), "hello cauldron");
        assert!(b.undo());
        assert_eq!(b.rope().to_string(), "hello world");
        assert!(b.redo());
        assert_eq!(b.rope().to_string(), "hello cauldron");
    }

    #[test]
    fn multibyte_edit() {
        let mut b = Buffer::from_text("héllo"); // é = 2 bytes
        b.apply(&Transaction::insert(3, "X")); // after é
        assert_eq!(b.rope().to_string(), "héXllo");
        assert!(b.undo());
        assert_eq!(b.rope().to_string(), "héllo");
    }

    // --- coalescing ---------------------------------------------------------------------------

    fn meta(kind: EditKind, carets: usize, time: f64) -> EditMeta {
        EditMeta { kind, carets, time, before: SelectionSnapshot::default(), after: SelectionSnapshot::default() }
    }

    /// Type `s` one char at a time starting at `at`, each as a single-caret InsertText at `t0+i*dt`.
    fn type_run(b: &mut Buffer, at: usize, s: &str, t0: f64, dt: f64) {
        let mut off = at;
        for (i, ch) in s.chars().enumerate() {
            let tx = Transaction::insert(off, ch.to_string());
            b.record(&tx, meta(EditKind::InsertText, 1, t0 + i as f64 * dt));
            off += ch.len_utf8();
        }
    }

    #[test]
    fn coalesce_groups_typed_run() {
        let mut b = Buffer::from_text("");
        type_run(&mut b, 0, "hello", 0.0, 0.1); // 5 chars within the timeout
        assert_eq!(b.rope().to_string(), "hello");
        assert_eq!(b.history.undo.len(), 1, "the run is one undo group");
        assert!(b.undo());
        assert_eq!(b.rope().to_string(), ""); // one undo removes the whole word
        assert!(b.redo());
        assert_eq!(b.rope().to_string(), "hello");
    }

    #[test]
    fn pause_breaks_group() {
        let mut b = Buffer::from_text("");
        b.record(&Transaction::insert(0, "a"), meta(EditKind::InsertText, 1, 0.0));
        b.record(&Transaction::insert(1, "b"), meta(EditKind::InsertText, 1, 0.6)); // > 0.5s later
        assert_eq!(b.history.undo.len(), 2);
        assert!(b.undo());
        assert_eq!(b.rope().to_string(), "a");
        assert!(b.undo());
        assert_eq!(b.rope().to_string(), "");
    }

    #[test]
    fn seal_breaks_group() {
        let mut b = Buffer::from_text("");
        b.record(&Transaction::insert(0, "a"), meta(EditKind::InsertText, 1, 0.0));
        b.seal(); // simulate a caret move
        b.record(&Transaction::insert(1, "b"), meta(EditKind::InsertText, 1, 0.1));
        assert_eq!(b.history.undo.len(), 2);
    }

    #[test]
    fn kind_change_breaks_group() {
        let mut b = Buffer::from_text("");
        b.record(&Transaction::insert(0, "a"), meta(EditKind::InsertText, 1, 0.0));
        // Backspace it: a DeleteBack right after InsertText must NOT coalesce.
        b.record(&Transaction::delete(0, 1), meta(EditKind::DeleteBack, 1, 0.1));
        assert_eq!(b.history.undo.len(), 2);
    }

    #[test]
    fn backspace_run_coalesces_and_restores() {
        let mut b = Buffer::from_text("xABy"); // caret conceptually after B (offset 3)
        // backspace "B" then "A"
        b.record(&Transaction::delete(2, 3), meta(EditKind::DeleteBack, 1, 0.0));
        assert_eq!(b.rope().to_string(), "xAy");
        b.record(&Transaction::delete(1, 2), meta(EditKind::DeleteBack, 1, 0.1));
        assert_eq!(b.rope().to_string(), "xy");
        assert_eq!(b.history.undo.len(), 1, "adjacent backspaces coalesce");
        assert!(b.undo());
        assert_eq!(b.rope().to_string(), "xABy", "one undo restores both deleted chars in order");
    }

    #[test]
    fn forward_delete_run_coalesces_and_restores() {
        let mut b = Buffer::from_text("xABy"); // caret before A (offset 1)
        b.record(&Transaction::delete(1, 2), meta(EditKind::DeleteFwd, 1, 0.0)); // del A
        b.record(&Transaction::delete(1, 2), meta(EditKind::DeleteFwd, 1, 0.1)); // del B
        assert_eq!(b.rope().to_string(), "xy");
        assert_eq!(b.history.undo.len(), 1);
        assert!(b.undo());
        assert_eq!(b.rope().to_string(), "xABy");
    }

    #[test]
    fn grouped_undo_is_coordinate_correct_multibyte() {
        let mut b = Buffer::from_text("");
        type_run(&mut b, 0, "héllo", 0.0, 0.05); // multibyte in the run
        assert_eq!(b.rope().to_string(), "héllo");
        assert_eq!(b.history.undo.len(), 1);
        assert!(b.undo());
        assert_eq!(b.rope().to_string(), "");
    }

    #[test]
    fn multi_caret_edit_is_its_own_group() {
        let mut b = Buffer::from_text("x.x");
        type_run(&mut b, 0, "a", 0.0, 0.1); // single-caret InsertText group
        // a two-change (multi-caret) transaction
        let multi = Transaction { changes: vec![
            Change { start: 1, end: 1, text: "Y".into() },
            Change { start: 3, end: 3, text: "Y".into() },
        ] };
        b.record(&multi, meta(EditKind::InsertText, 2, 0.15));
        assert_eq!(b.history.undo.len(), 2, "multi-caret edit never coalesces onto a single-caret group");
        // and a following single-caret edit does not coalesce onto the multi-caret group
        type_run(&mut b, 0, "z", 0.2, 0.1);
        assert_eq!(b.history.undo.len(), 3);
    }

    #[test]
    fn undo_returns_before_snapshot() {
        let mut b = Buffer::from_text("hi");
        let before = SelectionSnapshot { ranges: vec![(2, 2)], primary: 0 };
        let after = SelectionSnapshot { ranges: vec![(7, 7)], primary: 0 };
        b.record(&Transaction::insert(2, " there"), EditMeta { kind: EditKind::Paste, carets: 1, time: 0.0, before: before.clone(), after: after.clone() });
        let restored = b.undo_with(|_, _| {}).unwrap();
        assert_eq!(restored, before);
        let re = b.redo_with(|_, _| {}).unwrap();
        assert_eq!(re, after);
    }
}
