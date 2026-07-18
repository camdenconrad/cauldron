//! The PSI service: a RESIDENT indexer thread that owns the retained [`Index`]. ALL index work
//! — full scans (project open, standards toggle, future watcher bursts), single-file saves, and
//! dirty-buffer overlays — flows through ONE serialized message queue, so save/scan/overlay
//! events can never interleave into a lost update. Full rescans run `scan_files` over the
//! CANONICAL workspace file universe (`Workspace::all_files` — the app passes it in, so PSI
//! never re-walks with divergent rules); a saved file goes through
//! `invalidate::replace_file_facts`, keyed on the retained interface/body hashes: fully
//! identical facts are a no-op, hash-equal-but-moved facts install fresh positions (`Moved` —
//! a comment-only save must not pin witness lines to stale offsets), real changes swap that
//! one file's facts and rebuild the DERIVED layer (call graph + Rule-1 findings) from retained
//! facts — other files are never re-extracted.
//!
//! DIRTY-BUFFER OVERLAYS (item 7, docs/psi-design.md "Indexing pipeline (a)"): a debounced edit
//! ships the LIVE buffer text here; the worker re-collects facts from it and installs them as an
//! overlay SHADOWING the disk facts for that path (disk truth stashed for restore). NASA
//! squiggles therefore update without saving. Save replaces the overlay with the disk-truth
//! update (same queue — they can't fight); close-without-save restores the stashed disk facts.
//! Witness lines/guards for overlaid files are read from the overlay text, never from disk.
//! Dumb-mode honest (docs/psi-design.md): while an update is inflight the panel says
//! "indexing…" — it never shows stale whole-program answers as fresh.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{mpsc, Arc};
use std::time::Instant;

use cauldron_psi::collect::{self, FileFacts};
use cauldron_psi::index::Index;
use cauldron_psi::invalidate::{self, Invalidation};
use cauldron_psi::project::{self, rule1_findings_with, scan_files, Rule1Finding};

pub enum PsiState {
    /// No C sources in the workspace — the NASA layer stays out of the way.
    NotCProject,
    Indexing,
    Ready {
        /// The retained index snapshot: defs/callers/ident lookups for future consumers
        /// (find-usages, goto-def), generation-stamped by the incremental invalidation.
        index: Arc<Index>,
        findings: Vec<Rule1Finding>,
        files_indexed: usize,
        elapsed_ms: u128,
    },
}

/// One unit of work for the resident indexer. The single mpsc queue serializes full scans and
/// single-file updates in kick order — the seq/generation discipline needs exactly one lane.
enum IndexerMsg {
    FullScan { seq: u64, root: PathBuf, files: Vec<PathBuf>, ctx: egui::Context },
    /// `external` = the disk change did NOT come from an IDE save of the current buffer (the
    /// app passes "an open buffer for `path` is dirty"): a live overlay then stays
    /// authoritative — its disk stash is refreshed but the buffer facts stay installed.
    FileSaved { seq: u64, root: PathBuf, path: PathBuf, external: bool, ctx: egui::Context },
    /// Debounced dirty-buffer update: facts re-collected from `text` (the LIVE buffer) shadow
    /// the disk facts for `path` until save converges them or the buffer closes without saving.
    Overlay { seq: u64, root: PathBuf, path: PathBuf, text: String, ctx: egui::Context },
    /// Buffer closed WITHOUT save: drop `path`'s overlay and restore the stashed disk facts.
    BufferClosed { seq: u64, root: PathBuf, path: PathBuf, ctx: egui::Context },
    /// Project switch / standards-off: forget the retained index AND every overlay — the next
    /// FullScan must not re-apply another project's (or a stale) buffer shadow. Sends no result.
    Reset,
}

/// One finished unit of work, stamped with the seq of the kick that requested it.
struct IndexerResult {
    seq: u64,
    index: Arc<Index>,
    findings: Vec<Rule1Finding>,
    files_indexed: usize,
    elapsed_ms: u128,
}

pub struct PsiService {
    pub state: PsiState,
    rx: Receiver<IndexerResult>,
    work_tx: Sender<IndexerMsg>,
    /// Generation guard: results from a superseded kick are dropped.
    scan_seq: u64,
    inflight_seq: Option<u64>,
}

