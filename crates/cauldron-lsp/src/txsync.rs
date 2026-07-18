//! Transaction → LSP sync: the incremental `didChange` derivation, the diagnostic range mapper,
//! and the [`WorkspaceEdit`](lsp_types::WorkspaceEdit) → per-file flattener for code actions.
//!
//! THE highest-risk surface in the crate (design RISKS item 2): a wrong range here doesn't
//! error — it silently desyncs the server's copy of the document, and every later diagnostic,
//! hover and completion is subtly wrong forever after. Hence the shadow-replay property test
//! below: randomized multi-change transactions over mixed ASCII/BMP/astral text, replayed
//! event-by-event exactly the way a server applies `contentChanges`, in BOTH position encodings.

use cauldron_editor::buffer::{Change, Transaction};
use cauldron_editor::position::{byte_to_point, byte_to_utf16, Point};
use lsp_types::TextDocumentContentChangeEvent;
use ropey::Rope;

use crate::Encoding;

/// One pre-edit byte offset → an LSP `Position` in the negotiated encoding.
/// utf-8: `character` IS the byte column. utf-16: `character` counts utf-16 code units.
fn to_lsp_pos(pre_rope: &Rope, byte: usize, enc: Encoding) -> lsp_types::Position {
    let Point { line, col } = match enc {
        Encoding::Utf8 => byte_to_point(pre_rope, byte),
        Encoding::Utf16 => byte_to_utf16(pre_rope, byte),
    };
    lsp_types::Position { line: line as u32, character: col as u32 }
}

/// Derive the `contentChanges` for one [`Transaction`]: one event per [`Change`], emitted in
/// DESCENDING start order (the transaction's sorted-ascending changes, iterated in reverse).
///
/// LSP applies `contentChanges` SEQUENTIALLY, each against the document state left by the
/// previous event. Because a higher-offset edit never moves a lower-offset range, every event's
/// PRE-edit range is still valid when its turn comes — so ALL positions are computed against the
/// one pre-edit rope, with zero replay bookkeeping. (`Change` ranges are pre-edit byte offsets,
/// sorted ascending and disjoint — the buffer.rs invariant this leans on.)
pub fn changes_for_tx(
    pre_rope: &Rope,
    tx: &Transaction,
    enc: Encoding,
) -> Vec<TextDocumentContentChangeEvent> {
    debug_assert!(
        tx.changes.windows(2).all(|w| w[0].end <= w[1].start),
        "changes must be sorted + disjoint"
    );
    tx.changes
        .iter()
        .rev()
        .map(|ch| TextDocumentContentChangeEvent {
            range: Some(lsp_types::Range {
                start: to_lsp_pos(pre_rope, ch.start, enc),
                end: to_lsp_pos(pre_rope, ch.end, enc),
            }),
            range_length: None,
            text: ch.text.clone(),
        })
        .collect()
}

/// The Full-sync backstop: a single whole-document event with no range, for servers that
/// negotiate `TextDocumentSyncKind::FULL` (neither clangd nor rust-analyzer does, but the
/// correctness fallback must exist).
pub fn full_text_change(rope: &Rope) -> Vec<TextDocumentContentChangeEvent> {
    vec![TextDocumentContentChangeEvent { range: None, range_length: None, text: rope.to_string() }]
}

