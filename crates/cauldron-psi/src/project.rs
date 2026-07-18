//! Project-level entry point: walk a workspace, extract [`FileFacts`] in parallel, build the
//! call graph, and return IDE-shaped Rule-1 findings (absolute paths + line numbers, ready for
//! a Problems panel / CLI report). This is the one call the IDE service and the future
//! cauldron-lint CLI both share.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rayon::prelude::*;

use crate::collect::{self, FileFacts};
use crate::index::Index;

/// Directory-name fragments partitioned OUT of the scan: cFS-style unit-test stubs deliberately
/// duplicate real function names and would create phantom SCCs (docs/psi-design.md).
const PARTITION: &[&str] = &["ut-stubs", "ut_assert", "unit-test", "coveragetest", "ut-coverage", "build"];

/// One hop of a witness cycle, IDE-shaped: `func` calls the next hop's function at `file:line`.
#[derive(Debug, Clone)]
pub struct Rule1Hop {
    pub func: String,
    /// Absolute path, ready for open-file.
    pub file: PathBuf,
    pub offset: usize,
    /// 0-based line of `offset` (panels usually print 1-based).
    pub line: usize,
    /// The dominating re-entry-guard condition at this call site, when one is recognized
    /// (e.g. cFE's `CFE_SB_RequestToSendEvent(...) == CFE_SB_GRANTED`).
    pub guard: Option<String>,
}

/// One whole-program Rule-1 (no recursion) finding: an SCC of the Tier-1 call graph.
#[derive(Debug, Clone)]
pub struct Rule1Finding {
    /// Display names of every SCC member (macros carry a " [macro]" suffix).
    pub members: Vec<String>,
    pub hops: Vec<Rule1Hop>,
    /// The cycle does NOT survive macro expansion — it exists only in the raw macro-reference
    /// graph (C never re-expands a name already being expanded, so a macro cycle cannot run).
    /// Advice weight; a cycle that survives collapse is a real function cycle at error weight.
    ///
    /// This is deliberately NOT "every member is a macro": `OS_printf` reaches itself through
    /// BUGCHECK_VOID -> BUGCHECK -> BUGREPORT, three macros, and that recursion is REAL.
    /// Only expansion can tell the two apart.
    pub macro_textual: bool,
    /// The cycle needs an edge expanded through a macro with CONFLICTING definitions, so it
    /// exists only in the union of mutually exclusive build configs. Not a cycle any compiler
    /// emits; `config_macros` names the macros to blame.
    pub config_dependent: bool,
    pub config_macros: Vec<String>,
    /// At least one edge of the cycle sits behind a recognized re-entry guard — the recursion
    /// is bounded by design (still a PoT Rule-1 finding; reported with the guard cited).
    pub guarded: bool,
    /// Every witness file lives under host-side TOOLING (tools/, scripts/, generators/,
    /// examples/, …) rather than shipping code. Rendered softer: recursion in a code
    /// generator is worth knowing, recursion in flight code is the alarm.
    pub tooling: bool,
}

/// Result of one project scan: the RETAINED index plus the Rule-1 findings queried from it.
/// Findings are no longer the only survivor — `index` outlives the scan and serves
/// defs/callers/ident lookups (docs/psi-design.md).
pub struct ProjectScan {
    pub findings: Vec<Rule1Finding>,
    /// The retained project index (path<->FileId table, per-file facts + hashes, inverted maps).
    pub index: Arc<Index>,
    pub files_indexed: usize,
    pub files_skipped: usize,
    pub elapsed: Duration,
}