impl PsiService {
    pub fn new() -> Self {
        let (work_tx, work_rx) = mpsc::channel();
        let (res_tx, res_rx) = mpsc::channel();
        // The resident indexer lives for the whole session; it parks in recv() between kicks
        // and exits when the service (and thus work_tx) is dropped.
        std::thread::Builder::new()
            .name("cauldron-psi-index".into())
            .spawn(move || indexer_loop(work_rx, res_tx))
            .ok();
        Self { state: PsiState::NotCProject, rx: res_rx, work_tx, scan_seq: 0, inflight_seq: None }
    }

    /// Kick a FULL background scan over `files` — the canonical workspace file universe
    /// (absolute paths from `Workspace::all_files`; `.idea` excludes already applied by the
    /// producer). The fallback path for project open, standards toggles, and anything the
    /// incremental lane can't absorb. No-op degrade: if the workspace has no C files the state
    /// parks at NotCProject.
    pub fn rescan(&mut self, root: &Path, files: &[PathBuf], has_c: bool, ctx: &egui::Context) {
        // Bump even when degrading: an inflight result from the previous project/toggle must
        // not land on top of NotCProject.
        self.scan_seq += 1;
        if !has_c {
            self.inflight_seq = None;
            self.state = PsiState::NotCProject;
            // The worker must forget too: retained overlays would go stale while we're off
            // (no edits shipped in NotCProject) and replay into a later scan.
            let _ = self.work_tx.send(IndexerMsg::Reset);
            return;
        }
        self.inflight_seq = Some(self.scan_seq);
        self.state = PsiState::Indexing;
        let _ = self.work_tx.send(IndexerMsg::FullScan {
            seq: self.scan_seq,
            root: root.to_path_buf(),
            files: files.to_vec(),
            ctx: ctx.clone(),
        });
    }

    /// A file changed on disk (IDE save or watcher-observed external write): route it through
    /// the INCREMENTAL path (single-file re-extract + `replace_file_facts` on the resident
    /// indexer). `external` = the change did NOT converge an open dirty buffer (the app passes
    /// "a buffer for `path` is open and dirty") — the worker then keeps a live overlay
    /// authoritative instead of clobbering it with disk facts. Returns false when the service
    /// can't take it (PSI off / no scan ever kicked) — the caller falls back to scheduling a
    /// full rescan. Returns true-with-nothing-queued for files the scan filter would drop
    /// anyway (non-C, ut-stub partition, excluded dirs): they can never affect the index.
    pub fn file_saved(
        &mut self,
        root: &Path,
        path: &Path,
        excludes: &[PathBuf],
        external: bool,
        ctx: &egui::Context,
    ) -> bool {
        match self.state {
            // Ready, or Indexing behind an already-queued full scan: the serialized queue
            // guarantees the indexer holds a retained index by the time this save is processed.
            PsiState::Ready { .. } | PsiState::Indexing => {}
            PsiState::NotCProject => return false,
        }
        if !project::is_scan_source(root, path, excludes) {
            return true;
        }
        self.scan_seq += 1;
        self.inflight_seq = Some(self.scan_seq);
        self.state = PsiState::Indexing;
        let _ = self.work_tx.send(IndexerMsg::FileSaved {
            seq: self.scan_seq,
            root: root.to_path_buf(),
            path: path.to_path_buf(),
            external,
            ctx: ctx.clone(),
        });
        true
    }

    /// A dirty buffer settled (debounced edit): ship its LIVE text to the worker, which collects
    /// facts from it and installs them as an OVERLAY shadowing the disk facts. Same gating
    /// contract as [`PsiService::file_saved`]: false = can't take it (schedule a full rescan),
    /// true covers both "queued" and "can never affect the index".
    pub fn buffer_edited(
        &mut self,
        root: &Path,
        path: &Path,
        excludes: &[PathBuf],
        text: String,
        ctx: &egui::Context,
    ) -> bool {
        match self.state {
            PsiState::Ready { .. } | PsiState::Indexing => {}
            PsiState::NotCProject => return false,
        }
        if !project::is_scan_source(root, path, excludes) {
            return true;
        }
        self.scan_seq += 1;
        self.inflight_seq = Some(self.scan_seq);
        self.state = PsiState::Indexing;
        let _ = self.work_tx.send(IndexerMsg::Overlay {
            seq: self.scan_seq,
            root: root.to_path_buf(),
            path: path.to_path_buf(),
            text,
            ctx: ctx.clone(),
        });
        true
    }

