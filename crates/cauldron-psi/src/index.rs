//! The retained PSI index (docs/psi-design.md, "CORE TYPES" / `Index`): everything a project
//! scan learns, kept alive as a queryable artifact instead of being dropped after the Rule-1
//! pass. One [`Index`] holds:
//!
//! - a STABLE path <-> [`FileId`] table (a path keeps its id for the index's lifetime — item 4's
//!   incremental invalidation replaces a file's facts in place, never renumbers);
//! - the forward index `files: HashMap<FileId, Arc<FileFacts>>` — simultaneously the per-file
//!   cache and the retraction key set for item 4 (the old facts list exactly what the file
//!   contributed), including each file's `interface_hash` / `body_hash`;
//! - the retained name [`Interner`] shared by all inverted maps;
//! - inverted maps: `defs_by_name` / `decls_by_name` (definition/declaration stubs by name),
//!   `callers_of` (materialized call sites by CALLEE name — the one thing JetBrains doesn't
//!   materialize, cheap and right at cFS scale), and `files_with_ident` (the IdIndex analog,
//!   phase-1 of find-usages).
//!
//! `files_with_ident` is derived from what [`crate::collect`] already extracts — stub names,
//! call callees, address-taken names — not a full identifier harvest of every token; widening it
//! to comment/string contexts is collect.rs work (ContextMask) deferred with find-usages phase 2.
//!
//! The whole-program [`CallGraph`] stays a DERIVED artifact: [`Index::call_graph`] builds it on
//! demand (ms-scale; never incrementally maintained).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::collect::{CallSite, FileFacts, Stub, StubKind};
use crate::graph::{CallGraph, FileId, Interner, Sym};

/// One definition or declaration site: `stub` indexes into the owning file's
/// [`FileFacts::stubs`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DefRef {
    pub file: FileId,
    pub stub: u32,
}

/// The retained project index. Build once per scan via [`Index::build`]; queries are cheap map
/// lookups. Incremental invalidation goes through [`crate::invalidate::replace_file_facts`] —
/// the retract/insert chokepoint on top of [`Index::add_file`]'s fold. Clone exists for
/// copy-on-write snapshotting (`Arc::make_mut`): per-file facts are shared `Arc`s, so a clone
/// duplicates only the tables.
#[derive(Default, Clone)]
pub struct Index {
    /// FileId -> absolute path; ids are dense (`0..paths.len()`) and never renumbered.
    paths: Vec<PathBuf>,
    id_of: HashMap<PathBuf, FileId>,
    /// Forward index: the ONE per-file artifact, hashes included.
    files: HashMap<FileId, Arc<FileFacts>>,
    /// Retained name interner backing every inverted map.
    syms: Interner,
    defs_by_name: HashMap<Sym, Vec<DefRef>>,
    decls_by_name: HashMap<Sym, Vec<DefRef>>,
    /// Tag/typedef name -> its definitions. SEPARATE from `defs_by_name` on purpose: C keeps
    /// tags in their own namespace (`struct stat` and `stat()` coexist), and Change Signature
    /// resolves its anchor through `defs_by_name` and errors on a non-function.
    types_by_name: HashMap<Sym, Vec<DefRef>>,
    /// Field / enumerator name -> every declaration of it, across all aggregates. Same name in
    /// two structs yields two entries; the caller disambiguates by `Stub::parent`.
    members_by_name: HashMap<Sym, Vec<DefRef>>,
    /// File-scope variable name -> its definitions and `extern` declarations.
    globals_by_name: HashMap<Sym, Vec<DefRef>>,
    /// Callee name -> every call site targeting it (materialized inverse of `FileFacts::calls`).
    callers_of: HashMap<Sym, Vec<(FileId, CallSite)>>,
    /// Ident name -> files mentioning it (defs, decls, macros, typedefs, callees,
    /// address-taken). Deduped per file, ascending FileId.
    files_with_ident: HashMap<Sym, Vec<FileId>>,
    /// Bumped by item 4's `replace_file_facts`; queries stamp results with it so stale answers
    /// can be dropped by generation compare.
    generation: u64,
}

