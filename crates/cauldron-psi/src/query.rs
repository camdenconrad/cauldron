//! Query API (docs/psi-design.md, "Query API"): [`PsiSnapshot`] — a cheap, generation-stamped
//! view over a retained [`Index`] that answers the name-level questions the IDE asks
//! (find-definitions, find-usages, callers) without touching the indexer thread.
//!
//! Resolution identity: the structured lookups (defs / decls / call sites) go through the
//! index's interned-Sym inverted maps — the exact same name atoms the call graph's
//! linkage-exact [`crate::graph::SymKey`] resolution is keyed on, so a hit is a real semantic
//! mention, never a substring match. A name-only query has no anchor file, so every linkage
//! variant is reported (a `static f` and an extern `f` both appear; [`Definition::is_static`]
//! carries the distinction). What the structured facts cannot pinpoint — address-taken names
//! keep no retained offset — falls back to the [`Index::files_with_ident`] candidate set plus a
//! word-boundary text scan of just those files ([`UsageKind::Ident`] rows), the designed
//! two-phase find-usages shape.
//!
//! Line numbers and context are computed from the files' CURRENT text, re-read from disk at
//! query time (same policy as [`crate::project::rule1_findings`]); rows whose file vanished
//! since the scan are dropped. When the index holds item-7 OVERLAY facts (dirty-buffer
//! coordinates), disk text is the WRONG coordinate space — [`PsiSnapshot::with_overlay`] lets
//! the caller supply the live buffer texts, mirroring
//! [`crate::project::rule1_findings_with`], so lines/context match what the user sees. The
//! snapshot is stamped with [`Index::generation`] at construction, so consumers can compare
//! answers against the live index and drop stale ones.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::collect::StubKind;
use crate::index::{DefRef, Index};

/// What kind of semantic mention a [`Usage`] row is, in display/sort order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageKind {
    /// A definition site (function or macro).
    Definition,
    /// A declaration (prototype) site.
    Declaration,
    /// A call site whose callee is the queried name (direct or macro-mined).
    Call,
    /// A raw identifier hit from the `files_with_ident` fallback scan (e.g. address-taken
    /// in a dispatch table) — line-precise but text-derived, not structurally resolved.
    Ident,
}

impl UsageKind {
    fn rank(self) -> u8 {
        match self {
            UsageKind::Definition => 0,
            UsageKind::Declaration => 1,
            UsageKind::Call => 2,
            UsageKind::Ident => 3,
        }
    }
}

/// One usage row, panel-shaped: absolute path, 0-based line, the trimmed line text as context.
#[derive(Debug, Clone)]
pub struct Usage {
    pub path: PathBuf,
    /// 0-based (panels usually print 1-based).
    pub line: usize,
    /// The trimmed text of that line.
    pub context: String,
    pub kind: UsageKind,
}

/// One definition site: spans straight from the stub, line computed from current file text.
#[derive(Debug, Clone)]
pub struct Definition {
    pub path: PathBuf,
    /// 0-based line of the NAME (not the definition's first line of leading specifiers).
    pub line: usize,
    /// Whole-stub span (byte offsets into the file at extraction time).
    pub byte_range: Range<usize>,
    /// The name token's span.
    pub name_range: Range<usize>,
    pub kind: StubKind,
    pub is_static: bool,
}

/// A generation-stamped view over one retained index snapshot. Construction is an Arc clone +
/// one u64 read; all queries are map lookups plus on-demand disk reads of the touched files.
pub struct PsiSnapshot {
    index: Arc<Index>,
    generation: u64,
    /// Live buffer texts keyed by absolute path (item 7 coordinate fix): a path present here
    /// has its lines/context computed from THIS text instead of disk — the index may hold
    /// buffer-coordinate overlay facts for exactly those files.
    overlay: HashMap<PathBuf, String>,
}

impl PsiSnapshot {
    pub fn new(index: Arc<Index>) -> PsiSnapshot {
        PsiSnapshot::with_overlay(index, HashMap::new())
    }

    /// Overlay-aware constructor: `overlay` supplies the live text of dirty buffers so rows in
    /// overlaid files resolve offsets against the SAME coordinate space the facts carry
    /// (mirrors [`crate::project::rule1_findings_with`], which fixed this for findings).
    pub fn with_overlay(index: Arc<Index>, overlay: HashMap<PathBuf, String>) -> PsiSnapshot {
        let generation = index.generation();
        PsiSnapshot { index, generation, overlay }
    }

