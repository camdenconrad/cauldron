//! File watcher — external changes (git checkout, codegen, another editor) become visible
//! without a manual refresh (issue #2 item 5).
//!
//! One `notify` watcher on the project root (recursive) streams raw events to a named worker
//! thread (house template: `std::thread::Builder` + mpsc + `request_repaint`, as in
//! symbols.rs / search.rs / psi.rs). The worker DEBOUNCES: events are coalesced until the
//! stream goes quiet (bounded by a hard window so a storm can't starve delivery), filtered
//! through the workspace's unified exclusion rules (`.git` entirely, `.cauldron`, `.idea`
//! excludeFolders, root `.gitignore` + `.git/info/exclude`), and classified by the PURE
//! [`plan_batch`]: a small batch of touched files → [`Plan::Incremental`] (per-file index
//! updates), a burst (> [`BURST_FILES`] distinct files — git checkout) or any removal/rename
//! (the incremental lane has no retraction) → [`Plan::FullRescan`]. The UI drains plans once
//! per frame via [`FsWatcher::poll`].
//!
//! Degrades gracefully: if the OS watcher can't be set up (inotify descriptor limits, exotic
//! filesystems) we log and keep the manual-refresh behavior — never crash.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use notify::{RecommendedWatcher, RecursiveMode, Watcher};

/// A batch closes after the event stream stays quiet this long (the debounce window).
const QUIET: Duration = Duration::from_millis(300);
/// Hard cap on one coalesce window — a continuous storm still delivers batches.
const MAX_WINDOW: Duration = Duration::from_millis(1200);
/// More distinct files than this in one batch = burst (git checkout, generator run):
/// one coalesced full rescan instead of N incremental updates.
pub const BURST_FILES: usize = 64;

/// What one filesystem event means to the index layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsKind {
    /// Created or modified — the path's CURRENT content is (re-)indexable.
    Touched,
    /// Removed, or the FROM side of a rename — the path is gone.
    Removed,
}

/// One filtered-input event for [`plan_batch`] (paths are absolute, as notify reports them).
#[derive(Clone, Debug)]
pub struct FsEvent {
    pub path: PathBuf,
    pub kind: FsKind,
}

/// What a debounced batch asks the app to do. Every plan implies a workspace-tree refresh
/// (git tints change even for content-only edits); the variants pick the INDEX strategy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    /// Small batch: per-file invalidation (PSI `replace_file_facts`, symbol re-extraction)
    /// for exactly these deduped paths.
    Incremental(Vec<PathBuf>),
    /// Burst or removal/rename: rebuild the indexes wholesale.
    FullRescan,
}

/// PURE debounce/coalesce decision: classify one batch of raw events into a [`Plan`].
/// `ignored` is the unified exclusion predicate (excluded paths never influence the plan);
/// `burst` is the distinct-file threshold ([`BURST_FILES`] in production).
///
/// Rules, in order: drop ignored paths; nothing left → `None` (the batch was all noise);
/// any removal → `FullRescan` (the incremental index lane cannot retract, and the file
/// universe changed); more than `burst` distinct files → `FullRescan`; else
/// `Incremental(deduped paths in first-seen order)`.
pub fn plan_batch(
    events: &[FsEvent],
    ignored: &dyn Fn(&Path) -> bool,
    burst: usize,
) -> Option<Plan> {
    let mut seen: HashSet<&Path> = HashSet::new();
    let mut files: Vec<PathBuf> = Vec::new();
    let mut removed = false;
    for ev in events {
        if ignored(&ev.path) {
            continue;
        }
        removed |= ev.kind == FsKind::Removed;
        if seen.insert(ev.path.as_path()) {
            files.push(ev.path.clone());
        }
    }
    if files.is_empty() {
        return None;
    }
    if removed || files.len() > burst {
        return Some(Plan::FullRescan);
    }
    Some(Plan::Incremental(files))
}

/// Merge an incoming plan into an accumulator (multiple batches drained in one frame):
/// `FullRescan` absorbs everything; two incrementals union (overflow past `burst` upgrades).
pub fn merge_plans(acc: Option<Plan>, next: Plan, burst: usize) -> Plan {
    match (acc, next) {
        (None, p) => p,
        (Some(Plan::FullRescan), _) | (Some(_), Plan::FullRescan) => Plan::FullRescan,
        (Some(Plan::Incremental(mut a)), Plan::Incremental(b)) => {
            for p in b {
                if !a.contains(&p) {
                    a.push(p);
                }
            }
            if a.len() > burst {
                Plan::FullRescan
            } else {
                Plan::Incremental(a)
            }
        }
    }
}

