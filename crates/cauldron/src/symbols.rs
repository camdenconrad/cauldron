//! Project-wide symbol index + Ctrl+Alt+N "Go to Symbol" overlay.
//!
//! The index is rebuilt on a background thread (cider PTY template: std::thread + mpsc +
//! `request_repaint`) over the CANONICAL workspace file universe (`Workspace::all_files` /
//! `workspace::walk_files` — no private re-walk, no divergent exclude rules), extracting
//! definitions with pragmatic per-language regexes (Rust, C/C++, Python, JS/TS). This is the
//! regex tier, not tree-sitter — good enough for goto-symbol navigation.
//!
//! For C files a second tier supersedes it (cauldron#2 item 8): [`SymbolIndex::sync_psi`]
//! derives entries straight from the retained PSI index's stubs (tree-sitter truth — exact
//! names, kinds, and lines, covering files never opened), and `query`/`len` drop regex rows a
//! PSI entry claims by `(path, line)`. Non-C languages keep the regex tier untouched. The PSI
//! tier updates through the item 4/5/7 invalidation lanes automatically: the app re-syncs
//! whenever the retained index snapshot changes, no manual rescan.
//!
//! A third tier holds live `workspace/symbol` answers (cauldron#2 item 9): while the overlay is
//! open the app fans the query out to every indexed language server and feeds the merged rows in
//! via [`SymbolIndex::extend_lsp`]. Precedence at `(path, line)` granularity: PSI wins for C
//! (stub truth, buffer-fresh through overlays), LSP wins for its languages over the regex tier,
//! regex fills the gaps (no server / not indexed yet / unsupported language).
//!
//! `GotoSymbolUi` mirrors quickopen.rs exactly: centered overlay, monospace input, ranked list,
//! ↑/↓ + Enter / click to pick, Esc closes.

// Pub API awaiting the main.rs integrator (Ctrl+Alt+N wiring) — remove once wired.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, OnceLock};

use cauldron_lsp::lsp_types;
use cauldron_psi::collect::StubKind;
use cauldron_psi::index::Index as PsiIndex;
use egui::{Color32, Key};
use regex::Regex;

use crate::style::colors;

/// Hard cap on indexed symbols (runaway generated code protection).
const MAX_ENTRIES: usize = 100_000;
/// Files larger than this are skipped (generated blobs, not code).
const MAX_FILE_BYTES: u64 = 2_000_000;
/// Max rows shown in the overlay list.
const MAX_RESULTS: usize = 50;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SymKind {
    Function,
    Struct,
    Enum,
    Trait,
    Class,
    Const,
    Define,
    TypeDef,
}

impl SymKind {
    /// Single-char glyph shown in the overlay list (RustRover-style kind badge).
    pub fn glyph(self) -> &'static str {
        match self {
            SymKind::Function => "f",
            SymKind::Struct => "S",
            SymKind::Enum => "E",
            SymKind::Trait => "T",
            SymKind::Class => "C",
            SymKind::Const => "c",
            SymKind::Define => "#",
            SymKind::TypeDef => "t",
        }
    }

    /// Kind accent color, from the style tokens.
    pub fn color(self) -> Color32 {
        match self {
            SymKind::Function => colors::AMBER(),
            SymKind::Struct => colors::MOSS(),
            SymKind::Enum => colors::MOSS(),
            SymKind::Trait => colors::PLUM(),
            SymKind::Class => colors::PLUM(),
            SymKind::Const => colors::ACCENT_HI(),
            SymKind::Define => colors::WARN(),
            SymKind::TypeDef => colors::TEXT_MUTED(),
        }
    }
}

#[derive(Clone)]
pub struct SymbolEntry {
    pub name: String,
    /// Lowercase copy of `name`, precomputed for matching.
    pub name_lower: String,
    pub kind: SymKind,
    pub path: PathBuf,
    /// 0-based line.
    pub line: usize,
}

enum Msg {
    Batch { generation: u64, entries: Vec<SymbolEntry> },
    Done { generation: u64 },
}

pub struct SymbolIndex {
    entries: Vec<SymbolEntry>,
    /// UI cache-invalidation counter: bumped by EVERY visible mutation (rebuilds, PSI/LSP tier
    /// changes). Deliberately separate from `stream_gen` — tier syncs while a regex build is
    /// streaming must invalidate result caches WITHOUT orphaning the inflight stream.
    generation: u64,
    /// Stream stamp: bumped only when a worker stream starts (`rebuild`/`refresh_files`) or is
    /// deliberately orphaned (`clear`). `poll` applies only messages carrying this stamp.
    stream_gen: u64,
    building: bool,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    /// The C tier (cauldron#2 item 8): entries derived from the retained PSI index's stubs,
    /// superseding the regex tier for C paths. Rebuilt by [`SymbolIndex::sync_psi`].
    psi_entries: Vec<SymbolEntry>,
    /// `(path -> lines)` claimed by `psi_entries` — regex rows at a claimed position are
    /// duplicates of PSI truth and are dropped by `query`/`len`.
    psi_claimed: HashMap<PathBuf, HashSet<usize>>,
    /// The exact snapshot `psi_entries` came from. Same Arc AND same generation = already
    /// synced; a fresh full scan produces a NEW index that restarts at generation 0, so the
    /// generation alone is not a change detector.
    psi_index: Option<Arc<PsiIndex>>,
    /// The LSP tier (cauldron#2 item 9): live `workspace/symbol` rows for the CURRENT overlay
    /// query, accumulated across answering servers via [`SymbolIndex::extend_lsp`] and dropped
    /// by [`SymbolIndex::clear_lsp`] when the query changes / the overlay closes.
    lsp_entries: Vec<SymbolEntry>,
    /// `(path -> lines)` claimed by `lsp_entries` — regex rows at a claimed position duplicate
    /// server truth and are dropped by `query`/`len` (PSI rows are NOT: PSI wins for C).
    lsp_claimed: HashMap<PathBuf, HashSet<usize>>,
}

impl Default for SymbolIndex {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            entries: Vec::new(),
            generation: 0,
            stream_gen: 0,
            building: false,
            tx,
            rx,
            psi_entries: Vec::new(),
            psi_claimed: HashMap::new(),
            psi_index: None,
            lsp_entries: Vec::new(),
            lsp_claimed: HashMap::new(),
        }
    }
}

impl SymbolIndex {
    pub fn is_building(&self) -> bool {
        self.building
    }

    /// Total queryable symbols: the PSI tier, LSP rows PSI does not claim, and regex rows
    /// neither claims.
    pub fn len(&self) -> usize {
        if self.psi_entries.is_empty() && self.lsp_entries.is_empty() {
            return self.entries.len();
        }
        self.psi_entries.len()
            + self.lsp_entries.iter().filter(|e| !self.psi_claims(e)).count()
            + self.entries.iter().filter(|e| !self.psi_claims(e) && !self.lsp_claims(e)).count()
    }