    /// The [`Index::generation`] this snapshot was taken at — compare against the live index to
    /// detect answers computed before an incremental update landed.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The underlying index, for consumers needing the raw maps.
    pub fn index(&self) -> &Index {
        &self.index
    }

    /// Every definition site (functions + macros) named `name`, all linkages, in FileId/stub
    /// order. Rows whose file can no longer be read are dropped.
    pub fn find_definitions(&self, name: &str) -> Vec<Definition> {
        let mut texts = TextCache::new(&self.overlay);
        self.index
            .defs_by_name(name)
            .iter()
            .filter_map(|&dref| self.definition_row(dref, &mut texts))
            .collect()
    }

    /// Every call site targeting `name` (direct + macro-mined), as panel-shaped [`Usage`] rows
    /// in path/line order.
    pub fn callers(&self, name: &str) -> Vec<Usage> {
        let mut texts = TextCache::new(&self.overlay);
        let mut rows: Vec<Usage> = self
            .index
            .callers_of(name)
            .iter()
            .filter_map(|(fid, call)| {
                let path = self.index.path(*fid)?;
                let text = texts.get(path)?;
                let line = line_of(text, call.offset);
                Some(Usage {
                    path: path.to_path_buf(),
                    line,
                    context: context_of(text, line),
                    kind: UsageKind::Call,
                })
            })
            .collect();
        sort_rows(&mut rows);
        rows
    }

    /// Every known mention of `name`: definitions, declarations, and call sites resolved
    /// through the structured maps, then [`UsageKind::Ident`] rows from a word-boundary scan of
    /// the `files_with_ident` candidates for whatever the structured facts don't pinpoint
    /// (address-taken uses, mentions in comments/strings of files that semantically touch the
    /// name). One row per (path, line, kind); lines already claimed by a structured row never
    /// duplicate as Ident rows.
    pub fn find_usages(&self, name: &str) -> Vec<Usage> {
        let mut texts = TextCache::new(&self.overlay);
        let mut rows: Vec<Usage> = Vec::new();

        for (drefs, kind) in [
            (self.index.defs_by_name(name), UsageKind::Definition),
            (self.index.decls_by_name(name), UsageKind::Declaration),
        ] {
            for &dref in drefs {
                if let Some(d) = self.definition_row(dref, &mut texts) {
                    let context =
                        texts.get(&d.path).map(|t| context_of(t, d.line)).unwrap_or_default();
                    rows.push(Usage { path: d.path, line: d.line, context, kind });
                }
            }
        }
        for (fid, call) in self.index.callers_of(name) {
            let Some(path) = self.index.path(*fid) else { continue };
            let Some(text) = texts.get(path) else { continue };
            let line = line_of(text, call.offset);
            rows.push(Usage {
                path: path.to_path_buf(),
                line,
                context: context_of(text, line),
                kind: UsageKind::Call,
            });
        }

        // Phase-2 fallback: word-boundary scan of ONLY the candidate files the index says
        // mention the name at all.
        let claimed: HashSet<(PathBuf, usize)> =
            rows.iter().map(|u| (u.path.clone(), u.line)).collect();
        for &fid in self.index.files_with_ident(name) {
            let Some(path) = self.index.path(fid) else { continue };
            let Some(text) = texts.get(path) else { continue };
            for (line, line_text) in text.lines().enumerate() {
                if !claimed.contains(&(path.to_path_buf(), line))
                    && line_mentions_word(line_text, name)
                {
                    rows.push(Usage {
                        path: path.to_path_buf(),
                        line,
                        context: trim_context(line_text),
                        kind: UsageKind::Ident,
                    });
                }
            }
        }

        sort_rows(&mut rows);
        rows
    }

    fn definition_row(&self, dref: DefRef, texts: &mut TextCache) -> Option<Definition> {
        let stub = self.index.stub(dref)?;
        let path = self.index.path(dref.file)?;
        let text = texts.get(path)?;
        Some(Definition {
            path: path.to_path_buf(),
            line: line_of(text, stub.name_range.start),
            byte_range: stub.byte_range.clone(),
            name_range: stub.name_range.clone(),
            kind: stub.kind,
            is_static: stub.is_static,
        })
    }
}