/// Scan an externally supplied file universe (ABSOLUTE paths — the IDE hands over its canonical
/// workspace walk, so PSI can never disagree with the tree/search/symbols about which files
/// exist). Non-C files are ignored; the ut-stub PARTITION is applied here because it is a PSI
/// concern (phantom SCCs), not a workspace-visibility one. This is the canonical entry;
/// [`scan_project`] is the self-walking fallback for CLIs (psi-spike, cauldron-lint).
pub fn scan_files(root: &Path, universe: &[PathBuf]) -> ProjectScan {
    let started = Instant::now();
    let (kept, skipped) = keep_c_sources(root, universe.iter().cloned(), &[]);
    scan_kept(root, kept, skipped, started)
}

/// Self-walking fallback: gitignore-respecting walk of `root` (hidden shown, `.git` skipped),
/// then the same C-source filter as [`scan_files`], with caller-supplied `extra_excludes`
/// (workspace-relative) folded into the partition. Pure + synchronous: callers own the
/// threading. Prefer [`scan_files`] when a canonical file universe already exists.
pub fn scan_project(root: &Path, extra_excludes: &[PathBuf]) -> ProjectScan {
    let started = Instant::now();
    let (kept, skipped) = project_files(root, extra_excludes);
    scan_kept(root, kept, skipped, started)
}

/// The self-walk fallback's file producer: walk `root` and return the kept *.c / *.h list
/// (sorted for deterministic FileIds) plus the count partitioned out. Public so the psi-spike
/// harness reuses the exact scan universe instead of re-walking with its own rules.
pub fn project_files(root: &Path, extra_excludes: &[PathBuf]) -> (Vec<PathBuf>, usize) {
    let mut all: Vec<PathBuf> = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build();
    for entry in walker {
        let Ok(e) = entry else { continue };
        if e.file_type().map(|t| t.is_file()).unwrap_or(false) {
            all.push(e.path().to_path_buf());
        }
    }
    keep_c_sources(root, all.into_iter(), extra_excludes)
}

/// Would `path` survive the scan's C-source filter (extension + ut-stub PARTITION +
/// `extra_excludes`)? The IDE's incremental save path asks this before routing a file to
/// [`crate::invalidate::replace_file_facts`] — a file the scan would drop can never affect the
/// index, so its saves are absorbed as no-ops.
pub fn is_scan_source(root: &Path, path: &Path, extra_excludes: &[PathBuf]) -> bool {
    !keep_c_sources(root, std::iter::once(path.to_path_buf()), extra_excludes).0.is_empty()
}

/// ONE exclusion filter for every scan path: keep *.c / *.h, drop (and count) files whose
/// root-relative path hits the ut-stub PARTITION or a caller-supplied exclude prefix.
/// Kept files are sorted so FileId assignment is deterministic across runs.
fn keep_c_sources(
    root: &Path,
    files: impl Iterator<Item = PathBuf>,
    extra_excludes: &[PathBuf],
) -> (Vec<PathBuf>, usize) {
    let mut kept: Vec<PathBuf> = Vec::new();
    let mut skipped = 0usize;
    for path in files {
        let ext = path.extension().and_then(|x| x.to_str()).unwrap_or("");
        if ext != "c" && ext != "h" {
            continue;
        }
        let rel = path.strip_prefix(root).unwrap_or(&path);
        let partitioned = rel.iter().any(|comp| {
            let c = comp.to_string_lossy();
            PARTITION.iter().any(|p| c.starts_with(p))
        }) || extra_excludes.iter().any(|x| rel.starts_with(x));
        if partitioned {
            skipped += 1;
        } else {
            kept.push(path);
        }
    }
    kept.sort();
    (kept, skipped)
}

/// Shared back half of every scan: rayon extraction -> retained [`Index`] -> Rule-1 findings
/// queried from it.
fn scan_kept(root: &Path, kept: Vec<PathBuf>, skipped: usize, started: Instant) -> ProjectScan {
    // Parallel extraction. rayon's collect preserves input order, so the sorted kept list yields
    // deterministic FileIds.
    let extracted: Vec<(PathBuf, Arc<FileFacts>)> = kept
        .par_iter()
        .filter_map(|p| {
            let text = std::fs::read_to_string(p).ok()?;
            Some((p.clone(), Arc::new(collect::file_facts(&text))))
        })
        .collect();

    let index = Arc::new(Index::build(extracted));
    let findings = rule1_findings(&index, root);
    ProjectScan {
        findings,
        files_indexed: index.file_count(),
        index,
        files_skipped: skipped,
        elapsed: started.elapsed(),
    }
}