    /// Production caller was the retired rebuild-only-when-empty kick (rebuilds are now
    /// event-driven: build-on-open / project switch / watcher); kept for UIs + tests.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.psi_entries.is_empty() && self.lsp_entries.is_empty()
    }

    /// Monotonic rebuild counter — bumps every `rebuild`, letting UIs invalidate cached results.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Drain the background stream into `entries`. Call once per frame (cheap when idle).
    /// Messages are stamped with `stream_gen`, NOT `generation`: PSI/LSP tier syncs bump the
    /// UI generation freely mid-build without orphaning the stream (or wedging `building`).
    pub fn poll(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Batch { generation, entries } => {
                    if generation == self.stream_gen {
                        let room = MAX_ENTRIES.saturating_sub(self.entries.len());
                        self.entries.extend(entries.into_iter().take(room));
                        self.generation += 1;
                    }
                }
                Msg::Done { generation } => {
                    if generation == self.stream_gen {
                        self.building = false;
                        crate::boot_trace::boot_mark!(
                            "symbols-rebuild-done entries={}",
                            self.entries.len()
                        );
                    }
                }
            }
        }
    }

    /// Rebuild the index on a background thread from `files` — the canonical workspace file
    /// universe (absolute paths, excludes already applied by the producer: pass
    /// [`crate::workspace::Workspace::all_files`]). No walking happens here, so the index can
    /// never disagree with the tree/quick-open/search about which files exist. Previous
    /// contents are cleared immediately; results stream in via `poll`.
    pub fn rebuild(&mut self, files: &[PathBuf], ctx: &egui::Context) {
        self.stream_gen += 1;
        self.generation += 1;
        self.entries.clear();
        self.building = true;
        let generation = self.stream_gen;
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        let files = files.to_vec();
        let spawned = std::thread::Builder::new()
            .name("cauldron-symbol-index".into())
            .spawn(move || {
                let mut batch: Vec<SymbolEntry> = Vec::new();
                let mut total = 0usize;
                let mut walked = 0usize;
                'walk: for path in &files {
                    let Some(lang) = Lang::of(path) else { continue };
                    if std::fs::metadata(path).map(|m| m.len() > MAX_FILE_BYTES).unwrap_or(true) {
                        continue;
                    }
                    let Ok(text) = std::fs::read_to_string(path) else { continue };
                    walked += 1;
                    for (name, kind, line) in extract_symbols(lang, &text) {
                        let name_lower = name.to_lowercase();
                        batch.push(SymbolEntry {
                            name,
                            name_lower,
                            kind,
                            path: path.clone(),
                            line,
                        });
                        total += 1;
                        if total >= MAX_ENTRIES {
                            break 'walk;
                        }
                    }
                    if batch.len() >= 2048 {
                        if tx
                            .send(Msg::Batch { generation, entries: std::mem::take(&mut batch) })
                            .is_err()
                        {
                            return;
                        }
                        ctx.request_repaint();
                    }
                    if walked.is_multiple_of(256) {
                        ctx.request_repaint();
                    }
                }
                if !batch.is_empty() {
                    let _ = tx.send(Msg::Batch { generation, entries: batch });
                }
                let _ = tx.send(Msg::Done { generation });
                ctx.request_repaint();
            });
        if spawned.is_err() {
            // A failed spawn sends no Done: leaving `building` true would wedge the
            // event-driven drain (take_symbol_rebuild_kick) forever — no rebuild would ever
            // fire again and watcher invalidations would re-arm the pending flag indefinitely.
            self.building = false;
        }
    }

    /// Drop everything NOW (project switch): entries gone, any inflight stream orphaned (its
    /// messages carry a stale generation). The caller schedules the rebuild for the new root —
    /// between the two, goto-symbol honestly shows nothing rather than the OLD project.
    pub fn clear(&mut self) {
        self.stream_gen += 1;
        self.generation += 1;
        self.entries.clear();
        self.building = false;
        self.psi_entries.clear();
        self.psi_claimed.clear();
        self.psi_index = None;
        self.lsp_entries.clear();
        self.lsp_claimed.clear();
    }

    /// Derive the C tier from the retained PSI index snapshot (cauldron#2 item 8): one entry
    /// per definition-shaped stub — functions, macros, typedefs; prototypes are skipped, same
    /// as the regex tier's `;`-line rule — with the exact name row tree-sitter recorded
    /// (`Stub::name_line`, buffer coordinates for overlaid files). Covers every scanned file,
    /// opened or not, and reflects whatever the save/overlay/watcher invalidation lanes did to
    /// the index. No-op when `index` is the snapshot already synced (Arc identity + generation
    /// — a fresh full scan restarts at generation 0, so the stamp alone can't detect change);
    /// otherwise bumps `generation` so UI result caches invalidate.
    pub fn sync_psi(&mut self, index: &Arc<PsiIndex>) {
        if self
            .psi_index
            .as_ref()
            .is_some_and(|old| Arc::ptr_eq(old, index) && old.generation() == index.generation())
        {
            return;
        }
        self.psi_entries = psi_symbol_entries(index);
        self.psi_claimed.clear();
        for e in &self.psi_entries {
            self.psi_claimed.entry(e.path.clone()).or_default().insert(e.line);
        }
        self.psi_index = Some(Arc::clone(index));
        self.generation += 1;
    }

    /// Drop the C tier (PSI parked at NotCProject: standards off / project without C) — C
    /// symbols honestly fall back to the regex rows instead of freezing at the last snapshot.
    pub fn clear_psi(&mut self) {
        if self.psi_index.is_none() && self.psi_entries.is_empty() {
            return;
        }
        self.psi_entries.clear();
        self.psi_claimed.clear();
        self.psi_index = None;
        self.generation += 1;
    }

    /// True when the PSI tier claims `e`'s exact `(path, line)` — the regex row is a duplicate.
    fn psi_claims(&self, e: &SymbolEntry) -> bool {
        self.psi_claimed.get(&e.path).is_some_and(|lines| lines.contains(&e.line))
    }

    /// True when the LSP tier claims `e`'s exact `(path, line)` — server truth supersedes the
    /// regex row (only regex rows are filtered by this; PSI outranks LSP for C).
    fn lsp_claims(&self, e: &SymbolEntry) -> bool {
        self.lsp_claimed.get(&e.path).is_some_and(|lines| lines.contains(&e.line))
    }

    /// Merge one server's `workspace/symbol` answer into the LSP tier (cauldron#2 item 9).
    /// Several servers can answer the same query — rows are ACCUMULATED, deduped inside the
    /// tier by `(path, line)` (first server to claim a position wins). Bumps `generation` when
    /// anything landed so overlay result caches invalidate.
    pub fn extend_lsp(&mut self, entries: Vec<SymbolEntry>) {
        let mut added = false;
        for e in entries {
            let lines = self.lsp_claimed.entry(e.path.clone()).or_default();
            if lines.insert(e.line) {
                self.lsp_entries.push(e);
                added = true;
            }
        }
        if added {
            self.generation += 1;
        }
    }

    /// Drop the LSP tier (query changed / overlay closed) — its rows answer exactly one
    /// `workspace/symbol` query and must never leak into the next one.
    pub fn clear_lsp(&mut self) {
        if self.lsp_entries.is_empty() {
            return;
        }
        self.lsp_entries.clear();
        self.lsp_claimed.clear();
        self.generation += 1;
    }

    /// Incrementally re-index just `files` (watcher/save-driven invalidation): their old
    /// entries are dropped immediately, fresh extractions stream in via [`SymbolIndex::poll`]
    /// under the bumped generation (stale caches invalidate, orphaned streams stay dropped).
    /// Do NOT call while [`SymbolIndex::is_building`] — a running stream's batches would be
    /// orphaned mid-flight, losing symbols; schedule a full rebuild instead.
    pub fn refresh_files(&mut self, files: &[PathBuf], ctx: &egui::Context) {
        if files.is_empty() {
            return;
        }
        debug_assert!(!self.building, "refresh_files during a build orphans the stream");
        let affected: std::collections::HashSet<PathBuf> = files.iter().cloned().collect();
        self.entries.retain(|e| !affected.contains(&e.path));
        self.stream_gen += 1;
        self.generation += 1;
        self.building = true;
        let generation = self.stream_gen;
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        let files = files.to_vec();
        let spawned = std::thread::Builder::new()
            .name("cauldron-symbol-refresh".into())
            .spawn(move || {
                let mut batch: Vec<SymbolEntry> = Vec::new();
                for path in &files {
                    let Some(lang) = Lang::of(path) else { continue };
                    // Same caps as the full rebuild: oversized/vanished files contribute nothing
                    // (a deleted file's entries were already dropped above).
                    if std::fs::metadata(path).map(|m| m.len() > MAX_FILE_BYTES).unwrap_or(true) {
                        continue;
                    }
                    let Ok(text) = std::fs::read_to_string(path) else { continue };
                    for (name, kind, line) in extract_symbols(lang, &text) {
                        let name_lower = name.to_lowercase();
                        batch.push(SymbolEntry { name, name_lower, kind, path: path.clone(), line });
                    }
                }
                if !batch.is_empty() {
                    let _ = tx.send(Msg::Batch { generation, entries: batch });
                }
                let _ = tx.send(Msg::Done { generation });
                ctx.request_repaint();
            });
        if spawned.is_err() {
            // Same wedge as `rebuild`: no thread means no Done, so un-wedge `building` here.
            self.building = false;
        }
    }

    /// Rank entries against `q` (subsequence fuzzy, quickopen-style):
    /// exact > prefix > substring > subsequence, ties broken by shorter name. The candidate set
    /// merges the three tiers with `(path, line)` dedupe: PSI first (wins for C), then LSP rows
    /// PSI doesn't claim (server truth for its languages), then regex rows neither tier claims.
    /// LSP rows went through the SERVER's matcher for this query, but they still pass through
    /// the local filter/ranking so the list stays consistent.
    pub fn query(&self, q: &str, max: usize) -> Vec<&SymbolEntry> {
        let q = q.trim().to_lowercase();
        let merged = self
            .psi_entries
            .iter()
            .chain(self.lsp_entries.iter().filter(|e| !self.psi_claims(e)))
            .chain(self.entries.iter().filter(|e| !self.psi_claims(e) && !self.lsp_claims(e)));
        if q.is_empty() {
            return merged.take(max).collect();
        }
        let mut scored: Vec<(u8, usize, usize, &SymbolEntry)> = merged
            .enumerate()
            .filter_map(|(i, e)| match_tier(&e.name_lower, &q).map(|t| (t, e.name.len(), i, e)))
            .collect();
        scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
        scored.into_iter().take(max).map(|(_, _, _, e)| e).collect()
    }
}