/// Per-query text cache: overlay (live buffer) text shadows disk; each touched file is
/// resolved at most once per query call.
struct TextCache<'a> {
    overlay: &'a HashMap<PathBuf, String>,
    map: HashMap<PathBuf, Option<String>>,
}

impl<'a> TextCache<'a> {
    fn new(overlay: &'a HashMap<PathBuf, String>) -> TextCache<'a> {
        TextCache { overlay, map: HashMap::new() }
    }

    fn get(&mut self, path: &Path) -> Option<&str> {
        if !self.map.contains_key(path) {
            let text = self
                .overlay
                .get(path)
                .cloned()
                .or_else(|| std::fs::read_to_string(path).ok());
            self.map.insert(path.to_path_buf(), text);
        }
        self.map.get(path).and_then(|t| t.as_deref())
    }
}

/// 0-based line of `offset` — byte-wise, so a stale offset landing mid-codepoint can't panic.
fn line_of(text: &str, offset: usize) -> usize {
    let end = offset.min(text.len());
    text.as_bytes()[..end].iter().filter(|&&b| b == b'\n').count()
}

/// The trimmed text of `line` (0-based), capped for panel display.
fn context_of(text: &str, line: usize) -> String {
    text.lines().nth(line).map(trim_context).unwrap_or_default()
}

fn trim_context(line: &str) -> String {
    line.trim().chars().take(200).collect()
}

/// Whole-word (identifier-boundary) containment: `f` never matches inside `ff` or `_f`.
fn line_mentions_word(line: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut start = 0;
    while let Some(pos) = line[start..].find(name) {
        let at = start + pos;
        let before_ok = line[..at].chars().next_back().is_none_or(|c| !is_ident(c));
        let after_ok = line[at + name.len()..].chars().next().is_none_or(|c| !is_ident(c));
        if before_ok && after_ok {
            return true;
        }
        start = at + name.len();
    }
    false
}

fn sort_rows(rows: &mut [Usage]) {
    rows.sort_by(|a, b| {
        (a.path.as_path(), a.line, a.kind.rank()).cmp(&(b.path.as_path(), b.line, b.kind.rank()))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect;
    use crate::invalidate;

    /// Two-file fixture ON DISK (queries re-read text for lines/context): a.c defines f (calls
    /// g) with a decoy `ff` mention; b.c declares f, defines g (calls f), and takes f's address
    /// in a dispatch table (no retained offset -> exercised via the Ident fallback).
    fn fixture(tag: &str) -> (PathBuf, Index) {
        let dir = std::env::temp_dir().join(format!("cauldron-psi-query-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = "void g(void);\nvoid f(void) { g(); }\n/* decoy: ff off_f f_off ffi */\n";
        let b = "void f(void);\nvoid g(void) { f(); }\nvoid (*tab[])(void) = { f };\n";
        std::fs::write(dir.join("a.c"), a).unwrap();
        std::fs::write(dir.join("b.c"), b).unwrap();
        let idx = Index::build([
            (dir.join("a.c"), Arc::new(collect::file_facts(a))),
            (dir.join("b.c"), Arc::new(collect::file_facts(b))),
        ]);
        (dir, idx)
    }

    #[test]
    fn find_definitions_resolves_stub_spans_and_lines() {
        let (dir, idx) = fixture("defs");
        let snap = PsiSnapshot::new(Arc::new(idx));
        let defs = snap.find_definitions("f");
        assert_eq!(defs.len(), 1, "one definition of f: {defs:?}");
        let d = &defs[0];
        assert_eq!(d.path, dir.join("a.c"));
        assert_eq!(d.line, 1, "definition name on 0-based line 1");
        assert_eq!(d.kind, StubKind::FnDef);
        assert!(!d.is_static);
        assert!(!d.name_range.is_empty() && d.byte_range.start <= d.name_range.start);
        assert!(snap.find_definitions("nope").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_usages_spans_two_files_and_all_kinds() {
        let (dir, idx) = fixture("usages");
        let snap = PsiSnapshot::new(Arc::new(idx));
        let rows = snap.find_usages("f");
        let shaped: Vec<(&Path, usize, UsageKind)> =
            rows.iter().map(|u| (u.path.as_path(), u.line, u.kind)).collect();
        assert_eq!(
            shaped,
            vec![
                (dir.join("a.c").as_path(), 1, UsageKind::Definition),
                (dir.join("b.c").as_path(), 0, UsageKind::Declaration),
                (dir.join("b.c").as_path(), 1, UsageKind::Call),
                (dir.join("b.c").as_path(), 2, UsageKind::Ident),
            ],
            "def + decl + call + address-taken Ident row, sorted; the a.c decoy line (ff, \
             off_f) must NOT match: {rows:?}"
        );
        assert_eq!(rows[0].context, "void f(void) { g(); }", "context is the trimmed line");
        assert!(rows[3].context.contains("tab"), "Ident row carries the dispatch-table line");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn callers_returns_call_sites_only() {
        let (dir, idx) = fixture("callers");
        let snap = PsiSnapshot::new(Arc::new(idx));
        let calls = snap.callers("g");
        assert_eq!(calls.len(), 1, "exactly f's call to g: {calls:?}");
        assert_eq!(calls[0].path, dir.join("a.c"));
        assert_eq!(calls[0].line, 1);
        assert_eq!(calls[0].kind, UsageKind::Call);
        assert!(snap.callers("tab").is_empty(), "address-taken is not a call");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn generation_stamp_visible_and_stable_per_snapshot() {
        let (dir, mut idx) = fixture("gen");
        let snap0 = PsiSnapshot::new(Arc::new(idx.clone()));
        assert_eq!(snap0.generation(), 0, "fresh build stamps generation 0");

        // Real mutation: f no longer calls g. Write-through so query context stays honest.
        let a2 = "void g(void);\nvoid f(void) { }\n";
        std::fs::write(dir.join("a.c"), a2).unwrap();
        let out = invalidate::replace_file_facts(
            &mut idx,
            dir.join("a.c"),
            Arc::new(collect::file_facts(a2)),
        );
        assert!(out.changed());
        let snap1 = PsiSnapshot::new(Arc::new(idx));
        assert_eq!(snap1.generation(), 1, "one mutation, one bump — visible on the snapshot");
        assert_eq!(snap0.generation(), 0, "old snapshot keeps its stamp");
        assert!(snap1.callers("g").is_empty(), "post-mutation answers reflect the new facts");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Issue #2 review: when the index holds OVERLAY facts (buffer coordinates, item 7), the
    /// snapshot must resolve their offsets against the live buffer text, not the shorter disk
    /// text — otherwise every row in a dirty file reports shifted lines and wrong context.
    #[test]
    fn overlay_facts_resolve_against_buffer_text_not_disk() {
        let (dir, mut idx) = fixture("overlay");
        // The user inserts three lines at the top of a.c WITHOUT saving; the worker installed
        // buffer-coordinate facts for it (overlay lane always installs).
        let buffer = "// one\n// two\n// three\nvoid g(void);\nvoid f(void) { g(); }\n";
        invalidate::overlay_file_facts(
            &mut idx,
            dir.join("a.c"),
            Arc::new(collect::file_facts(buffer)),
        );

        // Disk-only snapshot: coordinate spaces mix (the pre-fix failure mode).
        // Overlay-aware snapshot: rows land exactly where the user sees them.
        let mut overlay = HashMap::new();
        overlay.insert(dir.join("a.c"), buffer.to_string());
        let snap = PsiSnapshot::with_overlay(Arc::new(idx), overlay);

        let defs = snap.find_definitions("f");
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].line, 4, "definition line in BUFFER coordinates");
        let calls = snap.callers("g");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].line, 4, "call line in BUFFER coordinates");
        assert_eq!(calls[0].context, "void f(void) { g(); }", "context from the buffer text");
        // Un-overlaid files still resolve from disk.
        let g_defs = snap.find_definitions("g");
        assert_eq!(g_defs.len(), 1);
        assert_eq!(g_defs[0].path, dir.join("b.c"));
        assert_eq!(g_defs[0].line, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn word_boundary_scan() {
        assert!(line_mentions_word("cb = f;", "f"));
        assert!(line_mentions_word("f", "f"));
        assert!(!line_mentions_word("ff();", "f"));
        assert!(!line_mentions_word("off_f = 1;", "f"));
        assert!(line_mentions_word("x = ff + f;", "f"), "later hit after a rejected prefix hit");
        assert!(!line_mentions_word("anything", ""));
    }
}