    /// A buffer closed (its last view, saved or not): tell the worker so a live overlay for the
    /// path is dropped and disk truth restored. No-overlay closes are absorbed by the worker
    /// (cheap round-trip); non-index files and NotCProject need nothing at all — with no index
    /// there is no overlay to drop, so unlike the edit/save lanes there is NO rescan fallback.
    pub fn buffer_closed(&mut self, root: &Path, path: &Path, excludes: &[PathBuf], ctx: &egui::Context) {
        match self.state {
            PsiState::Ready { .. } | PsiState::Indexing => {}
            PsiState::NotCProject => return,
        }
        if !project::is_scan_source(root, path, excludes) {
            return;
        }
        self.scan_seq += 1;
        self.inflight_seq = Some(self.scan_seq);
        self.state = PsiState::Indexing;
        let _ = self.work_tx.send(IndexerMsg::BufferClosed {
            seq: self.scan_seq,
            root: root.to_path_buf(),
            path: path.to_path_buf(),
            ctx: ctx.clone(),
        });
    }

    /// Drop the current snapshot + orphan any inflight result NOW (project switch): switching
    /// workspaces must never show the OLD project's findings/index, not even for the frames
    /// until the follow-up rescan is drained. The caller schedules that rescan (which parks the
    /// state at NotCProject if the new root has no C layer). The worker forgets its retained
    /// index and overlays too — the next FullScan must not re-shadow with another project's
    /// buffers.
    pub fn invalidate(&mut self) {
        self.scan_seq += 1;
        self.inflight_seq = None;
        self.state = PsiState::Indexing;
        let _ = self.work_tx.send(IndexerMsg::Reset);
    }

    /// Standards toggled off: park at NotCProject and make the worker forget everything —
    /// overlays kept across an "off" period would replay STALE buffer text into the next scan
    /// (edits made while off are never shipped here).
    pub fn disable(&mut self) {
        self.scan_seq += 1;
        self.inflight_seq = None;
        self.state = PsiState::NotCProject;
        let _ = self.work_tx.send(IndexerMsg::Reset);
    }

    /// Drain finished work (call once per frame). Only the LATEST kick lands: results are
    /// seq-stamped, so anything superseded while it was being computed is dropped.
    pub fn pump(&mut self) {
        while let Ok(res) = self.rx.try_recv() {
            if self.inflight_seq == Some(res.seq) {
                self.inflight_seq = None;
                self.state = PsiState::Ready {
                    index: res.index,
                    findings: res.findings,
                    files_indexed: res.files_indexed,
                    elapsed_ms: res.elapsed_ms,
                };
            }
        }
    }

    /// The retained index snapshot, when the last scan finished. Cheap Arc clone for consumers
    /// (defs/callers/ident queries — the find-usages fallback wraps it in a
    /// `cauldron_psi::query::PsiSnapshot`); None while indexing or on non-C projects.
    pub fn index(&self) -> Option<Arc<Index>> {
        match &self.state {
            PsiState::Ready { index, .. } => Some(Arc::clone(index)),
            _ => None,
        }
    }

    /// Status-bar segment, or None when the project has no C layer.
    pub fn status(&self) -> Option<String> {
        match &self.state {
            PsiState::NotCProject => None,
            PsiState::Indexing => Some("PSI: indexing…".into()),
            PsiState::Ready { findings, .. } => {
                let n = findings.iter().filter(|f| !f.macro_textual).count();
                Some(if n == 0 { "PSI: rule-1 clean".into() } else { format!("PSI: {n} recursion") })
            }
        }
    }
}

/// One live buffer shadow held by the worker (path is the map key).
struct Overlay {
    /// The disk-truth facts this overlay shadows — restored on close-without-save. None = the
    /// path wasn't in the index when first overlaid (close retracts it entirely).
    disk: Option<Arc<FileFacts>>,
    /// The buffer-derived facts currently installed (re-applied across full rescans).
    facts: Arc<FileFacts>,
    /// The exact buffer text `facts` came from: witness lines/guards for this file must be read
    /// from HERE (buffer coordinates), never from disk.
    text: Arc<String>,
}