/// Rule-1 (no recursion) as a QUERY over the retained index: build the derived call graph, run
/// Tarjan, shape each Tier-1 SCC into an IDE-ready finding. Witness lines/guards come from the
/// hop files' text, re-read on demand (only witness files — a handful at most).
pub fn rule1_findings(index: &Index, root: &Path) -> Vec<Rule1Finding> {
    rule1_findings_with(index, root, &|_| None)
}

/// Overlay-aware variant (item 7): when a file's facts came from an UNSAVED buffer, its witness
/// offsets are buffer coordinates — `overlay_text` supplies that live text so hop lines and
/// guard detection match what the user actually sees; everything else falls back to disk.
pub fn rule1_findings_with(
    index: &Index,
    root: &Path,
    overlay_text: &dyn Fn(&Path) -> Option<String>,
) -> Vec<Rule1Finding> {
    let graph = index.call_graph();
    let mut texts: HashMap<PathBuf, Option<String>> = HashMap::new();
    let mut text_of = |path: &PathBuf| -> Option<String> {
        texts
            .entry(path.clone())
            .or_insert_with(|| overlay_text(path).or_else(|| std::fs::read_to_string(path).ok()))
            .clone()
    };

    graph
        .tier1_findings()
        .into_iter()
        .map(|f| {
            let macro_textual = f.kind == crate::graph::CycleKind::MacroTextual;
            let config_dependent = f.kind == crate::graph::CycleKind::ConfigDependent;
            let config_macros = f.config_macros.clone();
            let hops = f
                .witness
                .iter()
                .map(|h| {
                    let file = PathBuf::from(&h.file);
                    let text = text_of(&file);
                    let line = text
                        .as_deref()
                        .map(|t| t[..h.offset.min(t.len())].matches('\n').count())
                        .unwrap_or(0);
                    let guard = text
                        .as_deref()
                        .and_then(|t| collect::guard_condition_at(t, h.offset));
                    Rule1Hop { func: h.func.clone(), file, offset: h.offset, line, guard }
                })
                .collect::<Vec<Rule1Hop>>();
            let guarded = hops.iter().any(|h| h.guard.is_some());
            let tooling = !hops.is_empty()
                && hops.iter().all(|h| {
                    h.file
                        .strip_prefix(root)
                        .map(|rel| is_tooling_path(rel))
                        .unwrap_or(false)
                });
            Rule1Finding {
                members: f.members,
                hops,
                macro_textual,
                config_dependent,
                config_macros,
                guarded,
                tooling,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_finds_seeded_cross_file_recursion() {
        let dir = std::env::temp_dir().join(format!("cauldron-psi-proj-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("ut-stubs")).unwrap();
        std::fs::write(dir.join("a.c"), "void g(void);\nvoid f(void) { g(); }\n").unwrap();
        std::fs::write(dir.join("b.c"), "void f(void);\nvoid g(void) { f(); }\n").unwrap();
        // A same-named decoy inside the partition must NOT contaminate the graph.
        std::fs::write(dir.join("ut-stubs/a.c"), "void f(void) { }\n").unwrap();
        let scan = scan_project(&dir, &[]);
        assert_eq!(scan.files_indexed, 2);
        assert_eq!(scan.files_skipped, 1);
        assert_eq!(scan.findings.len(), 1, "exactly the f<->g cycle: {:?}", scan.findings);
        let f = &scan.findings[0];
        assert!(!f.macro_textual);
        assert_eq!(f.hops.len(), 2);
        assert!(f.hops.iter().all(|h| h.line == 1), "calls are on line 2 (0-based 1)");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_files_universe_is_authoritative() {
        let dir = std::env::temp_dir().join(format!("cauldron-psi-univ-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("ut-stubs")).unwrap();
        std::fs::write(dir.join("a.c"), "void g(void);\nvoid f(void) { g(); }\n").unwrap();
        std::fs::write(dir.join("b.c"), "void f(void);\nvoid g(void) { f(); }\n").unwrap();
        std::fs::write(dir.join("ut-stubs/a.c"), "void f(void) { }\n").unwrap();
        std::fs::write(dir.join("notes.md"), "not C\n").unwrap();
        // The caller-supplied universe OMITS b.c (even though it exists on disk) and includes a
        // partitioned stub + a non-C file: only a.c must be indexed, the stub counted skipped.
        let universe =
            vec![dir.join("a.c"), dir.join("ut-stubs/a.c"), dir.join("notes.md")];
        let scan = scan_files(&dir, &universe);
        assert_eq!(scan.files_indexed, 1, "only the universe's unpartitioned C file");
        assert_eq!(scan.files_skipped, 1, "the ut-stub from the universe");
        assert!(scan.findings.is_empty(), "no cycle without b.c: {:?}", scan.findings);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_scan_source_mirrors_the_scan_filter() {
        let root = Path::new("/proj");
        assert!(is_scan_source(root, &root.join("apps/foo/src/foo.c"), &[]));
        assert!(is_scan_source(root, &root.join("cfe/inc/cfe.h"), &[]));
        assert!(!is_scan_source(root, &root.join("notes.md"), &[]), "non-C never scanned");
        assert!(!is_scan_source(root, &root.join("ut-stubs/foo.c"), &[]), "partitioned out");
        assert!(!is_scan_source(root, &root.join("build/gen.c"), &[]));
        assert!(
            !is_scan_source(root, &root.join("vendor/x.c"), &[PathBuf::from("vendor")]),
            "extra excludes honored"
        );
    }

    #[test]
    fn extra_excludes_partition_dirs() {
        let dir = std::env::temp_dir().join(format!("cauldron-psi-proj2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("vendor")).unwrap();
        std::fs::write(dir.join("main.c"), "void ok(void) { }\n").unwrap();
        std::fs::write(dir.join("vendor/self.c"), "void s(void) { s(); }\n").unwrap();
        let scan = scan_project(&dir, &[PathBuf::from("vendor")]);
        assert_eq!(scan.files_indexed, 1);
        assert!(scan.findings.is_empty(), "vendored self-recursion excluded");
        let _ = std::fs::remove_dir_all(&dir);
    }
}


/// Host-side tooling classifier: does this workspace-relative path live under a directory
/// that conventionally holds build-time / developer tooling rather than shipping code?
/// Deliberately generic — works for any project layout, nothing product-specific.
pub fn is_tooling_path(rel: &std::path::Path) -> bool {
    rel.iter().any(|comp| {
        let c = comp.to_string_lossy().to_lowercase();
        matches!(
            c.as_str(),
            "tools" | "tool" | "tooling" | "scripts" | "script" | "devtools"
                | "generators" | "generator" | "codegen" | "gen"
                | "examples" | "example" | "samples" | "sample" | "demos" | "demo"
                | "benches" | "benchmarks" | "ci" | "docs"
        )
    })
}

#[cfg(test)]
mod tooling_tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn classifier() {
        assert!(is_tooling_path(Path::new("tools/eds/edslib/x.c")));
        assert!(is_tooling_path(Path::new("scripts/gen.c")));
        assert!(is_tooling_path(Path::new("apps/foo/examples/demo.c")));
        assert!(!is_tooling_path(Path::new("cfe/modules/sb/fsw/src/cfe_sb_priv.c")));
        assert!(!is_tooling_path(Path::new("src/toolbox.c")), "segment match, not substring");
    }
}
