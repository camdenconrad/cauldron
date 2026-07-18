//! Incremental invalidation (docs/psi-design.md, "Indexing pipeline"): ONE chokepoint,
//! [`replace_file_facts`], keyed on the two hashes the retained [`Index`] stores per file:
//!
//! - **both hashes equal AND facts positionally identical** — no-op fast path: the index is
//!   untouched and the generation is NOT bumped (nothing the index can see changed);
//! - **both hashes equal but positions moved** ([`Invalidation::Moved`]) — the hashes exclude
//!   offsets/name lines by design, so a comment-only edit shifts every retained position while
//!   leaving them intact; the fresh facts are installed (stale positions would otherwise be
//!   pinned forever) and position-derived layers must be recomputed;
//! - **body-only change** (`interface_hash` equal, `body_hash` differs) — retract the file's old
//!   contributions (the old facts are the exact retraction key set) and fold the new facts in;
//!   other files' facts are never re-extracted. Derived layers (call graph, Rule-1 findings) are
//!   rebuilt IN FULL from retained facts by the caller — "incremental facts, full graph
//!   rebuild". Incremental graph PATCHING is deliberately absent: a stale edge produces phantom
//!   recursion findings, which is worse than a ms-scale rebuild;
//! - **interface change** — same mechanics in v1 (the full derived rebuild re-resolves every
//!   dependent automatically), but reported distinctly so smarter dependent tracking can slot in
//!   later without an API change;
//! - **unknown path** — added to the index (stable-interned, gets a fresh FileId).
//!
//! Every real mutation bumps [`Index::generation`], so snapshots derived from the index are
//! stamped and stale answers can be dropped by generation compare.
//!
//! Item 7 adds the two overlay-lane siblings: [`overlay_file_facts`] (like
//! [`replace_file_facts`] but ALWAYS installs the new facts — dirty-buffer offsets must stay
//! fresh for live squiggles even when the hashes match, because `body_hash` deliberately
//! excludes offsets) and [`remove_file_facts`] (full retraction — restoring "not in the index"
//! when an overlay-only file's buffer closes without save; also the future delete lane).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::collect::FileFacts;
use crate::index::Index;

/// What [`replace_file_facts`] found, in increasing order of blast radius.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Invalidation {
    /// Facts identical INCLUDING positions — the index was not touched, the generation not
    /// bumped.
    Unchanged,
    /// Both hashes equal but the retained facts differ positionally (offsets / name lines
    /// shifted by a comment-only or whitespace edit): the fresh facts were installed and the
    /// generation bumped. Derived SEMANTICS (call graph shape) did not move, but anything that
    /// resolves positions to lines (witness rows, goto-symbol, find-usages) must be rebuilt —
    /// [`Invalidation::changed`] is true.
    Moved,
    /// Same exported surface, different body facts: the file's own facts were swapped in place;
    /// derived layers must be rebuilt, other files' facts are untouched.
    BodyOnly,
    /// The exported surface changed: dependents' resolution may shift. v1 mechanics equal
    /// [`Invalidation::BodyOnly`] — the on-demand graph rebuild re-resolves everyone — but the
    /// distinction is kept in the API for finer-grained invalidation later.
    Interface,
    /// The path was not in the index yet; its facts were added under a fresh FileId.
    Added,
}

impl Invalidation {
    /// Did this mutation change the index (i.e. must derived layers be rebuilt)?
    pub fn changed(self) -> bool {
        !matches!(self, Invalidation::Unchanged)
    }
}

/// THE invalidation chokepoint: make `path`'s contribution to `index` be exactly `facts`.
/// Retract-by-forward-diff + re-fold; FileIds are stable across replaces. Callers rebuild
/// derived artifacts (via [`Index::call_graph`] / [`crate::project::rule1_findings`]) whenever
/// the returned outcome [`Invalidation::changed`].
pub fn replace_file_facts(index: &mut Index, path: PathBuf, facts: Arc<FileFacts>) -> Invalidation {
    let Some(fid) = index.file_id(&path) else {
        index.add_file(path, Arc::clone(&facts));
        index.normalize_order_for(&facts);
        index.bump_generation();
        return Invalidation::Added;
    };
    let old = index.hashes(fid);
    if old == Some((facts.interface_hash, facts.body_hash)) {
        // Hash-equal is only a true no-op when the retained facts match POSITIONALLY too: the
        // two hashes deliberately exclude offsets and name lines, so a comment-only edit above
        // code shifts every retained position while leaving both hashes intact. Keeping the
        // old facts would pin witness lines / goto-symbol rows / find-usages to stale
        // coordinates FOREVER (the hashes never change again, so no later save heals it).
        if index.facts(fid).is_some_and(|old_facts| **old_facts == *facts) {
            return Invalidation::Unchanged;
        }
        index.retract_file(fid);
        index.add_file(path, Arc::clone(&facts));
        index.normalize_order_for(&facts);
        index.bump_generation();
        return Invalidation::Moved;
    }
    let outcome = match old {
        // Interned path whose facts were never (re-)folded — treat as an add.
        None => Invalidation::Added,
        Some((oi, _)) if oi == facts.interface_hash => Invalidation::BodyOnly,
        Some(_) => Invalidation::Interface,
    };
    index.retract_file(fid);
    index.add_file(path, Arc::clone(&facts));
    index.normalize_order_for(&facts);
    index.bump_generation();
    outcome
}