/// The unified event-exclusion rules (mirrors the item-1 walk: `.git` skipped, `.idea`
/// excludeFolders pruned, gitignore respected) plus `.cauldron` (the IDE's own scratch dir,
/// gitignored via `.git/info/exclude` — hidden from git's keyspace, so matched explicitly).
struct IgnoreRules {
    root: PathBuf,
    /// Workspace-relative excluded dirs (`.idea` excludeFolder interop).
    excludes: Vec<PathBuf>,
    /// Root `.gitignore` + `.git/info/exclude` matcher (best-effort; nested `.gitignore`s are
    /// not consulted). A false negative costs one wasted refresh for the tree; the INDEX lanes
    /// are protected separately — the app gates every incremental path on membership in the
    /// canonical workspace universe (`Workspace::contains`), which the full gitignore-aware
    /// walk produces, so an under-filtered event can never pollute PSI/symbols.
    gitignore: Option<ignore::gitignore::Gitignore>,
}

impl IgnoreRules {
    fn new(root: &Path, excludes: &[PathBuf]) -> Self {
        let mut b = ignore::gitignore::GitignoreBuilder::new(root);
        b.add(root.join(".gitignore"));
        b.add(root.join(".git/info/exclude"));
        Self {
            root: root.to_path_buf(),
            excludes: excludes.to_vec(),
            gitignore: b.build().ok(),
        }
    }

    fn ignored(&self, path: &Path) -> bool {
        let Ok(rel) = path.strip_prefix(&self.root) else {
            return true; // outside the workspace — never ours
        };
        // `.git` internals (index churn, checkout bookkeeping) — the working-tree events from
        // a checkout are the signal, not HEAD/refs. `.cauldron` = our own runconfig/build dir.
        if matches!(
            rel.components().next().and_then(|c| c.as_os_str().to_str()),
            Some(".git") | Some(".cauldron")
        ) {
            return true;
        }
        if self.excludes.iter().any(|x| rel.starts_with(x)) {
            return true;
        }
        if let Some(gi) = &self.gitignore {
            // is_dir: stat is fine here (deleted paths report false → matched as a file).
            if gi.matched_path_or_any_parents(path, path.is_dir()).is_ignore() {
                return true;
            }
        }
        false
    }
}

/// The app-side handle: owns the OS watcher + the debounce worker; drained once per frame.
pub struct FsWatcher {
    /// Keeps the OS watcher registered; `None` = degraded to manual-refresh behavior.
    _watcher: Option<RecommendedWatcher>,
    rx: Receiver<Plan>,
}

impl FsWatcher {
    /// Watch `root` recursively. On ANY setup error (descriptor limits, missing root) this
    /// logs and returns an inert watcher — the IDE keeps its manual-refresh behavior.
    pub fn start(root: &Path, excludes: &[PathBuf], ctx: &egui::Context) -> Self {
        let (plan_tx, plan_rx) = mpsc::channel::<Plan>();
        let (raw_tx, raw_rx) = mpsc::channel::<notify::Event>();
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res {
                let _ = raw_tx.send(ev);
            }
        })
        .and_then(|mut w| w.watch(root, RecursiveMode::Recursive).map(|()| w));
        let watcher = match watcher {
            Ok(w) => w,
            Err(e) => {
                log::warn!(
                    "file watcher unavailable on {} ({e}); falling back to manual refresh",
                    root.display()
                );
                return Self { _watcher: None, rx: plan_rx };
            }
        };
        let rules = IgnoreRules::new(root, excludes);
        let ctx = ctx.clone();
        // Worker exits when the watcher (and thus raw_tx) drops — e.g. on project switch.
        std::thread::Builder::new()
            .name("cauldron-fs-watch".into())
            .spawn(move || debounce_loop(raw_rx, plan_tx, rules, ctx))
            .ok();
        Self { _watcher: Some(watcher), rx: plan_rx }
    }

    /// Is the OS watcher actually running? (false = degraded: rely on manual refresh).
    #[allow(dead_code)] // surfaced for a future status-bar indicator
    pub fn active(&self) -> bool {
        self._watcher.is_some()
    }

    /// Drain finished batches (call once per frame). Multiple pending plans merge —
    /// `FullRescan` wins, incrementals union (upgrading on overflow).
    pub fn poll(&mut self) -> Option<Plan> {
        let mut merged: Option<Plan> = None;
        while let Ok(plan) = self.rx.try_recv() {
            merged = Some(merge_plans(merged, plan, BURST_FILES));
        }
        merged
    }
}