/// One [`SymbolEntry`] per definition-shaped stub in the retained PSI index, in FileId/stub
/// order. Kind mapping keeps the existing UI glyphs/colors: functions stay `f`, macros map to
/// the `#define` badge, typedefs to `t`. `FnDecl` prototypes are not goto-symbol targets
/// (mirrors the regex tier, which skips `;`-terminated lines).
fn psi_symbol_entries(index: &PsiIndex) -> Vec<SymbolEntry> {
    let mut out = Vec::new();
    for (_fid, path, facts) in index.files() {
        for stub in &facts.stubs {
            let kind = match stub.kind {
                StubKind::FnDef => SymKind::Function,
                StubKind::MacroFn | StubKind::MacroObj => SymKind::Define,
                StubKind::Typedef => SymKind::TypeDef,
                // The PSI tier can finally produce these; before, struct/enum rows in goto-symbol
                // came only from the regex fallback tier.
                StubKind::Struct | StubKind::Union => SymKind::Struct,
                StubKind::Enum => SymKind::Enum,
                StubKind::Global => SymKind::Const,
                // Not goto-symbol targets: prototypes and forward declarations point AT the real
                // definition, and members are found through their aggregate, not as top-level
                // rows that would swamp the list (a cFS tree has tens of thousands of fields).
                StubKind::FnDecl
                | StubKind::TagDecl
                | StubKind::GlobalDecl
                | StubKind::Field
                | StubKind::Enumerator => continue,
            };
            out.push(SymbolEntry {
                name: stub.name.clone(),
                name_lower: stub.name.to_lowercase(),
                kind,
                path: path.to_path_buf(),
                line: stub.name_line,
            });
            if out.len() >= MAX_ENTRIES {
                return out;
            }
        }
    }
    out
}

/// Convert one server's `workspace/symbol` rows into [`SymbolEntry`]s (cauldron#2 item 9):
/// `file://` locations only (macro-expansion schemes etc. are dropped — not openable files),
/// 0-based line straight from the location range, kinds mapped onto the existing badge set.
pub fn lsp_symbol_entries(symbols: &[lsp_types::SymbolInformation]) -> Vec<SymbolEntry> {
    symbols
        .iter()
        .filter_map(|s| {
            let path = cauldron_lsp::capabilities::uri_to_path(&s.location.uri)?;
            Some(SymbolEntry {
                name: s.name.clone(),
                name_lower: s.name.to_lowercase(),
                kind: sym_kind_of(s.kind),
                path,
                line: s.location.range.start.line as usize,
            })
        })
        .collect()
}