/// Flatten a [`lsp_types::WorkspaceEdit`] into per-file edit lists ready for back-to-front
/// application against a rope.
///
/// Both wire shapes are handled and merged: the legacy `changes` map AND `document_changes`
/// (its `Edits` variant directly, its `Operations` variant by keeping the embedded
/// `TextDocumentEdit`s — versioned ids pass through, only the uri is used). Create/Rename/Delete
/// resource operations are SKIPPED with a log line (v1 applies text edits only); annotated
/// edits contribute their inner [`lsp_types::TextEdit`]; non-`file://` uris are skipped.
///
/// Per file the edits come back SORTED DESCENDING by start position, with ties in REVERSE
/// array order — so a caller applying them sequentially (a) never invalidates a
/// still-unapplied range and (b) reproduces the LSP rule that same-position inserts land in
/// array order. Positions stay in the server's negotiated encoding: the caller converts with
/// its own [`Encoding`] knowledge (`cauldron_editor::position`). Files are sorted by path so
/// the output is deterministic. PURE — no I/O, no server state.
pub fn workspace_edit_to_file_edits(
    edit: &lsp_types::WorkspaceEdit,
) -> Vec<(std::path::PathBuf, Vec<lsp_types::TextEdit>)> {
    let mut per_file: Vec<(std::path::PathBuf, Vec<lsp_types::TextEdit>)> = Vec::new();
    let mut push = |uri: &lsp_types::Url, edits: Vec<lsp_types::TextEdit>| {
        let Some(path) = crate::capabilities::uri_to_path(uri) else {
            log::warn!("workspace edit targets non-file uri {uri}, skipped");
            return;
        };
        match per_file.iter_mut().find(|(p, _)| *p == path) {
            Some((_, v)) => v.extend(edits),
            None => per_file.push((path, edits)),
        }
    };

    if let Some(changes) = &edit.changes {
        for (uri, edits) in changes {
            push(uri, edits.clone());
        }
    }
    if let Some(doc_changes) = &edit.document_changes {
        let doc_edits: Vec<&lsp_types::TextDocumentEdit> = match doc_changes {
            lsp_types::DocumentChanges::Edits(edits) => edits.iter().collect(),
            lsp_types::DocumentChanges::Operations(ops) => ops
                .iter()
                .filter_map(|op| match op {
                    lsp_types::DocumentChangeOperation::Edit(e) => Some(e),
                    lsp_types::DocumentChangeOperation::Op(op) => {
                        log::warn!("workspace edit resource op skipped (v1 is text-only): {op:?}");
                        None
                    }
                })
                .collect(),
        };
        for de in doc_edits {
            let edits = de
                .edits
                .iter()
                .map(|e| match e {
                    lsp_types::OneOf::Left(edit) => edit.clone(),
                    lsp_types::OneOf::Right(annotated) => annotated.text_edit.clone(),
                })
                .collect();
            push(&de.text_document.uri, edits);
        }
    }

    // `changes` is a HashMap — sort by path so multi-file output is deterministic.
    per_file.sort_by(|a, b| a.0.cmp(&b.0));
    for (_, edits) in &mut per_file {
        // Stable ascending sort then reverse = descending with ties in reverse array order,
        // exactly what sequential back-to-front application needs (see doc comment).
        edits.sort_by_key(|e| (e.range.start.line, e.range.start.character));
        edits.reverse();
    }
    per_file
}

/// Which side of an exactly-coincident insertion a mapped position sticks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bias {
    /// Stay BEFORE text inserted exactly at the position — used for `range.end`, so an insert
    /// at a squiggle's right edge does not extend the squiggle.
    Before,
    /// Land AFTER text inserted exactly at the position — used for `range.start`, so an insert
    /// at a squiggle's left edge is not absorbed (the squiggle shifts right whole).
    After,
}

/// Map one pre-edit byte offset into post-edit coordinates with the cumulative-delta walk (the
/// same delta arithmetic as buffer.rs `apply_inner`'s inverse construction): every change left
/// of the position shifts it by `text.len() - (end - start)`.
fn map_pos(pos: usize, changes: &[Change], bias: Bias) -> usize {
    let mut delta: isize = 0;
    for ch in changes {
        let ins = ch.text.len() as isize;
        let del = (ch.end - ch.start) as isize;
        if ch.end < pos || (ch.end == pos && (ch.start < ch.end || bias == Bias::After)) {
            // Entirely before `pos`: a deletion ending exactly at `pos` removed text on our
            // left (shifts under both biases), while an insertion exactly at `pos` counts as
            // "before" only under after-bias.
            delta += ins - del;
            continue;
        }
        if ch.start > pos || (ch.start == pos && ch.start == ch.end) {
            // Entirely after `pos` (a before-bias insertion exactly at `pos` lands here too);
            // changes are sorted ascending, so every later change is further right still.
            break;
        }
        // Here ch.start <= pos < ch.end with a real deleted span [start, end).
        if pos == ch.start {
            // Leading boundary: the position survives, glued to the replacement's start.
            return (ch.start as isize + delta) as usize;
        }
        // Strictly inside the deleted span: the position's own text is gone. After-bias lands
        // past the replacement text, before-bias at its start — so a range straddling the span
        // keeps only its surviving side and never claims replacement text it doesn't cover.
        let side = if bias == Bias::After { ins } else { 0 };
        return (ch.start as isize + delta + side) as usize;
    }
    (pos as isize + delta) as usize
}