/// The debounce worker: park on the first event, coalesce until the stream goes quiet
/// (`QUIET`) or the hard window (`MAX_WINDOW`) closes, classify, deliver, repeat.
fn debounce_loop(
    raw: Receiver<notify::Event>,
    out: Sender<Plan>,
    rules: IgnoreRules,
    ctx: egui::Context,
) {
    while let Ok(first) = raw.recv() {
        let opened = Instant::now();
        let mut events: Vec<FsEvent> = Vec::new();
        map_event(first, &mut events);
        let mut last = Instant::now();
        loop {
            let now = Instant::now();
            if now.duration_since(opened) >= MAX_WINDOW {
                break;
            }
            let Some(left) = QUIET.checked_sub(now.duration_since(last)).filter(|d| !d.is_zero())
            else {
                break;
            };
            match raw.recv_timeout(left) {
                Ok(ev) => {
                    map_event(ev, &mut events);
                    last = Instant::now();
                }
                Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        if let Some(plan) = plan_batch(&events, &|p| rules.ignored(p), BURST_FILES) {
            if out.send(plan).is_err() {
                return; // app side gone
            }
            ctx.request_repaint();
        }
    }
}

/// Flatten one notify event into [`FsEvent`]s: access noise dropped; removes and the FROM
/// side of renames are `Removed`; everything else (create/modify/unknown) is `Touched`.
fn map_event(ev: notify::Event, out: &mut Vec<FsEvent>) {
    use notify::event::{EventKind, ModifyKind, RenameMode};
    match ev.kind {
        EventKind::Access(_) => {}
        EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
            out.extend(ev.paths.into_iter().map(|p| FsEvent { path: p, kind: FsKind::Removed }));
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
            // paths = [from, to]
            let mut it = ev.paths.into_iter();
            if let Some(from) = it.next() {
                out.push(FsEvent { path: from, kind: FsKind::Removed });
            }
            out.extend(it.map(|p| FsEvent { path: p, kind: FsKind::Touched }));
        }
        _ => {
            out.extend(ev.paths.into_iter().map(|p| FsEvent { path: p, kind: FsKind::Touched }));
        }
    }
}

// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn touched(p: &str) -> FsEvent {
        FsEvent { path: PathBuf::from(p), kind: FsKind::Touched }
    }
    fn removed(p: &str) -> FsEvent {
        FsEvent { path: PathBuf::from(p), kind: FsKind::Removed }
    }
    fn keep_all(_: &Path) -> bool {
        false
    }

    #[test]
    fn plan_all_noise_is_none() {
        assert_eq!(plan_batch(&[], &keep_all, 64), None);
        // Everything filtered out → the batch never surfaces.
        let evs = [touched("/r/target/debug/x.o"), touched("/r/.git/index")];
        let ignore_all = |_: &Path| true;
        assert_eq!(plan_batch(&evs, &ignore_all, 64), None);
    }

    #[test]
    fn plan_small_batch_is_incremental_and_dedupes() {
        let evs = [
            touched("/r/src/a.c"),
            touched("/r/src/b.c"),
            touched("/r/src/a.c"), // editor writes twice — one entry
        ];
        let plan = plan_batch(&evs, &keep_all, 64).unwrap();
        assert_eq!(
            plan,
            Plan::Incremental(vec![PathBuf::from("/r/src/a.c"), PathBuf::from("/r/src/b.c")])
        );
    }

    #[test]
    fn plan_burst_coalesces_to_full_rescan() {
        // git checkout: hundreds of distinct files → ONE full rescan, not N updates.
        let evs: Vec<FsEvent> =
            (0..65).map(|i| touched(&format!("/r/src/f{i}.c"))).collect();
        assert_eq!(plan_batch(&evs, &keep_all, 64), Some(Plan::FullRescan));
        // Exactly at the threshold still goes incremental.
        let evs: Vec<FsEvent> =
            (0..64).map(|i| touched(&format!("/r/src/f{i}.c"))).collect();
        assert!(matches!(plan_batch(&evs, &keep_all, 64), Some(Plan::Incremental(v)) if v.len() == 64));
    }

    #[test]
    fn plan_removal_forces_full_rescan() {
        // The incremental index lane has no retraction — a delete/rename rebuilds.
        let evs = [touched("/r/src/a.c"), removed("/r/src/old.c")];
        assert_eq!(plan_batch(&evs, &keep_all, 64), Some(Plan::FullRescan));
    }

    #[test]
    fn plan_ignored_removal_does_not_escalate() {
        // A delete inside an excluded dir must not trigger anything.
        let evs = [touched("/r/src/a.c"), removed("/r/target/gone.o")];
        let ignore_target = |p: &Path| p.starts_with("/r/target");
        assert_eq!(
            plan_batch(&evs, &ignore_target, 64),
            Some(Plan::Incremental(vec![PathBuf::from("/r/src/a.c")]))
        );
    }

    #[test]
    fn merge_full_wins_and_union_overflows() {
        let a = Plan::Incremental(vec![PathBuf::from("/r/a.c")]);
        let b = Plan::Incremental(vec![PathBuf::from("/r/a.c"), PathBuf::from("/r/b.c")]);
        // Union dedupes.
        assert_eq!(
            merge_plans(Some(a.clone()), b.clone(), 64),
            Plan::Incremental(vec![PathBuf::from("/r/a.c"), PathBuf::from("/r/b.c")])
        );
        // FullRescan absorbs in either position.
        assert_eq!(merge_plans(Some(Plan::FullRescan), b.clone(), 64), Plan::FullRescan);
        assert_eq!(merge_plans(Some(a.clone()), Plan::FullRescan, 64), Plan::FullRescan);
        // Overflowing the burst threshold upgrades.
        assert_eq!(merge_plans(Some(a), b, 1), Plan::FullRescan);
    }

    #[test]
    fn ignore_rules_unified_exclusions() {
        let dir = std::env::temp_dir().join(format!("cauldron-watch-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join(".git/info")).unwrap();
        std::fs::write(dir.join(".gitignore"), "target/\n*.log\n").unwrap();
        std::fs::write(dir.join(".git/info/exclude"), "generated/\n").unwrap();
        let rules = IgnoreRules::new(&dir, &[PathBuf::from("out")]);
        // Kept: ordinary source files.
        assert!(!rules.ignored(&dir.join("src/main.rs")));
        // .git internals + the IDE's own .cauldron dir.
        assert!(rules.ignored(&dir.join(".git/index")));
        assert!(rules.ignored(&dir.join(".git/refs/heads/master")));
        assert!(rules.ignored(&dir.join(".cauldron/runconfigs.json")));
        // .idea excludeFolder (workspace-relative).
        assert!(rules.ignored(&dir.join("out/artifact.bin")));
        // Root .gitignore rules (dir pattern + glob), including files under matched dirs.
        assert!(rules.ignored(&dir.join("target/debug/junk.o")));
        assert!(rules.ignored(&dir.join("src/build.log")));
        // .git/info/exclude rules (where the IDE hides its own scratch entries).
        assert!(rules.ignored(&dir.join("generated/x.c")));
        // Outside the root is never ours.
        assert!(rules.ignored(Path::new("/elsewhere/file.rs")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn map_event_classifies_kinds() {
        use notify::event::{CreateKind, EventKind, ModifyKind, RemoveKind, RenameMode};
        let mk = |kind: EventKind, paths: &[&str]| notify::Event {
            kind,
            paths: paths.iter().map(PathBuf::from).collect(),
            attrs: Default::default(),
        };
        let mut out = Vec::new();
        map_event(mk(EventKind::Create(CreateKind::File), &["/r/new.c"]), &mut out);
        map_event(mk(EventKind::Remove(RemoveKind::File), &["/r/gone.c"]), &mut out);
        map_event(
            mk(EventKind::Modify(ModifyKind::Name(RenameMode::Both)), &["/r/from.c", "/r/to.c"]),
            &mut out,
        );
        map_event(mk(EventKind::Access(notify::event::AccessKind::Any), &["/r/read.c"]), &mut out);
        let got: Vec<(&str, FsKind)> =
            out.iter().map(|e| (e.path.to_str().unwrap(), e.kind)).collect();
        assert_eq!(
            got,
            vec![
                ("/r/new.c", FsKind::Touched),
                ("/r/gone.c", FsKind::Removed),
                ("/r/from.c", FsKind::Removed),
                ("/r/to.c", FsKind::Touched),
                // access event dropped entirely
            ]
        );
    }
}