/// LSP SymbolKind → the overlay's badge set. Value-ish kinds share the `c` badge, container-ish
/// kinds the `S` badge; anything exotic falls back to the muted `t` (generic symbol) rather
/// than being dropped — a server hit is a hit.
fn sym_kind_of(kind: lsp_types::SymbolKind) -> SymKind {
    use lsp_types::SymbolKind as K;
    match kind {
        K::FUNCTION | K::METHOD | K::CONSTRUCTOR => SymKind::Function,
        K::STRUCT => SymKind::Struct,
        K::ENUM | K::ENUM_MEMBER => SymKind::Enum,
        K::INTERFACE => SymKind::Trait,
        K::CLASS => SymKind::Class,
        K::CONSTANT | K::VARIABLE | K::FIELD | K::PROPERTY => SymKind::Const,
        K::MODULE | K::NAMESPACE | K::PACKAGE | K::OBJECT => SymKind::Struct,
        _ => SymKind::TypeDef,
    }
}

/// Match tier for a lowercase haystack vs lowercase needle:
/// 0 exact, 1 prefix, 2 substring, 3 subsequence, None = no match.
fn match_tier(name_lower: &str, q_lower: &str) -> Option<u8> {
    if name_lower == q_lower {
        return Some(0);
    }
    if name_lower.starts_with(q_lower) {
        return Some(1);
    }
    if name_lower.contains(q_lower) {
        return Some(2);
    }
    // Subsequence: every query char appears in order.
    let mut hay = name_lower.chars();
    if q_lower.chars().all(|qc| hay.any(|hc| hc == qc)) {
        return Some(3);
    }
    None
}

// ---------------------------------------------------------------------------
// Per-language regex extractors (the pragmatic tier).
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Lang {
    Rust,
    C,
    Python,
    Js,
}

impl Lang {
    fn of(path: &Path) -> Option<Lang> {
        let ext = path.extension()?.to_str()?.to_lowercase();
        match ext.as_str() {
            "rs" => Some(Lang::Rust),
            "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => Some(Lang::C),
            "py" | "pyi" => Some(Lang::Python),
            "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" => Some(Lang::Js),
            _ => None,
        }
    }
}

fn re(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pat).expect("static regex"))
}

/// Extract `(name, kind, 0-based line)` definitions from `text` for `lang`.
fn extract_symbols(lang: Lang, text: &str) -> Vec<(String, SymKind, usize)> {
    let mut out = Vec::new();
    for (ln, line) in text.lines().enumerate() {
        match lang {
            Lang::Rust => extract_rust_line(line, ln, &mut out),
            Lang::C => extract_c_line(line, ln, &mut out),
            Lang::Python => extract_python_line(line, ln, &mut out),
            Lang::Js => extract_js_line(line, ln, &mut out),
        }
    }
    out
}

fn extract_rust_line(line: &str, ln: usize, out: &mut Vec<(String, SymKind, usize)>) {
    static RE: OnceLock<Regex> = OnceLock::new();
    let r = re(
        &RE,
        r#"^\s*(?:pub\s*(?:\([^)]*\))?\s+)?(?:default\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?(?:extern\s+"[^"]*"\s+)?(fn|struct|enum|trait|type|const|static)\s+([A-Za-z_]\w*)"#,
    );
    if let Some(c) = r.captures(line) {
        let kind = match &c[1] {
            "fn" => SymKind::Function,
            "struct" => SymKind::Struct,
            "enum" => SymKind::Enum,
            "trait" => SymKind::Trait,
            "type" => SymKind::TypeDef,
            _ => SymKind::Const, // const | static
        };
        out.push((c[2].to_string(), kind, ln));
    }
}

const C_KEYWORDS: &[&str] = &[
    "if", "else", "for", "while", "do", "switch", "case", "return", "goto", "sizeof", "typedef",
    "struct", "enum", "union", "class", "namespace", "using", "template", "public", "private",
    "protected", "static_assert", "extern",
];

fn extract_c_line(line: &str, ln: usize, out: &mut Vec<(String, SymKind, usize)>) {
    static DEFINE: OnceLock<Regex> = OnceLock::new();
    static AGG: OnceLock<Regex> = OnceLock::new();
    static TYPEDEF: OnceLock<Regex> = OnceLock::new();
    static CLASS: OnceLock<Regex> = OnceLock::new();
    static FUNC: OnceLock<Regex> = OnceLock::new();

    if let Some(c) = re(&DEFINE, r"^\s*#\s*define\s+([A-Za-z_]\w*)").captures(line) {
        out.push((c[1].to_string(), SymKind::Define, ln));
        return;
    }
    if let Some(c) = re(&TYPEDEF, r"^typedef\b.*?\b([A-Za-z_]\w*)\s*;").captures(line) {
        out.push((c[1].to_string(), SymKind::TypeDef, ln));
        return;
    }
    if let Some(c) =
        re(&AGG, r"^(struct|enum|union)\s+([A-Za-z_]\w*)").captures(line)
    {
        let kind = if &c[1] == "enum" { SymKind::Enum } else { SymKind::Struct };
        out.push((c[2].to_string(), kind, ln));
        return;
    }
    if let Some(c) = re(&CLASS, r"^\s*class\s+([A-Za-z_]\w*)").captures(line) {
        out.push((c[1].to_string(), SymKind::Class, ln));
        return;
    }
    // Function *definitions* at column 0: `ret_type name(args...` with no `;` (prototypes end
    // with `;`), first token not a control keyword.
    if line.starts_with(|ch: char| ch.is_ascii_alphabetic() || ch == '_')
        && !line.contains(';')
        && line.contains('(')
    {
        let first = line.split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_').next();
        if first.map(|w| C_KEYWORDS.contains(&w)).unwrap_or(true) {
            return;
        }
        if let Some(c) =
            re(&FUNC, r"^[A-Za-z_][\w\s\*&:<>,]*?\b([A-Za-z_]\w*)\s*\(").captures(line)
        {
            let name = c[1].to_string();
            if !C_KEYWORDS.contains(&name.as_str()) {
                out.push((name, SymKind::Function, ln));
            }
        }
    }
}

fn extract_python_line(line: &str, ln: usize, out: &mut Vec<(String, SymKind, usize)>) {
    static DEF: OnceLock<Regex> = OnceLock::new();
    static CLASS: OnceLock<Regex> = OnceLock::new();
    if let Some(c) = re(&DEF, r"^\s*(?:async\s+)?def\s+([A-Za-z_]\w*)").captures(line) {
        out.push((c[1].to_string(), SymKind::Function, ln));
    } else if let Some(c) = re(&CLASS, r"^\s*class\s+([A-Za-z_]\w*)").captures(line) {
        out.push((c[1].to_string(), SymKind::Class, ln));
    }
}