/// Rule-1 findings with overlaid files' witness text coming from their live buffers.
fn findings_over(
    index: &Index,
    root: &Path,
    overlays: &HashMap<PathBuf, Overlay>,
) -> Vec<Rule1Finding> {
    rule1_findings_with(index, root, &|p: &Path| overlays.get(p).map(|o| (*o.text).clone()))
}

/// The resident indexer: owns the retained index + last findings + live buffer overlays between
/// kicks and processes work strictly in kick order (one queue = no lost updates between
/// save/scan/overlay events).
fn indexer_loop(work: Receiver<IndexerMsg>, out: Sender<IndexerResult>) {
    // `Arc` so results share the snapshot with the UI thread; `Arc::make_mut` copies-on-write
    // when the UI still holds the previous snapshot (per-file facts stay shared).
    let mut retained: Option<(Arc<Index>, Vec<Rule1Finding>)> = None;
    let mut overlays: HashMap<PathBuf, Overlay> = HashMap::new();
    while let Ok(msg) = work.recv() {
        match msg {
            IndexerMsg::Reset => {
                retained = None;
                overlays.clear();
            }
            IndexerMsg::FullScan { seq, root, files, ctx } => {
                let scan = scan_files(&root, &files);
                let mut index = scan.index;
                let mut findings = scan.findings;
                if !overlays.is_empty() {
                    // Dirty buffers stay authoritative across full rescans (standards toggle,
                    // watcher burst): re-shadow each one, refreshing its disk stash from the
                    // fresh scan, then recompute findings with the overlay texts visible.
                    for (path, ov) in overlays.iter_mut() {
                        ov.disk =
                            index.file_id(path).and_then(|fid| index.facts(fid)).map(Arc::clone);
                        invalidate::overlay_file_facts(
                            Arc::make_mut(&mut index),
                            path.clone(),
                            Arc::clone(&ov.facts),
                        );
                    }
                    findings = findings_over(&index, &root, &overlays);
                }
                retained = Some((Arc::clone(&index), findings.clone()));
                let _ = out.send(IndexerResult {
                    seq,
                    files_indexed: index.file_count(),
                    index,
                    findings,
                    elapsed_ms: scan.elapsed.as_millis(),
                });
                ctx.request_repaint();
            }
            IndexerMsg::FileSaved { seq, root, path, external, ctx } => {
                let started = Instant::now();
                let Some((index, findings)) = retained.as_mut() else {
                    // Unreachable by construction: the app only routes saves here once a full
                    // scan has been queued ahead of them. Nothing retained = nothing to update.
                    debug_assert!(false, "FileSaved before any FullScan");
                    continue;
                };
                if external && overlays.contains_key(&path) {
                    // External disk change UNDER a live dirty buffer (watcher lane): the
                    // buffer stays authoritative — same contract as the FullScan re-shadow.
                    // Keep the overlay facts installed; only refresh the disk stash so a
                    // later close-without-save restores the NEW disk truth.
                    if let Ok(text) = std::fs::read_to_string(&path) {
                        if let Some(ov) = overlays.get_mut(&path) {
                            ov.disk = Some(Arc::new(collect::file_facts(&text)));
                        }
                    }
                    let _ = out.send(IndexerResult {
                        seq,
                        index: Arc::clone(index),
                        findings: findings.clone(),
                        files_indexed: index.file_count(),
                        elapsed_ms: started.elapsed().as_millis(),
                    });
                    ctx.request_repaint();
                    continue;
                }
                // Save converges buffer and disk: the overlay is superseded by disk truth
                // (item-7 contract — the single queue serializes this against edit debounces).
                let had_overlay = overlays.remove(&path).is_some();
                // Re-extract JUST the saved file; the two-hash compare decides the blast radius.
                let outcome = match std::fs::read_to_string(&path) {
                    Ok(text) => invalidate::replace_file_facts(
                        Arc::make_mut(index),
                        path,
                        Arc::new(collect::file_facts(&text)),
                    ),
                    // Unreadable right after a save (raced delete?): keep the retained facts.
                    Err(_) => Invalidation::Unchanged,
                };
                if outcome.changed() || had_overlay {
                    // Incremental facts, FULL derived rebuild: graph + findings recomputed from
                    // retained facts. Never patch edges incrementally — a stale edge means
                    // phantom recursion findings, worse than a ms-scale rebuild. A dropped
                    // overlay also forces the rebuild: witness text moves back to disk.
                    *findings = findings_over(index.as_ref(), &root, &overlays);
                }
                let _ = out.send(IndexerResult {
                    seq,
                    index: Arc::clone(index),
                    findings: findings.clone(),
                    files_indexed: index.file_count(),
                    elapsed_ms: started.elapsed().as_millis(),
                });
                ctx.request_repaint();
            }
            IndexerMsg::Overlay { seq, root, path, text, ctx } => {
                let started = Instant::now();
                let Some((index, findings)) = retained.as_mut() else {
                    debug_assert!(false, "Overlay before any FullScan");
                    continue;
                };
                // Full re-collect from the live buffer text — on THIS worker, never the UI
                // thread (collect.rs is ms-scale per file, see docs/psi-spike.md).
                let facts = Arc::new(collect::file_facts(&text));
                // First overlay for the path stashes disk truth; later ticks keep the original
                // stash (the index currently holds the PREVIOUS overlay, not disk).
                let disk = match overlays.remove(&path) {
                    Some(prev) => prev.disk,
                    None => index.file_id(&path).and_then(|fid| index.facts(fid)).map(Arc::clone),
                };
                invalidate::overlay_file_facts(
                    Arc::make_mut(index),
                    path.clone(),
                    Arc::clone(&facts),
                );
                overlays.insert(path, Overlay { disk, facts, text: Arc::new(text) });
                // Always rebuild: even hash-equal ticks moved offsets (overlay_file_facts
                // installed them), and the witness text just changed.
                *findings = findings_over(index.as_ref(), &root, &overlays);
                let _ = out.send(IndexerResult {
                    seq,
                    index: Arc::clone(index),
                    findings: findings.clone(),
                    files_indexed: index.file_count(),
                    elapsed_ms: started.elapsed().as_millis(),
                });
                ctx.request_repaint();
            }
            IndexerMsg::BufferClosed { seq, root, path, ctx } => {
                let started = Instant::now();
                let Some((index, findings)) = retained.as_mut() else {
                    // A close can race a Reset benignly (queued before the switch): ignore.
                    continue;
                };
                if let Some(ov) = overlays.remove(&path) {
                    // Close WITHOUT save: restore disk truth (the stashed facts keep their
                    // original identity), or retract entirely if the overlay ADDED the file.
                    // overlay_file_facts, not replace_file_facts: the stash must be installed
                    // even when hashes match (buffer diverged only in offsets), or the index
                    // would keep buffer-coordinate offsets with no buffer to match them.
                    match ov.disk {
                        Some(disk) => {
                            invalidate::overlay_file_facts(Arc::make_mut(index), path, disk);
                        }
                        None => {
                            invalidate::remove_file_facts(Arc::make_mut(index), &path);
                        }
                    }
                    *findings = findings_over(index.as_ref(), &root, &overlays);
                }
                let _ = out.send(IndexerResult {
                    seq,
                    index: Arc::clone(index),
                    findings: findings.clone(),
                    files_indexed: index.file_count(),
                    elapsed_ms: started.elapsed().as_millis(),
                });
                ctx.request_repaint();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn wait_ready(svc: &mut PsiService) -> (Arc<Index>, Vec<Rule1Finding>) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            svc.pump();
            if let PsiState::Ready { index, findings, .. } = &svc.state {
                return (Arc::clone(index), findings.clone());
            }
            assert!(Instant::now() < deadline, "indexer did not finish in time");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// End-to-end over the resident thread: full scan -> body-only save updates findings
    /// incrementally (no re-extraction of other files) -> identical save is a no-op ->
    /// non-scannable saves never reach the queue -> no index yet = full-rescan fallback.
    #[test]
    fn save_routes_through_incremental_invalidation() {
        let dir = std::env::temp_dir().join(format!("cauldron-psi-svc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.c");
        let b = dir.join("b.c");
        std::fs::write(&a, "void g(void);\nvoid f(void) { g(); }\n").unwrap();
        std::fs::write(&b, "void f(void);\nvoid g(void) { f(); }\n").unwrap();
        let ctx = egui::Context::default();
        let mut svc = PsiService::new();

        // Before any scan the incremental lane refuses — caller must full-rescan.
        assert!(!svc.file_saved(&dir, &a, &[], false, &ctx), "no index yet -> fallback");

        svc.rescan(&dir, &[a.clone(), b.clone()], true, &ctx);
        assert!(matches!(svc.state, PsiState::Indexing));
        let (idx0, findings0) = wait_ready(&mut svc);
        assert_eq!(findings0.len(), 1, "seeded f<->g cycle: {findings0:?}");
        let b_facts0 = Arc::clone(idx0.facts(idx0.file_id(&b).unwrap()).unwrap());

        // Body-only save of a.c: the cycle disappears WITHOUT re-extracting b.c.
        std::fs::write(&a, "void g(void);\nvoid f(void) { }\n").unwrap();
        assert!(svc.file_saved(&dir, &a, &[], false, &ctx), "incremental path accepted the save");
        assert!(matches!(svc.state, PsiState::Indexing), "honest while inflight");
        let (idx1, findings1) = wait_ready(&mut svc);
        assert!(findings1.is_empty(), "findings updated incrementally: {findings1:?}");
        assert_eq!(idx1.generation(), idx0.generation() + 1, "one mutation, one bump");
        let b_facts1 = idx1.facts(idx1.file_id(&b).unwrap()).unwrap();
        assert!(Arc::ptr_eq(&b_facts0, b_facts1), "b.c was NOT re-extracted");

        // Saving identical content: hashes match, no-op fast path, generation stays.
        assert!(svc.file_saved(&dir, &a, &[], false, &ctx));
        let (idx2, findings2) = wait_ready(&mut svc);
        assert!(findings2.is_empty());
        assert_eq!(idx2.generation(), idx1.generation(), "no-op must not bump the generation");

        // Non-scannable files are absorbed without an indexer round-trip.
        let seq_before = svc.scan_seq;
        assert!(svc.file_saved(&dir, &dir.join("notes.md"), &[], false, &ctx));
        assert_eq!(svc.scan_seq, seq_before, "non-C save never reaches the queue");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Item 7 end-to-end: a dirty-buffer overlay makes findings reflect the UNSAVED buffer
    /// (witness lines in buffer coordinates, from the overlay text), survives a full rescan,
    /// and close-without-save restores the exact disk facts and disk findings.
    #[test]
    fn overlay_shadows_disk_and_close_without_save_restores() {
        let dir = std::env::temp_dir().join(format!("cauldron-psi-ovl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.c");
        let b = dir.join("b.c");
        // DISK truth: no cycle (f's body is empty).
        std::fs::write(&a, "void g(void);\nvoid f(void) { }\n").unwrap();
        std::fs::write(&b, "void f(void);\nvoid g(void) { f(); }\n").unwrap();
        let ctx = egui::Context::default();
        let mut svc = PsiService::new();

        // Before any scan the overlay lane refuses — caller must full-rescan.
        assert!(!svc.buffer_edited(&dir, &a, &[], "x".into(), &ctx), "no index yet -> fallback");

        svc.rescan(&dir, &[a.clone(), b.clone()], true, &ctx);
        let (idx0, findings0) = wait_ready(&mut svc);
        assert!(findings0.is_empty(), "disk truth is cycle-free: {findings0:?}");
        let a_disk_facts = Arc::clone(idx0.facts(idx0.file_id(&a).unwrap()).unwrap());

        // The user types a call to g() into a.c WITHOUT saving (plus a comment line, so the
        // witness line only matches if it is read from the BUFFER, never from disk).
        let buffer = "// unsaved edit\nvoid g(void);\nvoid f(void) { g(); }\n";
        assert!(svc.buffer_edited(&dir, &a, &[], buffer.into(), &ctx));
        assert!(matches!(svc.state, PsiState::Indexing), "honest while the overlay lands");
        let (idx1, findings1) = wait_ready(&mut svc);
        assert_eq!(findings1.len(), 1, "squiggles update without saving: {findings1:?}");
        assert!(idx1.generation() > idx0.generation(), "overlay is a real index mutation");
        let hop_in_a = findings1[0].hops.iter().find(|h| h.file == a).expect("hop in a.c");
        assert_eq!(hop_in_a.line, 2, "witness line computed from the overlay text");
        assert_eq!(
            std::fs::read_to_string(&a).unwrap(),
            "void g(void);\nvoid f(void) { }\n",
            "disk was never touched"
        );

        // Non-index files absorb without a round-trip; then close a.c WITHOUT saving.
        let seq_before = svc.scan_seq;
        assert!(svc.buffer_edited(&dir, &dir.join("notes.md"), &[], "hi".into(), &ctx));
        assert_eq!(svc.scan_seq, seq_before, "non-C edit never reaches the queue");
        svc.buffer_closed(&dir, &a, &[], &ctx);
        let (idx3, findings3) = wait_ready(&mut svc);
        assert!(findings3.is_empty(), "disk findings restored: {findings3:?}");
        let a_facts3 = idx3.facts(idx3.file_id(&a).unwrap()).unwrap();
        assert!(Arc::ptr_eq(&a_disk_facts, a_facts3), "the EXACT disk facts were restored");

        // Closing again (no overlay left) is absorbed as a cheap no-change round-trip.
        svc.buffer_closed(&dir, &a, &[], &ctx);
        let (idx4, findings4) = wait_ready(&mut svc);
        assert!(findings4.is_empty());
        assert_eq!(idx4.generation(), idx3.generation(), "no-overlay close mutates nothing");

        // Re-overlay, then a full rescan (standards toggle / watcher burst) must NOT lose the
        // live overlay — the dirty buffer stays authoritative until save or close.
        assert!(svc.buffer_edited(&dir, &a, &[], buffer.into(), &ctx));
        let (_, findings5) = wait_ready(&mut svc);
        assert_eq!(findings5.len(), 1);
        svc.rescan(&dir, &[a.clone(), b.clone()], true, &ctx);
        let (_, findings6) = wait_ready(&mut svc);
        assert_eq!(findings6.len(), 1, "overlay survives the full rescan: {findings6:?}");
        svc.buffer_closed(&dir, &a, &[], &ctx);
        let (_, findings7) = wait_ready(&mut svc);
        assert!(findings7.is_empty(), "close after rescan still restores disk truth");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Issue #2 review: an offset-only save (comment typed above a flagged call, Ctrl+S inside
    /// the overlay debounce so no overlay ever shipped) must MOVE the retained positions — the
    /// hash-equal fast path used to keep the pre-edit facts, pinning the witness line/stub
    /// line to stale coordinates forever.
    #[test]
    fn comment_only_save_moves_witness_and_stub_lines() {
        let dir = std::env::temp_dir().join(format!("cauldron-psi-mv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.c");
        let b = dir.join("b.c");
        std::fs::write(&a, "void g(void);\nvoid f(void) { g(); }\n").unwrap();
        std::fs::write(&b, "void f(void);\nvoid g(void) { f(); }\n").unwrap();
        let ctx = egui::Context::default();
        let mut svc = PsiService::new();
        svc.rescan(&dir, &[a.clone(), b.clone()], true, &ctx);
        let (idx0, findings0) = wait_ready(&mut svc);
        assert_eq!(findings0.len(), 1);
        let hop0 = findings0[0].hops.iter().find(|h| h.file == a).expect("hop in a.c");
        assert_eq!(hop0.line, 1);

        // Comment-only edit saved directly to disk: both hashes stay equal, every position
        // shifts down one line.
        std::fs::write(&a, "// note\nvoid g(void);\nvoid f(void) { g(); }\n").unwrap();
        assert!(svc.file_saved(&dir, &a, &[], false, &ctx));
        let (idx1, findings1) = wait_ready(&mut svc);
        assert!(idx1.generation() > idx0.generation(), "Moved is a real index mutation");
        assert_eq!(findings1.len(), 1, "semantics unchanged: {findings1:?}");
        let hop1 = findings1[0].hops.iter().find(|h| h.file == a).expect("hop in a.c");
        assert_eq!(hop1.line, 2, "witness line follows the shifted disk text");
        let a_facts = idx1.facts(idx1.file_id(&a).unwrap()).unwrap();
        let f_stub = a_facts.stubs.iter().find(|s| s.name == "f").unwrap();
        assert_eq!(f_stub.name_line, 2, "goto-symbol stub line follows the shifted text");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Issue #2 review: a watcher-observed EXTERNAL disk change under a dirty open buffer must
    /// not drop the buffer's overlay (squiggles would jump to disk truth while the editor shows
    /// the divergent buffer). The overlay stays authoritative; its disk stash refreshes, so a
    /// later close-without-save restores the NEW disk truth.
    #[test]
    fn external_change_under_dirty_buffer_keeps_overlay_authoritative() {
        let dir = std::env::temp_dir().join(format!("cauldron-psi-ext-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.c");
        let b = dir.join("b.c");
        // DISK truth: no cycle.
        std::fs::write(&a, "void g(void);\nvoid f(void) { }\n").unwrap();
        std::fs::write(&b, "void f(void);\nvoid g(void) { f(); }\n").unwrap();
        let ctx = egui::Context::default();
        let mut svc = PsiService::new();
        svc.rescan(&dir, &[a.clone(), b.clone()], true, &ctx);
        let (_, findings0) = wait_ready(&mut svc);
        assert!(findings0.is_empty());

        // The buffer types the cycle in WITHOUT saving (overlay lands).
        let buffer = "void g(void);\nvoid f(void) { g(); }\n";
        assert!(svc.buffer_edited(&dir, &a, &[], buffer.into(), &ctx));
        let (_, findings1) = wait_ready(&mut svc);
        assert_eq!(findings1.len(), 1, "overlay sees the buffer's cycle");

        // git stash pop / script rewrites a.c on disk while the buffer stays dirty.
        std::fs::write(&a, "void g(void);\nvoid f(void) { }\nvoid extra_fn(void) { }\n").unwrap();
        assert!(svc.file_saved(&dir, &a, &[], true, &ctx), "external watcher lane accepted");
        let (idx2, findings2) = wait_ready(&mut svc);
        assert_eq!(findings2.len(), 1, "dirty buffer stays authoritative: {findings2:?}");
        assert!(
            idx2.defs_by_name("extra_fn").is_empty(),
            "disk facts must not shadow the live overlay"
        );

        // Close WITHOUT save: the refreshed stash restores the NEW disk truth.
        svc.buffer_closed(&dir, &a, &[], &ctx);
        let (idx3, findings3) = wait_ready(&mut svc);
        assert!(findings3.is_empty(), "new disk truth is cycle-free: {findings3:?}");
        assert_eq!(
            idx3.defs_by_name("extra_fn").len(),
            1,
            "close restored the POST-external-change disk facts"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Save replaces the overlay with disk truth (they must not fight): after save, findings
    /// come from disk, and a later close does NOT roll anything back.
    #[test]
    fn save_supersedes_overlay_with_disk_truth() {
        let dir = std::env::temp_dir().join(format!("cauldron-psi-ovs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.c");
        let b = dir.join("b.c");
        std::fs::write(&a, "void g(void);\nvoid f(void) { }\n").unwrap();
        std::fs::write(&b, "void f(void);\nvoid g(void) { f(); }\n").unwrap();
        let ctx = egui::Context::default();
        let mut svc = PsiService::new();
        svc.rescan(&dir, &[a.clone(), b.clone()], true, &ctx);
        let (_, findings0) = wait_ready(&mut svc);
        assert!(findings0.is_empty());

        // Type the cycle into the buffer (overlay), then SAVE it: buffer and disk converge.
        let text = "void g(void);\nvoid f(void) { g(); }\n";
        assert!(svc.buffer_edited(&dir, &a, &[], text.into(), &ctx));
        let (_, findings1) = wait_ready(&mut svc);
        assert_eq!(findings1.len(), 1, "overlay sees the cycle pre-save");
        std::fs::write(&a, text).unwrap();
        assert!(svc.file_saved(&dir, &a, &[], false, &ctx));
        let (_, findings2) = wait_ready(&mut svc);
        assert_eq!(findings2.len(), 1, "disk truth now carries the cycle");

        // The overlay is GONE (replaced by the disk-truth update): closing the buffer must not
        // restore the stale pre-edit stash.
        svc.buffer_closed(&dir, &a, &[], &ctx);
        let (_, findings3) = wait_ready(&mut svc);
        assert_eq!(findings3.len(), 1, "close after save rolls nothing back: {findings3:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