/// The DIRTY-BUFFER variant of [`replace_file_facts`]: identical mechanics, but the hash-equal
/// fast path still swaps the facts in (and bumps the generation). Buffer-derived facts carry
/// buffer-coordinate offsets; a comment-only edit shifts every offset while leaving both hashes
/// untouched, so the fast path would pin squiggles/witnesses to stale positions exactly while
/// the user is watching them. The returned classification is unchanged ([`Invalidation::Unchanged`]
/// here means "hashes equal — derived SEMANTICS did not move", but the facts were installed).
pub fn overlay_file_facts(index: &mut Index, path: PathBuf, facts: Arc<FileFacts>) -> Invalidation {
    let Some(fid) = index.file_id(&path) else {
        index.add_file(path, Arc::clone(&facts));
        index.normalize_order_for(&facts);
        index.bump_generation();
        return Invalidation::Added;
    };
    let outcome = match index.hashes(fid) {
        None => Invalidation::Added,
        Some((oi, ob)) if (oi, ob) == (facts.interface_hash, facts.body_hash) => {
            Invalidation::Unchanged
        }
        Some((oi, _)) if oi == facts.interface_hash => Invalidation::BodyOnly,
        Some(_) => Invalidation::Interface,
    };
    index.retract_file(fid);
    index.add_file(path, Arc::clone(&facts));
    index.normalize_order_for(&facts);
    index.bump_generation();
    outcome
}