fn extract_js_line(line: &str, ln: usize, out: &mut Vec<(String, SymKind, usize)>) {
    static FUNC: OnceLock<Regex> = OnceLock::new();
    static CLASS: OnceLock<Regex> = OnceLock::new();
    static ARROW: OnceLock<Regex> = OnceLock::new();
    if let Some(c) = re(
        &FUNC,
        r"^\s*(?:export\s+)?(?:default\s+)?(?:async\s+)?function\s*\*?\s*([A-Za-z_$][\w$]*)",
    )
    .captures(line)
    {
        out.push((c[1].to_string(), SymKind::Function, ln));
    } else if let Some(c) =
        re(&CLASS, r"^\s*(?:export\s+)?(?:default\s+)?class\s+([A-Za-z_$][\w$]*)").captures(line)
    {
        out.push((c[1].to_string(), SymKind::Class, ln));
    } else if let Some(c) = re(
        &ARROW,
        r"^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*=\s*(?:async\s+)?(?:function\b|(?:\([^)]*\)|[A-Za-z_$][\w$]*)\s*=>)",
    )
    .captures(line)
    {
        out.push((c[1].to_string(), SymKind::Const, ln));
    }
}

// ---------------------------------------------------------------------------
// Go to Symbol overlay (Ctrl+Alt+N) — quickopen-style.
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct GotoSymbolUi {
    open: bool,
    query: String,
    /// True the frame the overlay opens — used to focus the text field once.
    just_opened: bool,
    /// Cached picks for (`results_for` query, index generation, index len).
    results: Vec<SymbolEntry>,
    results_for: Option<(String, u64, usize)>,
    selected: usize,
}

impl GotoSymbolUi {
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The overlay's current query text — the app fans it out as `workspace/symbol` when it
    /// changes (cauldron#2 item 9).
    pub fn query_text(&self) -> &str {
        &self.query
    }

    pub fn open(&mut self) {
        self.open = true;
        self.just_opened = true;
        self.query.clear();
        self.results_for = None;
        self.selected = 0;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.results.clear();
        self.results_for = None;
    }

    /// Draw the overlay if open. Returns `Some((path, 0-based line))` the frame a symbol is
    /// chosen (Enter / click), which also closes it. Esc closes, returning `None`.
    pub fn ui(&mut self, ctx: &egui::Context, index: &SymbolIndex) -> Option<(PathBuf, usize)> {
        if !self.open {
            return None;
        }
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.close();
            return None;
        }
        let mut chosen: Option<(PathBuf, usize)> = None;