/// Map a pre-transaction byte range into post-transaction coordinates; `None` when the range
/// collapsed (fell entirely inside a deleted span). Keeps diagnostic squiggles glued to their
/// tokens during the window between an edit and the server's next publish.
///
/// Bias: `range.start` maps with after-bias and `range.end` with before-bias, so an insertion
/// exactly at either edge neither absorbs into nor extends the squiggle — the range shifts or
/// stays put, but never grows over text the server hasn't diagnosed.
pub fn map_range_through_tx(
    range: std::ops::Range<usize>,
    tx: &Transaction,
) -> Option<std::ops::Range<usize>> {
    let start = map_pos(range.start, &tx.changes, Bias::After);
    let end = map_pos(range.end, &tx.changes, Bias::Before);
    if start > end || (start == end && range.start < range.end) {
        // Inverted (an empty range bias-split by an insertion at it) or a non-empty range that
        // collapsed to nothing inside a deletion: the token is gone, drop the squiggle.
        return None;
    }
    Some(start..end)
}

/// One step of a `WorkspaceEdit`, in the order the server listed it.
///
/// Resource operations and text edits are ORDER-SENSITIVE with respect to each other — a
/// "move module to its own file" refactor creates the file, edits it, then edits the original —
/// so unlike [`workspace_edit_to_file_edits`] this preserves sequence instead of grouping by file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceOp {
    /// Text edits for one file, sorted DESCENDING (back-to-front application), same as
    /// [`workspace_edit_to_file_edits`].
    Edit { path: std::path::PathBuf, edits: Vec<lsp_types::TextEdit> },
    Create { path: std::path::PathBuf, overwrite: bool, ignore_if_exists: bool },
    Rename { from: std::path::PathBuf, to: std::path::PathBuf, overwrite: bool, ignore_if_exists: bool },
    Delete { path: std::path::PathBuf, recursive: bool, ignore_if_not_exists: bool },
}