impl Index {
    /// Build an index from `(path, facts)` pairs. FileIds are assigned in iteration order, so a
    /// sorted input (the scan pipeline's kept list) yields deterministic ids across runs.
    pub fn build(entries: impl IntoIterator<Item = (PathBuf, Arc<FileFacts>)>) -> Index {
        let mut idx = Index::default();
        for (path, facts) in entries {
            idx.add_file(path, facts);
        }
        idx
    }

    /// Intern `path` (stable: an existing path keeps its FileId) and fold `facts` into the
    /// forward index + inverted maps. This is the single insert chokepoint; item 4's
    /// `replace_file_facts` will pair it with retract-by-forward-diff.
    pub fn add_file(&mut self, path: PathBuf, facts: Arc<FileFacts>) -> FileId {
        let fid = match self.id_of.get(&path) {
            Some(&fid) => fid,
            None => {
                let fid = FileId(self.paths.len() as u32);
                self.paths.push(path.clone());
                self.id_of.insert(path, fid);
                fid
            }
        };
        debug_assert!(
            !self.files.contains_key(&fid),
            "re-adding a file without retraction double-folds its contributions (item 4)"
        );
        for (si, stub) in facts.stubs.iter().enumerate() {
            let sym = self.syms.intern(&stub.name);
            let dref = DefRef { file: fid, stub: si as u32 };
            match stub.kind {
                StubKind::FnDef | StubKind::MacroFn | StubKind::MacroObj => {
                    self.defs_by_name.entry(sym).or_default().push(dref);
                }
                StubKind::FnDecl => {
                    self.decls_by_name.entry(sym).or_default().push(dref);
                }
                // Types get their OWN map. Folding them into `defs_by_name` would break Change
                // Signature, which takes the first resolvable def/decl as its anchor and hard-
                // errors when it is not a function — and C tags live in a separate namespace, so
                // `struct stat` and a `stat()` function legitimately share a name.
                StubKind::Struct
                | StubKind::Union
                | StubKind::Enum
                | StubKind::TagDecl
                | StubKind::Typedef => {
                    self.types_by_name.entry(sym).or_default().push(dref);
                }
                // Members are reachable through their parent aggregate, and by name for
                // find-usages; they are never top-level definitions.
                StubKind::Field | StubKind::Enumerator => {
                    self.members_by_name.entry(sym).or_default().push(dref);
                }
                StubKind::Global | StubKind::GlobalDecl => {
                    self.globals_by_name.entry(sym).or_default().push(dref);
                }
            }
            push_ident(&mut self.files_with_ident, sym, fid);
        }
        for call in &facts.calls {
            let sym = self.syms.intern(&call.callee);
            self.callers_of.entry(sym).or_default().push((fid, call.clone()));
            push_ident(&mut self.files_with_ident, sym, fid);
        }
        for (name, _ctx) in &facts.address_taken {
            let sym = self.syms.intern(name);
            push_ident(&mut self.files_with_ident, sym, fid);
        }
        self.files.insert(fid, facts);
        fid
    }

    // ---- Path table ---------------------------------------------------------------------------

    pub fn file_id(&self, path: &Path) -> Option<FileId> {
        self.id_of.get(path).copied()
    }