        egui::Area::new("gotosymbol".into())
            .anchor(egui::Align2::CENTER_TOP, [0.0, 80.0])
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .inner_margin(egui::Margin::same(crate::style::sizes::OVERLAY_PAD))
                    .show(ui, |ui| {
                        ui.set_width(560.0);

                        ui.horizontal(|ui| {
                            crate::style::panel_header_inline(ui, "Go to Symbol");
                            if index.is_building() {
                                ui.spinner();
                            } else {
                                ui.colored_label(
                                    colors::TEXT_FAINT(),
                                    format!("{} symbols", index.len()),
                                );
                            }
                        });
                        let edit = egui::TextEdit::singleline(&mut self.query)
                            .hint_text("Go to symbol…")
                            .desired_width(f32::INFINITY)
                            .font(egui::TextStyle::Monospace);
                        let resp = ui.add(edit);
                        if self.just_opened {
                            resp.request_focus();
                            self.just_opened = false;
                        }

                        let key = (self.query.clone(), index.generation(), index.len());
                        if self.results_for.as_ref() != Some(&key) {
                            self.results =
                                index.query(&self.query, MAX_RESULTS).into_iter().cloned().collect();
                            self.results_for = Some(key);
                            self.selected = 0;
                        }

                        let shown = self.results.len();
                        if shown > 0 {
                            if ui.input(|i| i.key_pressed(Key::ArrowDown)) {
                                self.selected = (self.selected + 1) % shown;
                            }
                            if ui.input(|i| i.key_pressed(Key::ArrowUp)) {
                                self.selected = (self.selected + shown - 1) % shown;
                            }
                            if ui.input(|i| i.key_pressed(Key::Enter)) {
                                let e = &self.results[self.selected];
                                chosen = Some((e.path.clone(), e.line));
                            }
                        }

                        ui.add_space(4.0);
                        crate::style::hairline(ui);
                        egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                            for (row, e) in self.results.iter().enumerate() {
                                let selected = row == self.selected;
                                let mut job = egui::text::LayoutJob::default();
                                let font = egui::TextStyle::Monospace.resolve(ui.style());
                                job.append(
                                    &format!("{} ", e.kind.glyph()),
                                    0.0,
                                    egui::TextFormat {
                                        font_id: font.clone(),
                                        color: e.kind.color(),
                                        ..Default::default()
                                    },
                                );
                                job.append(
                                    &e.name,
                                    0.0,
                                    egui::TextFormat {
                                        font_id: font.clone(),
                                        color: if selected { colors::ACCENT_HI() } else { colors::TEXT() },
                                        ..Default::default()
                                    },
                                );
                                job.append(
                                    &format!("  {}:{}", e.path.display(), e.line + 1),
                                    0.0,
                                    egui::TextFormat {
                                        font_id: font,
                                        color: colors::TEXT_FAINT(),
                                        ..Default::default()
                                    },
                                );
                                if ui.selectable_label(selected, job).clicked_by(egui::PointerButton::Primary) {
                                    chosen = Some((e.path.clone(), e.line));
                                }
                            }
                            if self.results.is_empty() && !self.query.is_empty() {
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

    fn names(v: &[(String, SymKind, usize)]) -> Vec<(&str, SymKind, usize)> {
        v.iter().map(|(n, k, l)| (n.as_str(), *k, *l)).collect()
    }

    #[test]
    fn rust_extractor_finds_definitions() {
        let src = "\
use std::fmt;

pub fn render(x: u32) -> u32 { x }
pub(crate) struct Widget {
    id: u32,
}
enum Mode { A, B }
pub trait Draw {}
const MAX_DEPTH: usize = 4;
pub type Alias = Vec<u8>;
pub async unsafe fn weird() {}
static GLOBAL: u8 = 0;
";
        let syms = extract_symbols(Lang::Rust, src);
        assert_eq!(
            names(&syms),
            vec![
                ("render", SymKind::Function, 2),
                ("Widget", SymKind::Struct, 3),
                ("Mode", SymKind::Enum, 6),
                ("Draw", SymKind::Trait, 7),
                ("MAX_DEPTH", SymKind::Const, 8),
                ("Alias", SymKind::TypeDef, 9),
                ("weird", SymKind::Function, 10),
                ("GLOBAL", SymKind::Const, 11),
            ]
        );
    }

    #[test]
    fn c_extractor_finds_definitions() {
        let src = "\
#include <stdio.h>
#define BUF_SIZE 128
typedef struct point point_t;
struct point { int x; };
enum color { RED };
int main(void)
{
    if (x) {
        return 0;
    }
}
static void helper(int a, char *b)
int proto(void);
";
        let syms = extract_symbols(Lang::C, src);
        assert_eq!(
            names(&syms),
            vec![
                ("BUF_SIZE", SymKind::Define, 1),
                ("point_t", SymKind::TypeDef, 2),
                ("point", SymKind::Struct, 3),
                ("color", SymKind::Enum, 4),
                ("main", SymKind::Function, 5),
                ("helper", SymKind::Function, 11),
                // proto(void); is a prototype (ends with ;) — excluded
            ]
        );
    }

    #[test]
    fn python_extractor_finds_definitions() {
        let src = "\
import os

class Frobnicator:
    def spin(self):
        pass

async def fetch_all():
    pass
";
        let syms = extract_symbols(Lang::Python, src);
        assert_eq!(
            names(&syms),
            vec![
                ("Frobnicator", SymKind::Class, 2),
                ("spin", SymKind::Function, 3),
                ("fetch_all", SymKind::Function, 6),
            ]
        );
    }

    #[test]
    fn js_extractor_finds_definitions() {
        let src = "\
export default class App {}
function render(props) {}
export async function load() {}
const handler = async (e) => {}
let square = x => x * x
var legacy = function (a) {}
const NOT_A_FN = 42;
";
        let syms = extract_symbols(Lang::Js, src);
        assert_eq!(
            names(&syms),
            vec![
                ("App", SymKind::Class, 0),
                ("render", SymKind::Function, 1),
                ("load", SymKind::Function, 2),
                ("handler", SymKind::Const, 3),
                ("square", SymKind::Const, 4),
                ("legacy", SymKind::Const, 5),
            ]
        );
    }

    fn index_with(names: &[&str]) -> SymbolIndex {
        let mut idx = SymbolIndex::default();
        idx.entries = names
            .iter()
            .enumerate()
            .map(|(i, n)| SymbolEntry {
                name: n.to_string(),
                name_lower: n.to_lowercase(),
                kind: SymKind::Function,
                path: PathBuf::from("/r/a.rs"),
                line: i,
            })
            .collect();
        idx
    }

    #[test]
    fn query_ranks_exact_over_prefix_over_substring_over_subsequence() {
        // "reindeer" matches "render" only as a subsequence (r·e·n·d·e·r).
        let idx = index_with(&["render_frame", "prerender", "reindeer", "render"]);
        let got: Vec<&str> = idx.query("render", 10).iter().map(|e| e.name.as_str()).collect();
        assert_eq!(got, vec!["render", "render_frame", "prerender", "reindeer"]);
    }

    #[test]
    fn query_tie_breaks_shorter_name_and_is_case_insensitive() {
        let idx = index_with(&["DoThingLonger", "DoThing"]);
        let got: Vec<&str> = idx.query("dothing", 10).iter().map(|e| e.name.as_str()).collect();
        assert_eq!(got[0], "DoThing");
        assert!(idx.query("zzz", 10).is_empty());
    }

    #[test]
    fn match_tier_subsequence_only_in_order() {
        assert_eq!(match_tier("workspace", "wksp"), Some(3));
        assert_eq!(match_tier("workspace", "pskw"), None);
        assert_eq!(match_tier("workspace", "work"), Some(1));
        assert_eq!(match_tier("workspace", "space"), Some(2));
        assert_eq!(match_tier("workspace", "workspace"), Some(0));
    }

    /// The exclude bug (absolute walked paths vs workspace-relative excludes → excludes never
    /// matched) is fixed by construction: the index consumes the canonical universe, so a file
    /// under an excluded dir never reaches it — and PSI, fed the same universe, agrees exactly.
    #[test]
    fn symbol_index_and_psi_share_the_workspace_universe() {
        let dir = std::env::temp_dir()
            .join(format!("cauldron-shared-universe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("junkdir")).unwrap();
        std::fs::write(dir.join("src/a.c"), "void alpha_fn(void)\n{\n}\n").unwrap();
        std::fs::write(dir.join("src/b.h"), "#define BETA_DEF 1\n").unwrap();
        std::fs::write(dir.join("junkdir/c.c"), "void gamma_fn(void)\n{\n}\n").unwrap();

        // ONE producer: the workspace walk, with junkdir excluded (workspace-relative).
        let excludes = vec![PathBuf::from("junkdir")];
        let universe = crate::workspace::walk_files(&dir, &excludes);
        assert!(
            !universe.iter().any(|p| p.starts_with(dir.join("junkdir"))),
            "excluded dir leaked into the universe"
        );

        // SymbolIndex consumes the universe verbatim.
        let mut idx = SymbolIndex::default();
        let ctx = egui::Context::default();
        idx.rebuild(&universe, &ctx);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while idx.is_building() {
            idx.poll();
            assert!(std::time::Instant::now() < deadline, "symbol index build timed out");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let names: Vec<&str> = idx.entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"alpha_fn"));
        assert!(names.contains(&"BETA_DEF"));
        assert!(!names.contains(&"gamma_fn"), "excluded file was indexed: {names:?}");
        // Every indexed path is a member of the shared universe.
        assert!(idx.entries.iter().all(|e| universe.contains(&e.path)));

        // PSI consumes the SAME universe and sees the same C files (both of them, none more).
        let scan = cauldron_psi::project::scan_files(&dir, &universe);
        let c_files_in_universe = universe
            .iter()
            .filter(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("c") | Some("h")))
            .count();
        assert_eq!(scan.files_indexed, c_files_in_universe);
        assert_eq!(scan.files_indexed, 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Watcher-driven invalidation: `refresh_files` re-extracts JUST the touched files (old
    /// entries dropped, other files untouched), a vanished file's entries disappear, and
    /// `clear` empties the index immediately (project switch — no stale symbols).
    #[test]
    fn refresh_files_updates_only_touched_paths_and_clear_empties() {
        let dir = std::env::temp_dir()
            .join(format!("cauldron-sym-refresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.rs");
        let b = dir.join("b.rs");
        std::fs::write(&a, "pub fn alpha_one() {}\n").unwrap();
        std::fs::write(&b, "pub fn beta_one() {}\n").unwrap();
        let ctx = egui::Context::default();
        let mut idx = SymbolIndex::default();
        let wait = |idx: &mut SymbolIndex| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                idx.poll();
                if !idx.is_building() {
                    break;
                }
                assert!(std::time::Instant::now() < deadline, "index stream timed out");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        };
        idx.rebuild(&[a.clone(), b.clone()], &ctx);
        wait(&mut idx);
        let names = |idx: &SymbolIndex| -> Vec<String> {
            idx.entries.iter().map(|e| e.name.clone()).collect()
        };
        assert!(names(&idx).contains(&"alpha_one".into()));
        assert!(names(&idx).contains(&"beta_one".into()));
        let gen0 = idx.generation();

        // a.rs changes on disk: only ITS symbols are re-extracted; b.rs entries stay put.
        std::fs::write(&a, "pub fn alpha_two() {}\n").unwrap();
        idx.refresh_files(std::slice::from_ref(&a), &ctx);
        assert!(idx.generation() > gen0, "UI caches must invalidate on refresh");
        wait(&mut idx);
        let n = names(&idx);
        assert!(!n.contains(&"alpha_one".into()), "stale symbol survived: {n:?}");
        assert!(n.contains(&"alpha_two".into()));
        assert!(n.contains(&"beta_one".into()), "untouched file lost its symbols");

        // A deleted file's entries vanish (nothing re-extracted for it).
        std::fs::remove_file(&b).unwrap();
        idx.refresh_files(std::slice::from_ref(&b), &ctx);
        wait(&mut idx);
        assert!(!names(&idx).contains(&"beta_one".into()));

        // Project switch: clear drops everything NOW.
        idx.clear();
        assert!(idx.is_empty() && !idx.is_building());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Item 8: for C files the PSI index supersedes the regex tier — a function the line-regex
    /// tier cannot see (indented definition, never opened in any editor) is findable with the
    /// stub's exact file+line; regex duplicates at a PSI-claimed (path, line) are dropped; kind
    /// badges map onto the existing UI set; Rust keeps resolving via the regex tier untouched.
    #[test]
    fn goto_symbol_c_tier_comes_from_psi_and_rust_stays_regex() {
        let dir =
            std::env::temp_dir().join(format!("cauldron-sym-psi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let c_src = "#define BUF_LEN 12\ntypedef int my_len_t;\n  void hidden_fn(void) { }\nvoid shared_fn(void) { }\n";
        let c_path = dir.join("main.c");
        let rs_path = dir.join("lib.rs");
        std::fs::write(&c_path, c_src).unwrap();
        std::fs::write(&rs_path, "pub fn rusty_fn() {}\n").unwrap();

        let ctx = egui::Context::default();
        let mut idx = SymbolIndex::default();
        idx.rebuild(&[c_path.clone(), rs_path.clone()], &ctx);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while idx.is_building() {
            idx.poll();
            assert!(std::time::Instant::now() < deadline, "regex build timed out");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // The regex tier alone misses the indented definition — the PSI tier is the provider.
        assert!(idx.query("hidden_fn", 10).is_empty(), "regex tier should miss hidden_fn");

        let psi = std::sync::Arc::new(cauldron_psi::index::Index::build([(
            c_path.clone(),
            std::sync::Arc::new(cauldron_psi::collect::file_facts(c_src)),
        )]));
        let gen_before = idx.generation();
        idx.sync_psi(&psi);
        assert!(idx.generation() > gen_before, "UI result caches must invalidate on sync");

        // Index-backed hit with the stub's exact file+line, for a file never opened.
        let hits = idx.query("hidden_fn", 10);
        assert_eq!(hits.len(), 1, "{hits:?}", hits = hits.iter().map(|e| &e.name).collect::<Vec<_>>());
        assert_eq!((hits[0].path.as_path(), hits[0].line, hits[0].kind),
            (c_path.as_path(), 2, SymKind::Function));

        // PSI precedence: the regex row for the same (file, line) is dropped, never doubled.
        for (name, line, kind) in [
            ("shared_fn", 3, SymKind::Function),
            ("BUF_LEN", 0, SymKind::Define),
            ("my_len_t", 1, SymKind::TypeDef),
        ] {
            let hits = idx.query(name, 10);
            assert_eq!(hits.len(), 1, "{name} must appear exactly once");
            assert_eq!((hits[0].path.as_path(), hits[0].line, hits[0].kind),
                (c_path.as_path(), line, kind), "{name}");
        }

        // Rust stays on the regex tier.
        let hits = idx.query("rusty_fn", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!((hits[0].path.as_path(), hits[0].kind), (rs_path.as_path(), SymKind::Function));

        // len() counts PSI rows plus unclaimed regex rows: 4 PSI + rusty_fn.
        assert_eq!(idx.len(), 5);

        // PSI parked (standards off / non-C project): honest fallback to the regex rows.
        idx.clear_psi();
        assert!(idx.query("hidden_fn", 10).is_empty());
        assert_eq!(idx.query("shared_fn", 10).len(), 1, "regex row resurfaces after clear_psi");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Item 8: the C tier follows the retained index through the item-4 invalidation lane with
    /// no manual rescan, re-syncing is change-detected by Arc identity + generation (a fresh
    /// full scan restarts at generation 0 and must still land), and prototypes never surface.
    #[test]
    fn psi_tier_tracks_index_mutations_and_new_snapshots() {
        use std::sync::Arc;
        let a = PathBuf::from("/proj/a.c");
        let facts = |src: &str| Arc::new(cauldron_psi::collect::file_facts(src));
        let mut psi = cauldron_psi::index::Index::build([(a.clone(), facts("void one_fn(void) { }\n"))]);
        let mut idx = SymbolIndex::default();
        idx.sync_psi(&Arc::new(psi.clone()));
        assert_eq!(idx.query("one_fn", 10).len(), 1);

        // A save routed through replace_file_facts: the tier updates on the next sync.
        cauldron_psi::invalidate::replace_file_facts(
            &mut psi,
            a.clone(),
            facts("void two_fn(void) { }\n"),
        );
        let arc1 = Arc::new(psi);
        idx.sync_psi(&arc1);
        assert!(idx.query("one_fn", 10).is_empty(), "stale symbol survived the mutation");
        assert_eq!(idx.query("two_fn", 10).len(), 1);

        // Same snapshot again: no-op, no cache churn.
        let gen = idx.generation();
        idx.sync_psi(&arc1);
        assert_eq!(idx.generation(), gen, "identical snapshot must not bump the generation");

        // A FRESH full-scan index restarts at generation 0 — Arc identity still detects it.
        let fresh = Arc::new(cauldron_psi::index::Index::build([(
            a.clone(),
            facts("void proto_fn(void);\nvoid three_fn(void) { }\n"),
        )]));
        assert_eq!(fresh.generation(), 0, "fresh scans restart the stamp");
        idx.sync_psi(&fresh);
        assert!(idx.query("two_fn", 10).is_empty());
        assert_eq!(idx.query("three_fn", 10).len(), 1);
        assert!(idx.query("proto_fn", 10).is_empty(), "prototypes are not goto-symbol targets");
    }

    /// Item 9: `workspace/symbol` rows convert with 0-based lines and mapped kind badges;
    /// non-`file://` locations (macro expansions etc.) are dropped, not mis-pathed.
    #[test]
    fn lsp_symbol_entries_convert_and_drop_non_file_uris() {
        #[allow(deprecated)] // SymbolInformation::deprecated must be filled to construct it.
        let si = |name: &str, kind: lsp_types::SymbolKind, uri: &str, line: u32| {
            lsp_types::SymbolInformation {
                name: name.into(),
                kind,
                tags: None,
                deprecated: None,
                location: lsp_types::Location {
                    uri: lsp_types::Url::parse(uri).unwrap(),
                    range: lsp_types::Range::new(
                        lsp_types::Position::new(line, 0),
                        lsp_types::Position::new(line, 4),
                    ),
                },
                container_name: None,
            }
        };
        let rows = vec![
            si("render", lsp_types::SymbolKind::FUNCTION, "file:///proj/src/lib.rs", 12),
            si("Widget", lsp_types::SymbolKind::STRUCT, "file:///proj/src/lib.rs", 3),
            si("Draw", lsp_types::SymbolKind::INTERFACE, "file:///proj/src/lib.rs", 7),
            si("expanded", lsp_types::SymbolKind::FUNCTION, "rust-analyzer://macro/x", 0),
        ];
        let entries = lsp_symbol_entries(&rows);
        assert_eq!(entries.len(), 3, "non-file URI must be dropped");
        assert_eq!(
            (entries[0].name.as_str(), entries[0].kind, entries[0].line),
            ("render", SymKind::Function, 12)
        );
        assert_eq!(entries[0].path, PathBuf::from("/proj/src/lib.rs"));
        assert_eq!(entries[1].kind, SymKind::Struct);
        assert_eq!(entries[2].kind, SymKind::Trait);
    }

    /// Item 9 precedence: LSP rows win over regex duplicates at the same `(path, line)`, PSI
    /// keeps winning over LSP for C, regex still fills gaps, rows accumulate across multiple
    /// answering servers without doubling, and `clear_lsp` restores the regex rows.
    #[test]
    fn lsp_tier_merges_with_psi_and_regex_by_path_line() {
        use std::sync::Arc;
        let rs = PathBuf::from("/proj/src/lib.rs");
        let c = PathBuf::from("/proj/src/main.c");
        let entry = |name: &str, kind: SymKind, path: &PathBuf, line: usize| SymbolEntry {
            name: name.into(),
            name_lower: name.to_lowercase(),
            kind,
            path: path.clone(),
            line,
        };

        let mut idx = SymbolIndex::default();
        // Regex tier: a Rust fn (line 12), a Rust fn only regex knows (line 30), a C fn (line 4).
        idx.entries = vec![
            entry("render", SymKind::Function, &rs, 12),
            entry("render_gap", SymKind::Function, &rs, 30),
            entry("c_render", SymKind::Function, &c, 4),
        ];
        // PSI tier claims the C definition at its exact line.
        let psi = Arc::new(cauldron_psi::index::Index::build([(
            c.clone(),
            Arc::new(cauldron_psi::collect::file_facts(
                "\n\n\n\nvoid c_render(void) { }\n", // name on line 4
            )),
        )]));
        idx.sync_psi(&psi);

        // Server 1 (rust-analyzer) answers: the same Rust fn at (path, 12) — supersedes regex.
        let gen0 = idx.generation();
        idx.extend_lsp(vec![entry("render", SymKind::Function, &rs, 12)]);
        assert!(idx.generation() > gen0, "result caches must invalidate when rows land");
        let hits = idx.query("render", 10);
        let got: Vec<(&str, &Path, usize)> =
            hits.iter().map(|e| (e.name.as_str(), e.path.as_path(), e.line)).collect();
        // Exactly one row per (path, line): LSP's render@12, PSI's c_render@4, regex render_gap@30.
        assert_eq!(got.len(), 3, "{got:?}");
        assert_eq!(got.iter().filter(|(n, _, _)| *n == "render").count(), 1);
        assert!(got.contains(&("c_render", c.as_path(), 4)));
        assert!(got.contains(&("render_gap", rs.as_path(), 30)));

        // Server 2 (clangd) answers the SAME query with the C fn PSI already claims, plus a
        // duplicate of server 1's row: neither may double.
        idx.extend_lsp(vec![
            entry("c_render", SymKind::Function, &c, 4),
            entry("render", SymKind::Function, &rs, 12),
        ]);
        let hits = idx.query("render", 10);
        assert_eq!(hits.len(), 3, "cross-server duplicates must not double rows");
        // PSI still wins for C: the surviving c_render row is PSI's (queried through psi tier).
        assert_eq!(idx.len(), 3);

        // Query moved on: the tier drops, regex rows resurface, caches invalidate.
        let gen1 = idx.generation();
        idx.clear_lsp();
        assert!(idx.generation() > gen1);
        let hits = idx.query("render", 10);
        assert_eq!(hits.len(), 3, "regex rows must resurface after clear_lsp");
        assert!(hits.iter().any(|e| e.name == "render" && e.line == 12));
        idx.clear_lsp(); // idempotent, no generation churn
        let gen2 = idx.generation();
        idx.clear_lsp();
        assert_eq!(idx.generation(), gen2);
    }

    /// Regression (issue #2 review): tier mutations while a build stream is inflight must not
    /// orphan it. `sync_psi` / `extend_lsp` / `clear_lsp` / `clear_psi` bump only the UI cache
    /// generation — the stream stamp is separate — so the remaining batches still land and
    /// `Done` still clears `building`. Previously one shared counter meant a mid-build
    /// Ctrl+Alt+N sync wedged `building` forever, killing every future rebuild.
    #[test]
    fn mid_build_tier_sync_does_not_orphan_the_stream() {
        use std::sync::Arc;
        let dir = std::env::temp_dir()
            .join(format!("cauldron-sym-midbuild-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.rs");
        std::fs::write(&a, "pub fn streamed_fn() {}\n").unwrap();
        let ctx = egui::Context::default();
        let mut idx = SymbolIndex::default();
        idx.rebuild(std::slice::from_ref(&a), &ctx);
        assert!(idx.is_building());

        // The goto-symbol overlay syncs the PSI/LSP tiers while the stream is inflight.
        let psi = Arc::new(cauldron_psi::index::Index::build([(
            PathBuf::from("/proj/x.c"),
            Arc::new(cauldron_psi::collect::file_facts("void c_fn(void) { }\n")),
        )]));
        idx.sync_psi(&psi);
        idx.extend_lsp(vec![SymbolEntry {
            name: "lsp_fn".into(),
            name_lower: "lsp_fn".into(),
            kind: SymKind::Function,
            path: PathBuf::from("/proj/l.rs"),
            line: 0,
        }]);
        idx.clear_lsp();
        idx.clear_psi();

        // The stream still completes: batches land and Done clears `building`.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while idx.is_building() {
            idx.poll();
            assert!(
                std::time::Instant::now() < deadline,
                "building wedged: mid-build tier sync orphaned the stream"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let names: Vec<&str> = idx.entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"streamed_fn"), "inflight batches were dropped: {names:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lang_of_maps_extensions() {
        assert_eq!(Lang::of(Path::new("a.rs")), Some(Lang::Rust));
        assert_eq!(Lang::of(Path::new("a.hpp")), Some(Lang::C));
        assert_eq!(Lang::of(Path::new("a.pyi")), Some(Lang::Python));
        assert_eq!(Lang::of(Path::new("a.tsx")), Some(Lang::Js));
        assert_eq!(Lang::of(Path::new("a.md")), None);
        assert_eq!(Lang::of(Path::new("Makefile")), None);
    }
}