/// Flatten a `WorkspaceEdit` into ordered [`WorkspaceOp`]s, resource operations included.
///
/// This is the move/safe-delete-capable counterpart to [`workspace_edit_to_file_edits`], which
/// drops resource ops and can therefore only express pure-text refactors. Non-`file://` uris are
/// skipped with a log line. The legacy `changes` map has no resource ops and no meaningful order,
/// so its entries are emitted first, sorted by path for determinism.
/// PURE — no I/O, no server state.
pub fn workspace_edit_to_ops(edit: &lsp_types::WorkspaceEdit) -> Vec<WorkspaceOp> {
    let mut ops: Vec<WorkspaceOp> = Vec::new();

    let sort_desc = |mut edits: Vec<lsp_types::TextEdit>| {
        edits.sort_by_key(|e| (e.range.start.line, e.range.start.character));
        edits.reverse();
        edits
    };

    if let Some(changes) = &edit.changes {
        let mut entries: Vec<(std::path::PathBuf, Vec<lsp_types::TextEdit>)> = changes
            .iter()
            .filter_map(|(uri, edits)| {
                crate::capabilities::uri_to_path(uri)
                    .or_else(|| {
                        log::warn!("workspace edit targets non-file uri {uri}, skipped");
                        None
                    })
                    .map(|p| (p, edits.clone()))
            })
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        ops.extend(
            entries.into_iter().map(|(path, edits)| WorkspaceOp::Edit { path, edits: sort_desc(edits) }),
        );
    }

    let Some(doc_changes) = &edit.document_changes else { return ops };
    let mut push_edit = |ops: &mut Vec<WorkspaceOp>, de: &lsp_types::TextDocumentEdit| {
        let Some(path) = crate::capabilities::uri_to_path(&de.text_document.uri) else {
            log::warn!("workspace edit targets non-file uri {}, skipped", de.text_document.uri);
            return;
        };
        let edits = de
            .edits
            .iter()
            .map(|e| match e {
                lsp_types::OneOf::Left(edit) => edit.clone(),
                lsp_types::OneOf::Right(annotated) => annotated.text_edit.clone(),
            })
            .collect();
        ops.push(WorkspaceOp::Edit { path, edits: sort_desc(edits) });
    };

    match doc_changes {
        lsp_types::DocumentChanges::Edits(edits) => {
            for de in edits {
                push_edit(&mut ops, de);
            }
        }
        lsp_types::DocumentChanges::Operations(raw) => {
            for op in raw {
                match op {
                    lsp_types::DocumentChangeOperation::Edit(de) => push_edit(&mut ops, de),
                    lsp_types::DocumentChangeOperation::Op(rop) => match rop {
                        lsp_types::ResourceOp::Create(c) => {
                            if let Some(path) = crate::capabilities::uri_to_path(&c.uri) {
                                let o = c.options.as_ref();
                                ops.push(WorkspaceOp::Create {
                                    path,
                                    overwrite: o.and_then(|o| o.overwrite).unwrap_or(false),
                                    ignore_if_exists: o
                                        .and_then(|o| o.ignore_if_exists)
                                        .unwrap_or(false),
                                });
                            }
                        }
                        lsp_types::ResourceOp::Rename(r) => {
                            match (
                                crate::capabilities::uri_to_path(&r.old_uri),
                                crate::capabilities::uri_to_path(&r.new_uri),
                            ) {
                                (Some(from), Some(to)) => {
                                    let o = r.options.as_ref();
                                    ops.push(WorkspaceOp::Rename {
                                        from,
                                        to,
                                        overwrite: o.and_then(|o| o.overwrite).unwrap_or(false),
                                        ignore_if_exists: o
                                            .and_then(|o| o.ignore_if_exists)
                                            .unwrap_or(false),
                                    });
                                }
                                _ => log::warn!("rename op with non-file uri skipped"),
                            }
                        }
                        lsp_types::ResourceOp::Delete(d) => {
                            if let Some(path) = crate::capabilities::uri_to_path(&d.uri) {
                                let o = d.options.as_ref();
                                ops.push(WorkspaceOp::Delete {
                                    path,
                                    recursive: o.and_then(|o| o.recursive).unwrap_or(false),
                                    ignore_if_not_exists: o
                                        .and_then(|o| o.ignore_if_not_exists)
                                        .unwrap_or(false),
                                });
                            }
                        }
                    },
                }
            }
        }
    }
    ops
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- the shadow-replay property test (design TESTS: the crown jewel) ----------------------
    //
    // For randomized (doc, transaction) pairs: derive the contentChanges, then apply them
    // SEQUENTIALLY to a shadow String the way a server would — resolving each event's range
    // against the CURRENT shadow state in the encoding under test — and assert the shadow ends
    // up byte-identical to the transaction's real result. Both interpreters live here so the
    // production code is never asked to grade its own homework.

    /// Deterministic 64-bit LCG (Knuth MMIX constants), high bits out. No dev-deps.
    struct Lcg(u64);

    impl Lcg {
        fn new(seed: u64) -> Self {
            Self(seed)
        }
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            self.0 >> 33
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// ASCII, BMP 2-byte (é), BMP 3-byte (中), astral 4-byte (𐍈, 🦀), plus newlines and spaces —
    /// every utf-8/utf-16 width class an LSP position can trip over.
    const PALETTE: &[char] = &['a', 'b', 'Z', '0', ' ', 'é', '中', '𐍈', '🦀', '\n'];

    fn gen_text(rng: &mut Lcg, len: usize) -> String {
        (0..len).map(|_| PALETTE[rng.below(PALETTE.len())]).collect()
    }

    /// 1..=8 sorted, disjoint (touching allowed) changes on char boundaries: 2n boundary picks,
    /// sorted, paired. Empty text ≈ 1/3 of the time (deletes), otherwise 1..=5 palette chars
    /// (inserts where start == end, replaces otherwise).
    fn gen_tx(doc: &str, rng: &mut Lcg) -> Transaction {
        let mut boundaries: Vec<usize> = doc.char_indices().map(|(i, _)| i).collect();
        boundaries.push(doc.len());
        let n = 1 + rng.below(8);
        let mut cuts: Vec<usize> =
            (0..n * 2).map(|_| boundaries[rng.below(boundaries.len())]).collect();
        cuts.sort_unstable();
        let changes = cuts
            .chunks(2)
            .map(|pair| {
                let text = if rng.below(3) == 0 {
                    String::new()
                } else {
                    let len = 1 + rng.below(5);
                    gen_text(rng, len)
                };
                Change { start: pair[0], end: pair[1], text }
            })
            .collect();
        Transaction { changes }
    }

    /// Ground truth: apply the transaction back-to-front to a plain String (the same order
    /// buffer.rs `apply_inner` splices, so same-offset inserts compose identically).
    fn apply_tx(doc: &str, tx: &Transaction) -> String {
        let mut s = doc.to_string();
        for ch in tx.changes.iter().rev() {
            s.replace_range(ch.start..ch.end, &ch.text);
        }
        s
    }

    /// Byte offset of every line start (offset 0, plus after each '\n') — the shadow document's
    /// stand-in for ropey's line indexing.
    fn line_starts(s: &str) -> Vec<usize> {
        let mut v = vec![0];
        for (i, b) in s.bytes().enumerate() {
            if b == b'\n' {
                v.push(i + 1);
            }
        }
        v
    }

    /// utf-8 interpreter: `character` is a byte column.
    fn resolve_utf8(s: &str, pos: &lsp_types::Position) -> usize {
        let byte = line_starts(s)[pos.line as usize] + pos.character as usize;
        assert!(s.is_char_boundary(byte), "utf-8 position must land on a char boundary");
        byte
    }

    /// utf-16 interpreter: walk chars from the line start counting utf-16 code units.
    fn resolve_utf16(s: &str, pos: &lsp_types::Position) -> usize {
        let start = line_starts(s)[pos.line as usize];
        let mut units = 0usize;
        let mut byte = start;
        for ch in s[start..].chars() {
            if units >= pos.character as usize {
                break;
            }
            units += ch.len_utf16();
            byte += ch.len_utf8();
        }
        assert_eq!(units, pos.character as usize, "utf-16 position must land on a unit boundary");
        byte
    }

    fn shadow_replay(enc: Encoding, seed: u64, iters: usize) {
        let mut rng = Lcg::new(seed);
        for iter in 0..iters {
            let len = rng.below(61);
            let doc = gen_text(&mut rng, len);
            let tx = gen_tx(&doc, &mut rng);
            let expected = apply_tx(&doc, &tx);

            let pre = Rope::from_str(&doc);
            let events = changes_for_tx(&pre, &tx, enc);
            assert_eq!(events.len(), tx.changes.len(), "one event per change");
            // Descending start order (non-increasing: same-offset inserts may tie).
            for w in events.windows(2) {
                let (a, b) = (w[0].range.unwrap().start, w[1].range.unwrap().start);
                assert!(
                    (a.line, a.character) >= (b.line, b.character),
                    "events must be emitted in descending start order"
                );
            }

            // Sequential application, each range resolved against the CURRENT shadow state —
            // exactly what a conforming server does with a contentChanges batch.
            let mut shadow = doc.clone();
            for ev in &events {
                let range = ev.range.expect("incremental events always carry a range");
                let (sb, eb) = match enc {
                    Encoding::Utf8 => {
                        (resolve_utf8(&shadow, &range.start), resolve_utf8(&shadow, &range.end))
                    }
                    Encoding::Utf16 => {
                        (resolve_utf16(&shadow, &range.start), resolve_utf16(&shadow, &range.end))
                    }
                };
                shadow.replace_range(sb..eb, &ev.text);
            }
            assert_eq!(
                shadow, expected,
                "shadow replay diverged at iter {iter} (enc {enc:?}): doc {doc:?}, tx {tx:?}"
            );
        }
    }

    #[test]
    fn shadow_replay_utf8() {
        shadow_replay(Encoding::Utf8, 0xC0FFEE, 300);
    }

    #[test]
    fn shadow_replay_utf16() {
        shadow_replay(Encoding::Utf16, 0xBADC0DE, 300);
    }

    // ---- changes_for_tx / full_text_change unit cases ------------------------------------------

    #[test]
    fn events_descend_and_carry_pre_edit_positions() {
        // "abc\ndef": replace "b" (1..2) and insert at "e" (5..5) in one transaction.
        let pre = Rope::from_str("abc\ndef");
        let tx = Transaction {
            changes: vec![
                Change { start: 1, end: 2, text: "XY".into() },
                Change { start: 5, end: 5, text: "!".into() },
            ],
        };
        let ev = changes_for_tx(&pre, &tx, Encoding::Utf8);
        assert_eq!(ev.len(), 2);
        // First event = LAST change (descending), positions from the pre-edit rope.
        let r0 = ev[0].range.unwrap();
        assert_eq!((r0.start.line, r0.start.character), (1, 1));
        assert_eq!((r0.end.line, r0.end.character), (1, 1));
        assert_eq!(ev[0].text, "!");
        let r1 = ev[1].range.unwrap();
        assert_eq!((r1.start.line, r1.start.character), (0, 1));
        assert_eq!((r1.end.line, r1.end.character), (0, 2));
        assert_eq!(ev[1].text, "XY");
    }

    #[test]
    fn encoding_decides_the_character_unit() {
        // "𐍈x": the astral char is 4 bytes / 2 utf-16 units; replace the "x" at bytes 4..5.
        let pre = Rope::from_str("𐍈x");
        let tx = Transaction { changes: vec![Change { start: 4, end: 5, text: "y".into() }] };
        let r8 = changes_for_tx(&pre, &tx, Encoding::Utf8)[0].range.unwrap();
        assert_eq!((r8.start.character, r8.end.character), (4, 5)); // byte columns
        let r16 = changes_for_tx(&pre, &tx, Encoding::Utf16)[0].range.unwrap();
        assert_eq!((r16.start.character, r16.end.character), (2, 3)); // utf-16 units
    }

    #[test]
    fn full_text_change_is_one_rangeless_event() {
        let rope = Rope::from_str("hé𐍈llo\nworld");
        let ev = full_text_change(&rope);
        assert_eq!(ev.len(), 1);
        assert!(ev[0].range.is_none());
        assert!(ev[0].range_length.is_none());
        assert_eq!(ev[0].text, "hé𐍈llo\nworld");
    }

    // ---- map_range_through_tx ------------------------------------------------------------------

    fn tx(changes: Vec<Change>) -> Transaction {
        Transaction { changes }
    }

    #[test]
    fn map_shifts_right_past_earlier_insert() {
        let t = tx(vec![Change { start: 2, end: 2, text: "abc".into() }]);
        assert_eq!(map_range_through_tx(5..8, &t), Some(8..11));
    }

    #[test]
    fn map_shifts_left_past_earlier_delete() {
        let t = tx(vec![Change { start: 1, end: 3, text: String::new() }]);
        assert_eq!(map_range_through_tx(5..8, &t), Some(3..6));
    }

    #[test]
    fn map_collapses_inside_deletion() {
        let t = tx(vec![Change { start: 2, end: 5, text: String::new() }]);
        assert_eq!(map_range_through_tx(3..4, &t), None); // strictly inside
        assert_eq!(map_range_through_tx(2..5, &t), None); // exactly the deleted span
    }

    #[test]
    fn insert_at_range_start_is_not_absorbed() {
        // The squiggle shifts right whole; the inserted text stays outside it.
        let t = tx(vec![Change { start: 3, end: 3, text: "XY".into() }]);
        assert_eq!(map_range_through_tx(3..6, &t), Some(5..8));
    }

    #[test]
    fn insert_at_range_end_does_not_extend() {
        let t = tx(vec![Change { start: 6, end: 6, text: "XY".into() }]);
        assert_eq!(map_range_through_tx(3..6, &t), Some(3..6));
    }

    #[test]
    fn multi_change_cumulative_shift() {
        // +3 (insert at 0), -2 (delete 10..12): net +1 for anything past both.
        let t = tx(vec![
            Change { start: 0, end: 0, text: "abc".into() },
            Change { start: 10, end: 12, text: String::new() },
        ]);
        assert_eq!(map_range_through_tx(20..25, &t), Some(21..26));
        // Between the two changes: only the insert's +3 applies.
        assert_eq!(map_range_through_tx(4..7, &t), Some(7..10));
    }

    #[test]
    fn partial_overlap_keeps_the_surviving_side() {
        // Deletion eats the range's tail: the head survives, clipped to the cut point.
        let t = tx(vec![Change { start: 4, end: 9, text: String::new() }]);
        assert_eq!(map_range_through_tx(2..6, &t), Some(2..4));
        // Deletion eats the head: the tail survives, starting at the cut point.
        assert_eq!(map_range_through_tx(6..12, &t), Some(4..7));
    }

    #[test]
    fn untouched_empty_range_survives() {
        let t = tx(vec![Change { start: 10, end: 12, text: String::new() }]);
        assert_eq!(map_range_through_tx(3..3, &t), Some(3..3));
    }

    // ---- workspace_edit_to_file_edits ----------------------------------------------------------
    //
    // Inputs are built from raw JSON so both wire shapes (`changes` map, `document_changes`)
    // are exercised through the exact serde path the reader thread uses.

    use std::path::PathBuf;

    fn we(v: serde_json::Value) -> lsp_types::WorkspaceEdit {
        serde_json::from_value(v).expect("valid WorkspaceEdit JSON")
    }

    fn edit_json(line: u32, ch: u32, end_ch: u32, text: &str) -> serde_json::Value {
        serde_json::json!({
            "range": {"start": {"line": line, "character": ch},
                      "end": {"line": line, "character": end_ch}},
            "newText": text,
        })
    }

    #[test]
    fn flatten_changes_map_sorts_descending() {
        // Server sends the edits ASCENDING; the flattener must hand them back DESCENDING.
        let e = we(serde_json::json!({
            "changes": {"file:///tmp/a.rs": [
                edit_json(0, 0, 0, "use x;\n"),
                edit_json(3, 4, 8, "fixed"),
                edit_json(3, 10, 12, "tail"),
            ]}
        }));
        let files = workspace_edit_to_file_edits(&e);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0, PathBuf::from("/tmp/a.rs"));
        let texts: Vec<&str> = files[0].1.iter().map(|e| e.new_text.as_str()).collect();
        assert_eq!(texts, ["tail", "fixed", "use x;\n"]);
        let starts: Vec<(u32, u32)> =
            files[0].1.iter().map(|e| (e.range.start.line, e.range.start.character)).collect();
        assert_eq!(starts, [(3, 10), (3, 4), (0, 0)]);
    }

    #[test]
    fn flatten_document_changes_edits_variant_with_versions_and_annotations() {
        let e = we(serde_json::json!({
            "documentChanges": [
                {
                    "textDocument": {"uri": "file:///tmp/b.c", "version": 7},
                    "edits": [
                        edit_json(1, 0, 3, "low"),
                        // AnnotatedTextEdit — the inner TextEdit must survive the flatten.
                        {"range": {"start": {"line": 5, "character": 2},
                                   "end": {"line": 5, "character": 2}},
                         "newText": "high", "annotationId": "ann-1"},
                    ]
                }
            ]
        }));
        let files = workspace_edit_to_file_edits(&e);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0, PathBuf::from("/tmp/b.c"));
        let texts: Vec<&str> = files[0].1.iter().map(|e| e.new_text.as_str()).collect();
        assert_eq!(texts, ["high", "low"], "descending by start position");
    }

    #[test]
    fn flatten_operations_variant_keeps_edits_and_skips_resource_ops() {
        let e = we(serde_json::json!({
            "documentChanges": [
                {"kind": "create", "uri": "file:///tmp/new.rs"},
                {
                    "textDocument": {"uri": "file:///tmp/c.rs", "version": null},
                    "edits": [edit_json(0, 0, 2, "kept")]
                },
                {"kind": "rename", "oldUri": "file:///tmp/c.rs", "newUri": "file:///tmp/d.rs"},
                {"kind": "delete", "uri": "file:///tmp/old.rs"},
            ]
        }));
        let files = workspace_edit_to_file_edits(&e);
        assert_eq!(files.len(), 1, "only the text edit survives, ops are skipped");
        assert_eq!(files[0].0, PathBuf::from("/tmp/c.rs"));
        assert_eq!(files[0].1.len(), 1);
        assert_eq!(files[0].1[0].new_text, "kept");
    }

    #[test]
    fn flatten_multi_file_merges_both_shapes_and_orders_by_path() {
        let e = we(serde_json::json!({
            "changes": {
                "file:///tmp/z.rs": [edit_json(0, 0, 1, "z0")],
                "file:///tmp/a.rs": [edit_json(2, 0, 1, "a2")],
            },
            "documentChanges": [
                {
                    "textDocument": {"uri": "file:///tmp/a.rs", "version": 3},
                    "edits": [edit_json(4, 0, 1, "a4")]
                }
            ]
        }));
        let files = workspace_edit_to_file_edits(&e);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].0, PathBuf::from("/tmp/a.rs"));
        assert_eq!(files[1].0, PathBuf::from("/tmp/z.rs"));
        // a.rs got edits from BOTH shapes, merged then sorted descending.
        let texts: Vec<&str> = files[0].1.iter().map(|e| e.new_text.as_str()).collect();
        assert_eq!(texts, ["a4", "a2"]);
        assert_eq!(files[1].1[0].new_text, "z0");
    }

    #[test]
    fn flatten_same_position_inserts_keep_lsp_array_order_under_reverse_application() {
        // LSP: same-position inserts land in array order ("A" then "B"). Applying our output
        // sequentially must reproduce that — so ties come back in REVERSE array order.
        let e = we(serde_json::json!({
            "changes": {"file:///tmp/t.rs": [
                edit_json(0, 5, 5, "A"),
                edit_json(0, 5, 5, "B"),
            ]}
        }));
        let files = workspace_edit_to_file_edits(&e);
        let texts: Vec<&str> = files[0].1.iter().map(|e| e.new_text.as_str()).collect();
        assert_eq!(texts, ["B", "A"]);
        // Shadow-apply to prove the point: "B" inserted at 5, then "A" at 5 → "…AB…".
        let mut s = String::from("0123456789");
        for te in &files[0].1 {
            let at = te.range.start.character as usize;
            s.replace_range(at..at, &te.new_text);
        }
        assert_eq!(s, "01234AB56789");
    }

    #[test]
    fn ops_preserve_resource_op_order_relative_to_edits() {
        // The "move item to its own file" shape: create the new file, edit it, then edit the
        // original. Order is load-bearing — editing before creating writes into nothing.
        let e = we(serde_json::json!({
            "documentChanges": [
                {"kind": "create", "uri": "file:///tmp/new.rs"},
                {
                    "textDocument": {"uri": "file:///tmp/new.rs", "version": null},
                    "edits": [edit_json(0, 0, 0, "fn moved() {}")]
                },
                {
                    "textDocument": {"uri": "file:///tmp/old.rs", "version": 7},
                    "edits": [edit_json(2, 0, 40, "")]
                }
            ]
        }));
        let ops = workspace_edit_to_ops(&e);
        assert_eq!(ops.len(), 3);
        assert_eq!(
            ops[0],
            WorkspaceOp::Create {
                path: PathBuf::from("/tmp/new.rs"),
                overwrite: false,
                ignore_if_exists: false,
            }
        );
        match &ops[1] {
            WorkspaceOp::Edit { path, edits } => {
                assert_eq!(path, &PathBuf::from("/tmp/new.rs"));
                assert_eq!(edits[0].new_text, "fn moved() {}");
            }
            other => panic!("expected edit to new.rs, got {other:?}"),
        }
        match &ops[2] {
            WorkspaceOp::Edit { path, .. } => assert_eq!(path, &PathBuf::from("/tmp/old.rs")),
            other => panic!("expected edit to old.rs, got {other:?}"),
        }
    }

    #[test]
    fn ops_decode_rename_and_delete_options() {
        let e = we(serde_json::json!({
            "documentChanges": [
                {
                    "kind": "rename",
                    "oldUri": "file:///tmp/a.rs",
                    "newUri": "file:///tmp/b.rs",
                    "options": {"overwrite": true}
                },
                {
                    "kind": "delete",
                    "uri": "file:///tmp/gone.rs",
                    "options": {"recursive": true, "ignoreIfNotExists": true}
                }
            ]
        }));
        let ops = workspace_edit_to_ops(&e);
        assert_eq!(
            ops,
            vec![
                WorkspaceOp::Rename {
                    from: PathBuf::from("/tmp/a.rs"),
                    to: PathBuf::from("/tmp/b.rs"),
                    overwrite: true,
                    ignore_if_exists: false,
                },
                WorkspaceOp::Delete {
                    path: PathBuf::from("/tmp/gone.rs"),
                    recursive: true,
                    ignore_if_not_exists: true,
                },
            ]
        );
    }

    #[test]
    fn ops_sort_edits_descending_like_the_legacy_flattener() {
        // Same back-to-front guarantee: a caller applying sequentially must not invalidate a
        // range it has not reached yet.
        let e = we(serde_json::json!({
            "changes": {"file:///tmp/t.rs": [
                edit_json(0, 1, 2, "early"),
                edit_json(9, 1, 2, "late"),
            ]}
        }));
        let ops = workspace_edit_to_ops(&e);
        match &ops[0] {
            WorkspaceOp::Edit { edits, .. } => {
                let texts: Vec<&str> = edits.iter().map(|e| e.new_text.as_str()).collect();
                assert_eq!(texts, ["late", "early"]);
            }
            other => panic!("expected edit, got {other:?}"),
        }
    }

    #[test]
    fn ops_skip_non_file_uris() {
        let e = we(serde_json::json!({
            "documentChanges": [
                {"kind": "create", "uri": "untitled:Untitled-1"},
                {"kind": "rename", "oldUri": "file:///tmp/a.rs", "newUri": "untitled:x"}
            ]
        }));
        assert!(workspace_edit_to_ops(&e).is_empty());
        assert!(workspace_edit_to_ops(&lsp_types::WorkspaceEdit::default()).is_empty());
    }

    #[test]
    fn flatten_skips_non_file_uris_and_empty_edit() {
        let e = we(serde_json::json!({
            "changes": {"untitled:Untitled-1": [edit_json(0, 0, 0, "x")]}
        }));
        assert!(workspace_edit_to_file_edits(&e).is_empty());
        assert!(workspace_edit_to_file_edits(&lsp_types::WorkspaceEdit::default()).is_empty());
    }
}