    pub fn path(&self, fid: FileId) -> Option<&Path> {
        self.paths.get(fid.0 as usize).map(PathBuf::as_path)
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Iterate every indexed file as `(FileId, path, facts)`, ascending FileId (ids are dense,
    /// so walking the path table gives a deterministic order). Retracted files (facts dropped,
    /// FileId parked per the stability contract) are skipped. Consumers deriving whole-project
    /// views — goto-symbol's C tier enumerates all stubs through this — pay one pass, no clones.
    pub fn files(&self) -> impl Iterator<Item = (FileId, &Path, &Arc<FileFacts>)> + '_ {
        (0..self.paths.len() as u32).map(FileId).filter_map(move |fid| {
            Some((fid, self.paths[fid.0 as usize].as_path(), self.files.get(&fid)?))
        })
    }

    // ---- Forward index ------------------------------------------------------------------------

    pub fn facts(&self, fid: FileId) -> Option<&Arc<FileFacts>> {
        self.files.get(&fid)
    }

    /// The two invalidation keys (item 4): `(interface_hash, body_hash)` of `fid`'s facts.
    pub fn hashes(&self, fid: FileId) -> Option<(u64, u64)> {
        self.files.get(&fid).map(|f| (f.interface_hash, f.body_hash))
    }

    /// Resolve a [`DefRef`] back to its stub (plain data — navigation re-opens the file at the
    /// stub's spans).
    pub fn stub(&self, dref: DefRef) -> Option<&Stub> {
        self.files.get(&dref.file)?.stubs.get(dref.stub as usize)
    }

    // ---- Inverted maps ------------------------------------------------------------------------

    /// Definition sites (functions + macros) named `name`, in FileId/stub order.
    pub fn defs_by_name(&self, name: &str) -> &[DefRef] {
        self.lookup(name).and_then(|s| self.defs_by_name.get(&s)).map_or(&[], Vec::as_slice)
    }

    /// Type (struct/union/enum/typedef) definitions named `name`.
    pub fn types_by_name(&self, name: &str) -> &[DefRef] {
        self.lookup(name).and_then(|s| self.types_by_name.get(&s)).map_or(&[], Vec::as_slice)
    }

    /// Field / enumerator declarations named `name`, across every aggregate that has one.
    pub fn members_by_name(&self, name: &str) -> &[DefRef] {
        self.lookup(name).and_then(|s| self.members_by_name.get(&s)).map_or(&[], Vec::as_slice)
    }

    /// File-scope variable definitions and `extern` declarations named `name`.
    pub fn globals_by_name(&self, name: &str) -> &[DefRef] {
        self.lookup(name).and_then(|s| self.globals_by_name.get(&s)).map_or(&[], Vec::as_slice)
    }

    /// Declaration (prototype) sites named `name`.
    pub fn decls_by_name(&self, name: &str) -> &[DefRef] {
        self.lookup(name).and_then(|s| self.decls_by_name.get(&s)).map_or(&[], Vec::as_slice)
    }

    /// Every call site whose CALLEE is `name` (direct + macro-mined), with the calling file.
    pub fn callers_of(&self, name: &str) -> &[(FileId, CallSite)] {
        self.lookup(name).and_then(|s| self.callers_of.get(&s)).map_or(&[], Vec::as_slice)
    }

    /// Files mentioning `name` at all — phase 1 of two-phase find-usages: a small candidate set
    /// the caller then re-parses precisely (phase 2).
    pub fn files_with_ident(&self, name: &str) -> &[FileId] {
        self.lookup(name).and_then(|s| self.files_with_ident.get(&s)).map_or(&[], Vec::as_slice)
    }

    fn lookup(&self, name: &str) -> Option<Sym> {
        self.syms.get(name)
    }

    // ---- Derived artifacts --------------------------------------------------------------------

    /// Build the whole-program call graph from the retained facts. Derived on demand, never
    /// incrementally maintained (full rebuild is ms-scale at cFS size); Rule-1 findings are a
    /// query over this.
    pub fn call_graph(&self) -> CallGraph {
        let mut files: Vec<(FileId, Arc<FileFacts>)> =
            self.files.iter().map(|(&fid, f)| (fid, Arc::clone(f))).collect();
        files.sort_by_key(|&(fid, _)| fid);
        let paths: Vec<String> =
            self.paths.iter().map(|p| p.to_string_lossy().into_owned()).collect();
        CallGraph::build(&files, &paths)
    }

    /// Generation this index (and anything derived from it) was computed at.
    /// [`crate::invalidate::replace_file_facts`] bumps it on every real mutation, so snapshots
    /// are stamped and stale answers can be dropped by generation compare.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    // ---- Mutation support (crate::invalidate) ---------------------------------------------------

    /// Remove every contribution `fid` folded into the inverted maps and drop its forward facts.
    /// The old facts ARE the retraction key set — only syms they touched are visited, never the
    /// whole maps. The path-table entry is kept: a file keeps its FileId for the index's
    /// lifetime (item 3's stability contract).
    pub(crate) fn retract_file(&mut self, fid: FileId) -> Option<Arc<FileFacts>> {
        let old = self.files.remove(&fid)?;
        for sym in self.touched_syms(&old) {
            retain_or_remove(&mut self.defs_by_name, sym, |d| d.file != fid);
            retain_or_remove(&mut self.decls_by_name, sym, |d| d.file != fid);
            retain_or_remove(&mut self.callers_of, sym, |(f, _)| *f != fid);
            retain_or_remove(&mut self.files_with_ident, sym, |f| *f != fid);
        }
        Some(old)
    }

    /// Restore the documented query orderings (DefRefs in FileId/stub order, call sites in
    /// FileId/offset order, `files_with_ident` ascending) for the syms `facts` touched — a
    /// replaced file's re-fold appends at the tail otherwise.
    pub(crate) fn normalize_order_for(&mut self, facts: &FileFacts) {
        for sym in self.touched_syms(facts) {
            if let Some(v) = self.defs_by_name.get_mut(&sym) {
                v.sort_by_key(|d| (d.file, d.stub));
            }
            if let Some(v) = self.decls_by_name.get_mut(&sym) {
                v.sort_by_key(|d| (d.file, d.stub));
            }
            if let Some(v) = self.callers_of.get_mut(&sym) {
                v.sort_by(|a, b| (a.0, a.1.offset).cmp(&(b.0, b.1.offset)));
            }
            if let Some(v) = self.files_with_ident.get_mut(&sym) {
                v.sort();
            }
        }
    }

    /// One mutation happened; snapshots derived before this are stale.
    pub(crate) fn bump_generation(&mut self) {
        self.generation += 1;
    }

    /// The deduped syms `facts` contributes to, via non-mutating lookup (never interns — a name
    /// absent from the interner cannot be in any map).
    fn touched_syms(&self, facts: &FileFacts) -> Vec<Sym> {
        let mut syms: Vec<Sym> = facts
            .stubs
            .iter()
            .filter_map(|s| self.syms.get(&s.name))
            .chain(facts.calls.iter().filter_map(|c| self.syms.get(&c.callee)))
            .chain(facts.address_taken.iter().filter_map(|(n, _)| self.syms.get(n)))
            .collect();
        syms.sort();
        syms.dedup();
        syms
    }
}

/// Retain only `keep` entries under `sym`; drop the whole entry when nothing survives (keeps
/// retract idempotent and the maps tidy — lookups return an empty slice either way).
fn retain_or_remove<T>(map: &mut HashMap<Sym, Vec<T>>, sym: Sym, keep: impl Fn(&T) -> bool) {
    if let Some(v) = map.get_mut(&sym) {
        v.retain(keep);
        if v.is_empty() {
            map.remove(&sym);
        }
    }
}

/// Append `fid` to the ident map, deduping consecutive pushes (all of one file's contributions
/// arrive together, so "last == fid" is a complete dedup).
fn push_ident(map: &mut HashMap<Sym, Vec<FileId>>, sym: Sym, fid: FileId) {
    let v = map.entry(sym).or_default();
    if v.last() != Some(&fid) {
        v.push(fid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect;

    /// Two-file fixture: a.c defines f (calls g), b.c defines g (calls f) + declares f + takes
    /// h's address in a dispatch table.
    fn fixture() -> Index {
        let a = "void g(void);\nvoid f(void) { g(); }\n";
        let b = "void f(void);\nvoid g(void) { f(); }\nvoid (*tab[])(void) = { h };\n";
        Index::build([
            (PathBuf::from("/proj/a.c"), Arc::new(collect::file_facts(a))),
            (PathBuf::from("/proj/b.c"), Arc::new(collect::file_facts(b))),
        ])
    }

    #[test]
    fn path_table_is_stable_and_dense() {
        let idx = fixture();
        assert_eq!(idx.file_count(), 2);
        let a = idx.file_id(Path::new("/proj/a.c")).expect("a.c interned");
        let b = idx.file_id(Path::new("/proj/b.c")).expect("b.c interned");
        assert_eq!((a, b), (FileId(0), FileId(1)), "sorted input -> deterministic ids");
        assert_eq!(idx.path(a), Some(Path::new("/proj/a.c")));
        assert_eq!(idx.file_id(Path::new("/proj/missing.c")), None);
    }

    #[test]
    fn defs_decls_and_stub_resolution() {
        let idx = fixture();
        let defs = idx.defs_by_name("f");
        assert_eq!(defs.len(), 1, "one definition of f");
        assert_eq!(defs[0].file, FileId(0));
        let stub = idx.stub(defs[0]).expect("DefRef resolves");
        assert_eq!(stub.name, "f");
        assert_eq!(stub.kind, StubKind::FnDef);
        // g is defined in b.c and declared in a.c.
        assert_eq!(idx.defs_by_name("g").len(), 1);
        assert_eq!(idx.defs_by_name("g")[0].file, FileId(1));
        assert_eq!(idx.decls_by_name("g"), &[DefRef { file: FileId(0), stub: 0 }]);
        assert_eq!(idx.decls_by_name("f"), &[DefRef { file: FileId(1), stub: 0 }]);
        assert!(idx.defs_by_name("nope").is_empty());
    }

    #[test]
    fn callers_of_finds_call_edges() {
        let idx = fixture();
        let callers = idx.callers_of("g");
        assert_eq!(callers.len(), 1, "exactly a.c's f() -> g() call");
        assert_eq!(callers[0].0, FileId(0));
        assert_eq!(callers[0].1.callee, "g");
        let callers_f = idx.callers_of("f");
        assert_eq!(callers_f.len(), 1);
        assert_eq!(callers_f[0].0, FileId(1));
        assert!(idx.callers_of("h").is_empty(), "address-taken is not a call");
    }

    #[test]
    fn files_with_ident_spans_all_fact_kinds() {
        let idx = fixture();
        // f: defined in a.c, declared + called in b.c.
        assert_eq!(idx.files_with_ident("f"), &[FileId(0), FileId(1)]);
        // h only ever appears address-taken in b.c.
        assert_eq!(idx.files_with_ident("h"), &[FileId(1)]);
        assert!(idx.files_with_ident("nope").is_empty());
    }

    #[test]
    fn hashes_retained_per_file() {
        let idx = fixture();
        let a = idx.file_id(Path::new("/proj/a.c")).unwrap();
        let facts = idx.facts(a).expect("forward index holds a.c");
        let (ih, bh) = idx.hashes(a).expect("hashes stored");
        assert_eq!((ih, bh), (facts.interface_hash, facts.body_hash));
        // Purity: rebuilding the same text yields the same stored hashes.
        let again = collect::file_facts("void g(void);\nvoid f(void) { g(); }\n");
        assert_eq!((ih, bh), (again.interface_hash, again.body_hash));
    }

    #[test]
    fn call_graph_is_a_query_over_the_index() {
        let idx = fixture();
        let findings = idx.call_graph().tier1_findings();
        assert_eq!(findings.len(), 1, "f<->g cross-file cycle: {findings:?}");
        assert!(findings[0].members.iter().any(|m| m == "f"));
    }
}