/// Retract `path`'s facts entirely — the index answers as if the file does not exist. Used when
/// a buffer whose overlay ADDED the file closes without save (nothing on disk to restore), and
/// ready for the watcher's future delete lane. The interned FileId survives (item 3's stability
/// contract) and is reused if the path comes back. Returns false when the path contributed
/// nothing (unknown or already retracted) — the index and generation are untouched.
pub fn remove_file_facts(index: &mut Index, path: &Path) -> bool {
    let Some(fid) = index.file_id(path) else { return false };
    if index.retract_file(fid).is_none() {
        return false;
    }
    index.bump_generation();
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect;
    use crate::graph::FileId;
    use std::path::Path;

    const A0: &str = "void g(void);\nvoid f(void) { g(); }\n";
    const B0: &str = "void f(void);\nvoid g(void) { f(); }\n";

    fn facts(text: &str) -> Arc<FileFacts> {
        Arc::new(collect::file_facts(text))
    }

    /// a.c defines f (calls g), b.c defines g (calls f) + declares f: one cross-file cycle.
    fn fixture() -> Index {
        Index::build([
            (PathBuf::from("/proj/a.c"), facts(A0)),
            (PathBuf::from("/proj/b.c"), facts(B0)),
        ])
    }

    fn cycles(idx: &Index) -> usize {
        idx.call_graph().tier1_findings().len()
    }

    #[test]
    fn noop_on_identical_content() {
        let mut idx = fixture();
        let a = idx.file_id(Path::new("/proj/a.c")).unwrap();
        let before = Arc::clone(idx.facts(a).unwrap());
        let gen = idx.generation();
        let out = replace_file_facts(&mut idx, "/proj/a.c".into(), facts(A0));
        assert_eq!(out, Invalidation::Unchanged);
        assert!(!out.changed());
        assert_eq!(idx.generation(), gen, "no-op must not bump the generation");
        assert!(Arc::ptr_eq(idx.facts(a).unwrap(), &before), "facts left untouched");
        assert_eq!(idx.callers_of("g").len(), 1, "inverted maps untouched");
        assert_eq!(cycles(&idx), 1, "f<->g still found");
    }

    #[test]
    fn body_edit_swaps_one_file_without_reextracting_others() {
        let mut idx = fixture();
        let a = idx.file_id(Path::new("/proj/a.c")).unwrap();
        let b = idx.file_id(Path::new("/proj/b.c")).unwrap();
        let b_before = Arc::clone(idx.facts(b).unwrap());
        let gen = idx.generation();
        assert_eq!(cycles(&idx), 1, "fixture starts with the cycle");
        // Same interface (g decl + f def), but the body no longer calls g.
        let new = facts("void g(void);\nvoid f(void) { }\n");
        assert_eq!(idx.hashes(a).unwrap().0, new.interface_hash, "edit is body-only");
        let out = replace_file_facts(&mut idx, "/proj/a.c".into(), Arc::clone(&new));
        assert_eq!(out, Invalidation::BodyOnly);
        assert_eq!(idx.generation(), gen + 1, "one mutation, one bump");
        // The invalidation's whole point: b.c was NOT re-extracted (same allocation),
        // a.c's facts were swapped in place.
        assert!(Arc::ptr_eq(idx.facts(b).unwrap(), &b_before));
        assert!(Arc::ptr_eq(idx.facts(a).unwrap(), &new));
        // Findings rebuilt from retained facts reflect the edit.
        assert_eq!(cycles(&idx), 0, "cycle broken by the body edit");
        assert!(idx.callers_of("g").is_empty(), "a.c's call edge retracted");
        assert_eq!(idx.callers_of("f").len(), 1, "b.c's call site untouched");
    }

    #[test]
    fn interface_edit_updates_dependents_resolution() {
        let mut idx = fixture();
        let a = idx.file_id(Path::new("/proj/a.c")).unwrap();
        let a_before = Arc::clone(idx.facts(a).unwrap());
        // b.c stops exporting g (renamed to h): a.c's f() -> g() must re-resolve to an extern
        // leaf, killing the cycle — WITHOUT re-extracting the dependent a.c.
        let out = replace_file_facts(&mut idx, "/proj/b.c".into(), facts("void h(void) { }\n"));
        assert_eq!(out, Invalidation::Interface);
        assert!(Arc::ptr_eq(idx.facts(a).unwrap(), &a_before), "dependent not re-extracted");
        assert!(idx.defs_by_name("g").is_empty(), "old surface retracted");
        assert!(idx.decls_by_name("f").is_empty(), "b.c's f prototype retracted");
        assert_eq!(idx.defs_by_name("h").len(), 1, "new surface folded");
        assert_eq!(cycles(&idx), 0, "dependent's call re-resolved: no cycle without g's def");
    }

    #[test]
    fn unknown_path_is_added_with_fresh_fileid() {
        let mut idx = fixture();
        let gen = idx.generation();
        let out = replace_file_facts(
            &mut idx,
            "/proj/c.c".into(),
            facts("void f(void);\nvoid q(void) { f(); }\n"),
        );
        assert_eq!(out, Invalidation::Added);
        assert_eq!(idx.generation(), gen + 1);
        assert_eq!(idx.file_count(), 3);
        assert_eq!(idx.file_id(Path::new("/proj/c.c")), Some(FileId(2)));
        assert_eq!(idx.defs_by_name("q").len(), 1);
        assert_eq!(idx.callers_of("f").len(), 2, "b.c's and c.c's calls");
    }

    /// Issue #2 review: an offset-only disk change (comment typed above code, then saved) hits
    /// the hash-equal path but must NOT keep the old facts — that would pin witness lines and
    /// goto-symbol rows to stale positions permanently. The fresh facts install as `Moved`
    /// (changed() = derived position layers rebuild); a truly identical replace stays the
    /// generation-free `Unchanged` no-op.
    #[test]
    fn save_lane_installs_moved_offsets_when_hashes_match() {
        let mut idx = fixture();
        let a = idx.file_id(Path::new("/proj/a.c")).unwrap();
        let gen = idx.generation();
        // Comment above the code: every offset/name_line shifts, both hashes stay identical.
        let shifted = facts("// comment-only edit\nvoid g(void);\nvoid f(void) { g(); }\n");
        assert_eq!(idx.hashes(a).unwrap(), (shifted.interface_hash, shifted.body_hash));
        let out = replace_file_facts(&mut idx, "/proj/a.c".into(), Arc::clone(&shifted));
        assert_eq!(out, Invalidation::Moved);
        assert!(out.changed(), "position layers must rebuild");
        assert!(Arc::ptr_eq(idx.facts(a).unwrap(), &shifted), "fresh offsets installed");
        assert_eq!(idx.generation(), gen + 1, "a real mutation happened: positions moved");
        assert_eq!(idx.callers_of("g").len(), 1, "no duplicate fold");
        assert_eq!(cycles(&idx), 1, "semantics unchanged");
        // The stub's recorded line reflects the new text.
        let f_stub = idx.facts(a).unwrap().stubs.iter().find(|s| s.name == "f").unwrap();
        assert_eq!(f_stub.name_line, 2, "name_line follows the shifted text");
        // Replaying the exact same facts again is a true no-op.
        let out = replace_file_facts(&mut idx, "/proj/a.c".into(), Arc::clone(&shifted));
        assert_eq!(out, Invalidation::Unchanged);
        assert_eq!(idx.generation(), gen + 1, "identical replace must not bump");
    }

    #[test]
    fn overlay_always_installs_fresh_facts_even_when_hashes_match() {
        let mut idx = fixture();
        let a = idx.file_id(Path::new("/proj/a.c")).unwrap();
        let gen = idx.generation();
        // Comment above the code: every offset shifts, both hashes stay identical.
        let shifted = facts("// dirty buffer\nvoid g(void);\nvoid f(void) { g(); }\n");
        assert_eq!(idx.hashes(a).unwrap(), (shifted.interface_hash, shifted.body_hash));
        let out = overlay_file_facts(&mut idx, "/proj/a.c".into(), Arc::clone(&shifted));
        assert_eq!(out, Invalidation::Unchanged, "classification still reports hash-equal");
        assert!(Arc::ptr_eq(idx.facts(a).unwrap(), &shifted), "facts swapped anyway");
        assert_eq!(idx.generation(), gen + 1, "a real mutation happened: offsets moved");
        assert_eq!(idx.callers_of("g").len(), 1, "no duplicate fold");
        assert_eq!(cycles(&idx), 1, "semantics unchanged");
    }

    #[test]
    fn remove_retracts_everything_and_keeps_the_fileid() {
        let mut idx = fixture();
        let a = idx.file_id(Path::new("/proj/a.c")).unwrap();
        let gen = idx.generation();
        assert!(remove_file_facts(&mut idx, Path::new("/proj/a.c")));
        assert_eq!(idx.generation(), gen + 1);
        assert_eq!(idx.file_count(), 1);
        assert!(idx.facts(a).is_none(), "forward facts gone");
        assert!(idx.defs_by_name("f").is_empty(), "surface retracted");
        assert!(idx.callers_of("g").is_empty(), "call edges retracted");
        assert_eq!(cycles(&idx), 0, "cycle can't survive one member's removal");
        // Second removal / unknown path: no-op, no generation churn.
        assert!(!remove_file_facts(&mut idx, Path::new("/proj/a.c")));
        assert!(!remove_file_facts(&mut idx, Path::new("/proj/nope.c")));
        assert_eq!(idx.generation(), gen + 1);
        // The path keeps its FileId when it comes back (stability contract).
        let out = replace_file_facts(&mut idx, "/proj/a.c".into(), facts(A0));
        assert_eq!(out, Invalidation::Added);
        assert_eq!(idx.file_id(Path::new("/proj/a.c")), Some(a));
        assert_eq!(cycles(&idx), 1, "restored facts close the cycle again");
    }

    #[test]
    fn replace_keeps_fileid_and_query_order_stable() {
        let mut idx = fixture();
        let a = idx.file_id(Path::new("/proj/a.c")).unwrap();
        // Replace file 0 twice; it re-folds AFTER file 1 each time, so ordering must be
        // normalized back and repeated retract/insert must not duplicate or leak entries.
        let two_calls = facts("void g(void);\nvoid f(void) { g(); g(); }\n");
        assert_eq!(
            replace_file_facts(&mut idx, "/proj/a.c".into(), two_calls),
            Invalidation::BodyOnly
        );
        assert_eq!(idx.callers_of("g").len(), 2, "both call sites materialized");
        assert_eq!(replace_file_facts(&mut idx, "/proj/a.c".into(), facts(A0)), Invalidation::BodyOnly);
        assert_eq!(idx.file_id(Path::new("/proj/a.c")), Some(a), "FileId stable across replaces");
        assert_eq!(idx.callers_of("g").len(), 1, "no duplicate folds after two replaces");
        // Documented orderings survive the tail re-fold.
        assert_eq!(idx.files_with_ident("f"), &[FileId(0), FileId(1)]);
        assert_eq!(idx.files_with_ident("g"), &[FileId(0), FileId(1)]);
        assert_eq!(idx.defs_by_name("f")[0].file, FileId(0));
        assert_eq!(cycles(&idx), 1, "back to the original cycle");
        assert_eq!(idx.generation(), 2, "two real mutations");
    }
}
