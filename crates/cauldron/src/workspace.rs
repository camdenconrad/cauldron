//! Workspace — the Project tool window's model + view (RustRover-style file tree).
//!
//! [`Workspace::open`] walks the root once with `ignore::WalkBuilder` (respects `.gitignore`,
//! skips `.git`), builds a nested dir/file tree eagerly (cFS-scale ~5k files is trivial), and
//! runs `git status --porcelain=v1 -z` once to tint changed files. Nothing in [`Workspace::tree_ui`]
//! touches the filesystem or spawns a process — per-frame cost is pure painting; call
//! [`Workspace::refresh`] to re-walk + re-run git status.
//!
//! Paths: the tree stores workspace-relative paths internally (git's keyspace); everything the
//! public API hands back — [`Workspace::tree_ui`]'s clicked file and [`Workspace::all_files`] —
//! is absolute (`root.join(rel)`), ready for `fs::read_to_string`. Strip `workspace.root` for
//! quick-open display.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use egui::Color32;

/// How long the cached git branch may serve before `.git/HEAD` is re-read (boot-wave item 7).
/// Covers branch switches that touch ONLY `.git` (e.g. `git switch` on a clean tree) — the
/// watcher ignores `.git`, so no refresh would otherwise fire.
const BRANCH_TTL: Duration = Duration::from_secs(2);

/// Golden amber — modified / staged-changed files (and the dirty-dir dot).
const AMBER: Color32 = Color32::from_rgb(217, 164, 65);
/// Moss green — untracked / newly added files.
const MOSS: Color32 = Color32::from_rgb(163, 190, 140);
/// Burnt rust — deleted (index-deleted paths that still render, e.g. unmerged).
const RUST: Color32 = Color32::from_rgb(197, 82, 46);
/// Ember red — the error squiggle (same red as the editor's), propagated up the tree.
const EMBER: Color32 = Color32::from_rgb(224, 82, 60);
/// Rust orange — the brand accent; open-folder chevrons.
const ACCENT: Color32 = Color32::from_rgb(233, 110, 44);

/// Git working-tree state of one path, parsed from `status --porcelain=v1 -z`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GitState {
    Modified,
    Untracked,
    Deleted,
}

impl GitState {
    fn color(self) -> Color32 {
        match self {
            Self::Modified => AMBER,
            Self::Untracked => MOSS,
            Self::Deleted => RUST,
        }
    }
}

/// A directory in the tree. `rel` is workspace-relative (empty for the root node).
#[derive(Debug, Default, PartialEq)]
struct DirNode {
    name: String,
    rel: PathBuf,
    dirs: Vec<DirNode>,
    files: Vec<FileNode>,
}

/// A file leaf. `rel` is workspace-relative.
#[derive(Debug, PartialEq)]
struct FileNode {
    name: String,
    rel: PathBuf,
}

/// What a tree interaction asks the app to do (context menus bubble up; fs ops live in the app).
#[derive(Debug, Clone)]
pub enum TreeAction {
    Open(PathBuf),
    /// Create a file inside this workspace-relative dir ("" = root).
    NewFile(PathBuf),
    NewFolder(PathBuf),
    /// Rename this workspace-relative path.
    Rename(PathBuf),
    /// Delete this workspace-relative path (the app confirms).
    Delete(PathBuf),
}

/// The open project: root path, eager file tree, flat quick-open list, git tint map.
pub struct Workspace {
    pub root: PathBuf,
    /// Display name: `.idea/.name` when present, else the root folder's basename.
    pub name: String,
    /// Workspace-relative dirs excluded by `.idea/*.iml` `<excludeFolder>` entries — hidden from
    /// the tree, quick-open, and every other walker consumer (JetBrains project interop).
    excludes: Vec<PathBuf>,
    tree: DirNode,
    /// Flat, case-insensitively sorted list of all files (absolute paths) for quick-open.
    files: Vec<PathBuf>,
    /// Workspace-relative path → git state (empty when the root is not a git repo).
    git: HashMap<PathBuf, GitState>,
    /// Workspace-relative dirs that contain at least one changed path (get the ' •' suffix).
    dirty_dirs: HashSet<PathBuf>,
    /// Workspace-relative path of the file highlighted in the tree.
    selected: Option<PathBuf>,
    /// Workspace-relative files with at least one error diagnostic (get the squiggle).
    problem_files: HashSet<PathBuf>,
    /// Ancestor dirs of `problem_files` — IntelliJ-style: the squiggle climbs the tree.
    problem_dirs: HashSet<PathBuf>,
    /// Background-refresh channel: workers deliver [`RefreshResult`]s, swapped in on
    /// [`Workspace::poll_refresh`]. Seq-guarded — only the latest kick's result lands.
    refresh_tx: Sender<RefreshResult>,
    refresh_rx: Receiver<RefreshResult>,
    refresh_seq: u64,
    /// A refresh worker is currently running. Kicks while inflight are QUEUED (latest ctx kept)
    /// instead of spawning — sustained fs churn must never stack N concurrent tree walks + git
    /// subprocesses whose results the seq guard would trash anyway.
    refresh_inflight: bool,
    /// A kick arrived while a worker was inflight; re-spawned when that worker finishes.
    refresh_queued: Option<egui::Context>,
    /// Cached `git_branch()` value — the top bar + bottom-tab row read it 1-2x per FRAME, so
    /// `.git/HEAD` is read only at refresh points / on a short TTL (boot-wave item 7).
    branch: Option<String>,
    /// When `branch` was last read from disk (None = never — first `poll_refresh` reads).
    branch_read: Option<Instant>,
}

/// One finished background re-walk (tree + universe + git tint), stamped with the seq of the
/// [`Workspace::refresh_async`] kick that requested it.
struct RefreshResult {
    seq: u64,
    tree: DirNode,
    files: Vec<PathBuf>,
    git: HashMap<PathBuf, GitState>,
    dirty_dirs: HashSet<PathBuf>,
}

impl Workspace {
    /// Open a workspace rooted at `root`: read `.idea` project metadata (name + excluded
    /// folders), walk the tree (gitignore-respecting, `.git` skipped) and run git status once.
    /// No further fs/subprocess work happens until [`Workspace::refresh`].
    pub fn open(root: PathBuf) -> Self {
        let idea = read_idea(&root);
        let name = idea.name.unwrap_or_else(|| {
            root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "project".into())
        });
        let (refresh_tx, refresh_rx) = mpsc::channel();
        let mut ws = Self {
            root,
            name,
            excludes: idea.excludes,
            tree: DirNode::default(),
            files: Vec::new(),
            git: HashMap::new(),
            dirty_dirs: HashSet::new(),
            selected: None,
            problem_files: HashSet::new(),
            problem_dirs: HashSet::new(),
            refresh_tx,
            refresh_rx,
            refresh_seq: 0,
            refresh_inflight: false,
            refresh_queued: None,
            branch: None,
            branch_read: None,
        };
        ws.refresh();
        ws
    }

    /// SYNCHRONOUS re-walk + git status. Only for [`Workspace::open`] (the first frame needs a
    /// tree) and tests — everything event-driven goes through [`Workspace::refresh_async`] so
    /// the walk + `git status` subprocess never stall the UI thread.
    pub fn refresh(&mut self) {
        let entries = walk_root(&self.root, &self.excludes);
        self.tree = build_tree(&entries);
        self.files = files_from_entries(&self.root, &entries);
        self.git = git_status(&self.root);
        self.dirty_dirs = dirty_dirs(&self.git);
        self.reread_branch();
    }

    /// Re-walk the tree + re-run git status on a WORKER thread; the result is swapped in by
    /// [`Workspace::poll_refresh`] on a later frame. Overlapping kicks are safe: each bumps the
    /// seq and only the newest result lands. AT MOST ONE worker runs at a time — a kick while
    /// one is inflight is queued (its result would be seq-trashed anyway) and re-spawned when
    /// the worker finishes, so a fs-event storm can't stack concurrent walks + git subprocesses.
    /// Until the swap the old tree keeps painting — consumers of [`Workspace::all_files`] just
    /// see the previous universe for a few ms.
    pub fn refresh_async(&mut self, ctx: &egui::Context) {
        self.refresh_seq += 1;
        if self.refresh_inflight {
            self.refresh_queued = Some(ctx.clone());
            return;
        }
        self.spawn_refresh(ctx);
    }

    fn spawn_refresh(&mut self, ctx: &egui::Context) {
        let seq = self.refresh_seq;
        let root = self.root.clone();
        let excludes = self.excludes.clone();
        let tx = self.refresh_tx.clone();
        let ctx = ctx.clone();
        self.refresh_inflight = std::thread::Builder::new()
            .name("cauldron-ws-refresh".into())
            .spawn(move || {
                let entries = walk_root(&root, &excludes);
                let tree = build_tree(&entries);
                let files = files_from_entries(&root, &entries);
                let git = git_status(&root);
                let dirty_dirs = dirty_dirs(&git);
                let _ = tx.send(RefreshResult { seq, tree, files, git, dirty_dirs });
                ctx.request_repaint();
            })
            .is_ok();
    }

    /// Drain finished background refreshes (call once per frame — cheap when idle). Returns
    /// true when a new tree/universe was swapped in, so dependents can react (re-kick index
    /// scans deferred until the post-event universe exists). A queued kick re-spawns here the
    /// moment the previous worker delivers (even if its stale result was dropped).
    pub fn poll_refresh(&mut self) -> bool {
        let mut swapped = false;
        let mut finished = false;
        while let Ok(res) = self.refresh_rx.try_recv() {
            finished = true;
            if res.seq == self.refresh_seq {
                self.tree = res.tree;
                self.files = res.files;
                self.git = res.git;
                self.dirty_dirs = res.dirty_dirs;
                swapped = true;
            }
        }
        if finished {
            self.refresh_inflight = false;
            if let Some(ctx) = self.refresh_queued.take() {
                self.spawn_refresh(&ctx);
            }
        }
        // Branch cache upkeep (called once per frame): re-read `.git/HEAD` when a new universe
        // just landed (watcher-driven refresh — checkouts route through here) or the TTL
        // lapsed (`.git`-only changes the watcher never sees). One tiny file read per ~2s
        // instead of 1-2x per frame.
        if swapped || self.branch_read.is_none_or(|t| t.elapsed() >= BRANCH_TTL) {
            self.reread_branch();
        }
        swapped
    }

    /// Flat case-insensitively sorted list of every file in the workspace (absolute paths).
    pub fn all_files(&self) -> &[PathBuf] {
        &self.files
    }

    /// Is `path` (absolute) a member of the canonical file universe? Binary search over the
    /// sorted list — the incremental index lanes gate on this so they never admit files the
    /// full scan excludes (gitignored, `.idea`-excluded, outside the root).
    pub fn contains(&self, path: &Path) -> bool {
        self.files.binary_search_by(|p| cmp_paths_ci(p, path)).is_ok()
    }

    /// Workspace-relative dirs excluded via `.idea`. Consumers of the file universe don't need
    /// this (excludes are applied by the walk itself); per-FILE deciders do — the incremental
    /// PSI save path re-derives "would the scan keep this file?" from it, and the future file
    /// watcher must re-derive the universe.
    pub fn excludes(&self) -> &[PathBuf] {
        &self.excludes
    }

    /// Feed the set of files (ABSOLUTE paths) currently carrying error diagnostics. The tree
    /// squiggles those files and every folder above them. Cheap — call whenever diagnostics
    /// change (or each frame; it's O(open files)).
    pub fn set_problem_files<'a>(&mut self, abs_paths: impl IntoIterator<Item = &'a PathBuf>) {
        self.problem_files = abs_paths
            .into_iter()
            .filter_map(|p| p.strip_prefix(&self.root).ok().map(Path::to_path_buf))
            .collect();
        self.problem_dirs = ancestor_dirs(self.problem_files.iter());
    }

    /// Current git branch, or None outside a repo / detached-HEAD short hash. CACHED — serves
    /// the value last read by [`Workspace::refresh`] / [`Workspace::poll_refresh`] (swap or
    /// [`BRANCH_TTL`] timer); the UI calls this every frame and must not hit the filesystem.
    pub fn git_branch(&self) -> Option<String> {
        self.branch.clone()
    }

    /// Re-read `.git/HEAD` into the branch cache and stamp the read time.
    fn reread_branch(&mut self) {
        self.branch = read_git_branch(&self.root);
        self.branch_read = Some(Instant::now());
    }

    /// Any C sources in the tree? Gates the NASA/PSI layer.
    pub fn has_c_sources(&self) -> bool {
        self.files.iter().any(|p| {
            matches!(p.extension().and_then(|e| e.to_str()), Some("c") | Some("h"))
        })
    }

    /// Render the file tree. Returns the action to perform, if any: a FILE click opens it
    /// (directories expand/collapse); right-click menus yield fs actions the app executes.
    /// Pure painting — no fs or subprocess calls here.
    pub fn tree_ui(&mut self, ui: &mut egui::Ui) -> Option<TreeAction> {
        let mut action: Option<TreeAction> = None;
        // Root-level context: right-click the empty space below the tree (a full-width strip).
        self.children_ui(ui, &self.tree, &mut action);
        let (rest, resp) = ui.allocate_exact_size(
            egui::Vec2::new(ui.available_width(), ui.available_height().max(24.0)),
            egui::Sense::click(),
        );
        let _ = rest;
        resp.context_menu(|ui| {
            if ui.button("New File…").clicked_by(egui::PointerButton::Primary) {
                action = Some(TreeAction::NewFile(PathBuf::new()));
                ui.close_menu();
            }
            if ui.button("New Folder…").clicked_by(egui::PointerButton::Primary) {
                action = Some(TreeAction::NewFolder(PathBuf::new()));
                ui.close_menu();
            }
        });
        if let Some(TreeAction::Open(rel)) = &action {
            // `children_ui` reports the RELATIVE path; resolve + remember the selection here.
            let rel = rel.clone();
            self.selected = Some(rel.clone());
            return Some(TreeAction::Open(self.root.join(rel)));
        }
        action
    }

    /// One level of the tree: collapsing headers for subdirs (dirs first), then file rows.
    /// Right-click menus on both bubble [`TreeAction`]s (paths stay workspace-relative here).
    fn children_ui(&self, ui: &mut egui::Ui, node: &DirNode, action: &mut Option<TreeAction>) {
        for dir in &node.dirs {
            // Recover the header's persisted open state BEFORE building it (same id derivation
            // as CollapsingHeader::id_salt) so the label can highlight open folders.
            let id = ui.make_persistent_id(egui::Id::new(&dir.rel));
            let open = egui::collapsing_header::CollapsingState::load(ui.ctx(), id)
                .is_some_and(|s| s.is_open());
            let header = egui::CollapsingHeader::new(self.dir_header_text(ui, dir, open))
                .id_salt(&dir.rel)
                .icon(chevron_icon) // IntelliJ-style '>' instead of the stock triangle
                .show(ui, |ui| self.children_ui(ui, dir, action));
            if self.problem_dirs.contains(&dir.rel) {
                // A file somewhere below has an error — the squiggle climbs to this folder.
                let rect = header.header_response.rect;
                let font = egui::TextStyle::Button.resolve(ui.style());
                let (w, h) = ui.fonts(|f| {
                    (f.layout_no_wrap(dir.name.clone(), font.clone(), EMBER).size().x,
                     f.row_height(&font))
                });
                let left = rect.left() + ui.spacing().indent; // header text x (egui layout)
                squiggle(ui, left, left + w, rect.center().y + h * 0.5 + 1.0);
            }
            header.header_response.context_menu(|ui| {
                if ui.button("New File…").clicked_by(egui::PointerButton::Primary) {
                    *action = Some(TreeAction::NewFile(dir.rel.clone()));
                    ui.close_menu();
                }
                if ui.button("New Folder…").clicked_by(egui::PointerButton::Primary) {
                    *action = Some(TreeAction::NewFolder(dir.rel.clone()));
                    ui.close_menu();
                }
                if ui.button("Rename…").clicked_by(egui::PointerButton::Primary) {
                    *action = Some(TreeAction::Rename(dir.rel.clone()));
                    ui.close_menu();
                }
                ui.separator();
                ui.menu_button("Delete", |ui| {
                    if ui.button(format!("Yes, delete {}/", dir.name)).clicked_by(egui::PointerButton::Primary) {
                        *action = Some(TreeAction::Delete(dir.rel.clone()));
                        ui.close_menu();
                    }
                });
            });
        }
        for file in &node.files {
            let selected = self.selected.as_deref() == Some(file.rel.as_path());
            let mut text = egui::RichText::new(&file.name);
            if let Some(state) = self.git.get(&file.rel) {
                text = text.color(state.color());
            }
            let resp = ui
                .horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 5.0;
                    let (icon_rect, _) =
                        ui.allocate_exact_size(egui::Vec2::splat(13.0), egui::Sense::hover());
                    crate::icons::file_icon(ui, icon_rect, &self.root.join(&file.rel));
                    ui.selectable_label(selected, text)
                })
                .inner;
            if self.problem_files.contains(&file.rel) {
                let r = resp.rect;
                let pad = ui.spacing().button_padding.x;
                squiggle(ui, r.left() + pad, r.right() - pad, r.bottom() - 1.5);
            }
            if resp.clicked_by(egui::PointerButton::Primary) {
                *action = Some(TreeAction::Open(file.rel.clone()));
            }
            resp.context_menu(|ui| {
                if ui.button("Rename…").clicked_by(egui::PointerButton::Primary) {
                    *action = Some(TreeAction::Rename(file.rel.clone()));
                    ui.close_menu();
                }
                ui.separator();
                ui.menu_button("Delete", |ui| {
                    if ui.button(format!("Yes, delete {}", file.name)).clicked_by(egui::PointerButton::Primary) {
                        *action = Some(TreeAction::Delete(file.rel.clone()));
                        ui.close_menu();
                    }
                });
            });
        }
    }

    /// Directory header label; open dirs are highlighted (bright name — IntelliJ-style), dirs
    /// containing changed files get a subtle amber ' •' suffix.
    fn dir_header_text(&self, ui: &egui::Ui, dir: &DirNode, open: bool) -> egui::WidgetText {
        let name_color = if open {
            ui.visuals().strong_text_color()
        } else {
            ui.visuals().text_color()
        };
        if !self.dirty_dirs.contains(&dir.rel) {
            let text = egui::RichText::new(&dir.name);
            return if open { text.color(name_color).into() } else { text.into() };
        }
        let font = egui::TextStyle::Body.resolve(ui.style());
        let mut job = egui::text::LayoutJob::default();
        job.append(
            &dir.name,
            0.0,
            egui::TextFormat { font_id: font.clone(), color: name_color, ..Default::default() },
        );
        job.append(
            " •",
            0.0,
            egui::TextFormat { font_id: font, color: AMBER, ..Default::default() },
        );
        job.into()
    }
}

/// IntelliJ-style folder chevron: a '>' pointing right when collapsed that rotates to point
/// down as the header opens; accent-orange while open so expanded branches read at a glance.
fn chevron_icon(ui: &mut egui::Ui, openness: f32, response: &egui::Response) {
    let rect = response.rect;
    let center = rect.center();
    let r = (rect.height() * 0.3).max(3.0);
    let color = if openness > 0.5 { ACCENT } else { ui.visuals().text_color() };
    let rot = egui::emath::Rot2::from_angle(openness * std::f32::consts::FRAC_PI_2);
    let pts: Vec<egui::Pos2> = [
        egui::vec2(-r * 0.5, -r),
        egui::vec2(r * 0.5, 0.0),
        egui::vec2(-r * 0.5, r),
    ]
    .into_iter()
    .map(|v| center + rot * v)
    .collect();
    ui.painter().add(egui::Shape::line(pts, egui::Stroke::new(1.8, color)));
}

/// Ember error squiggle from `left` to `right` around baseline `y` — the tree's rendition of
/// the editor's wavy underline.
fn squiggle(ui: &egui::Ui, left: f32, right: f32, y: f32) {
    if right <= left {
        return;
    }
    let step = 2.4_f32;
    let mut pts = Vec::new();
    let mut x = left;
    let mut up = false;
    while x < right + step * 0.5 {
        pts.push(egui::pos2(x.min(right), if up { y - 1.2 } else { y }));
        up = !up;
        x += step;
    }
    if pts.len() >= 2 {
        ui.painter().add(egui::Shape::line(pts, egui::Stroke::new(1.0, EMBER)));
    }
}

// ---------------------------------------------------------------------------------------------
// walking + tree building (pure below the walk itself)
// ---------------------------------------------------------------------------------------------

/// THE canonical file-universe producer: walk `root` with the same rules as the open workspace
/// (gitignore respected, hidden shown, `.git` skipped, workspace-RELATIVE `excludes` pruned) and
/// return every FILE as an ABSOLUTE path, case-insensitively sorted. [`Workspace::all_files`]
/// serves this exact list for the open workspace; this standalone form exists for consumers and
/// tests that don't hold a `Workspace`. Every project-wide feature (symbol index, find-in-files,
/// PSI, deps) must consume this universe rather than re-walking with its own rules.
#[allow(dead_code)] // the app consumes Workspace::all_files; this form serves tests + walkless consumers
pub fn walk_files(root: &Path, excludes: &[PathBuf]) -> Vec<PathBuf> {
    files_from_entries(root, &walk_root(root, excludes))
}

/// Flatten walk entries to the absolute, case-insensitively sorted file list (dirs dropped).
fn files_from_entries(root: &Path, entries: &[(PathBuf, bool)]) -> Vec<PathBuf> {
    let mut rels: Vec<&PathBuf> =
        entries.iter().filter(|(_, is_dir)| !is_dir).map(|(rel, _)| rel).collect();
    rels.sort_by(|a, b| cmp_paths_ci(a, b));
    rels.into_iter().map(|rel| root.join(rel)).collect()
}

/// Walk `root` with the ignore crate: gitignore rules respected, hidden files SHOWN (an IDE tree
/// wants `.github`, `.cargo`, …) but `.git` itself skipped, plus any `.idea` excludeFolder dirs.
/// Returns workspace-relative `(path, is_dir)` pairs; the root entry itself is excluded.
fn walk_root(root: &Path, excludes: &[PathBuf]) -> Vec<(PathBuf, bool)> {
    let mut out = Vec::new();
    let root_owned = root.to_path_buf();
    let excludes = excludes.to_vec();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(move |e| {
            if e.depth() == 0 {
                return true;
            }
            if e.file_name() == OsStr::new(".git") {
                return false;
            }
            if excludes.is_empty() {
                return true;
            }
            match e.path().strip_prefix(&root_owned) {
                Ok(rel) => !excludes.iter().any(|x| rel.starts_with(x)),
                Err(_) => true,
            }
        })
        .build();
    for entry in walker {
        let Ok(e) = entry else { continue }; // permission errors etc. — skip quietly
        if e.depth() == 0 {
            continue;
        }
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let Ok(rel) = e.path().strip_prefix(root) else { continue };
        out.push((rel.to_path_buf(), is_dir));
    }
    out
}

// ---------------------------------------------------------------------------------------------
// .idea (JetBrains project) interop
// ---------------------------------------------------------------------------------------------

/// What we read from a JetBrains `.idea` directory: display name + excluded folders.
#[derive(Debug, Default, PartialEq)]
struct IdeaMeta {
    name: Option<String>,
    excludes: Vec<PathBuf>,
}

/// Read `.idea/.name` (display name; absent by default — falls back to the dir basename) and
/// every `<excludeFolder>` from `.idea/*.iml`. Missing/malformed files are silently skipped —
/// interop is best-effort, never an error.
fn read_idea(root: &Path) -> IdeaMeta {
    let idea = root.join(".idea");
    let name = std::fs::read_to_string(idea.join(".name"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let mut excludes = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&idea) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "iml") {
                if let Ok(xml) = std::fs::read_to_string(&p) {
                    excludes.extend(scan_exclude_folders(&xml));
                }
            }
        }
    }
    excludes.sort();
    excludes.dedup();
    IdeaMeta { name, excludes }
}

/// Extract root-relative paths from `<excludeFolder url="file://$MODULE_DIR$/…" />` entries.
/// `$MODULE_DIR$` resolves to the project root in RustRover's layout (verified against a real
/// project); a leading `../` (the .iml-inside-.idea convention) lands on the same place, so it
/// is stripped. Plain string scanning — no XML dependency for two attribute reads.
fn scan_exclude_folders(xml: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for seg in xml.split("<excludeFolder").skip(1) {
        // The segment starts inside the tag: ` url="file://$MODULE_DIR$/target" />…`
        let Some(url) = seg.split('"').nth(1) else { continue };
        let Some(rest) = url.strip_prefix("file://$MODULE_DIR$/") else { continue };
        let rest = rest.strip_prefix("../").unwrap_or(rest);
        if !rest.is_empty() && !rest.contains('$') {
            out.push(PathBuf::from(rest));
        }
    }
    out
}

/// Build the nested tree from workspace-relative `(path, is_dir)` entries. Intermediate dirs are
/// created on demand (so a bare `a/b/c.rs` entry still yields dirs `a` and `a/b`). Children are
/// sorted dirs-first, case-insensitively.
fn build_tree(entries: &[(PathBuf, bool)]) -> DirNode {
    let mut root = DirNode::default();
    for (rel, is_dir) in entries {
        insert_path(&mut root, rel, *is_dir);
    }
    sort_node(&mut root);
    root
}

/// Insert one relative path into the tree, creating intermediate directory nodes as needed.
fn insert_path(root: &mut DirNode, rel: &Path, is_dir: bool) {
    let comps: Vec<&OsStr> = rel.iter().collect();
    let mut node = root;
    for (i, comp) in comps.iter().enumerate() {
        let name = comp.to_string_lossy().into_owned();
        let last = i + 1 == comps.len();
        if last && !is_dir {
            if !node.files.iter().any(|f| f.name == name) {
                node.files.push(FileNode { name, rel: rel.to_path_buf() });
            }
            return;
        }
        let idx = match node.dirs.iter().position(|d| d.name == name) {
            Some(idx) => idx,
            None => {
                let rel_so_far: PathBuf = comps[..=i].iter().collect();
                node.dirs.push(DirNode {
                    name,
                    rel: rel_so_far,
                    dirs: Vec::new(),
                    files: Vec::new(),
                });
                node.dirs.len() - 1
            }
        };
        node = &mut node.dirs[idx];
    }
}

/// Recursively sort children: dirs and files each case-insensitive (byte order tiebreak).
fn sort_node(node: &mut DirNode) {
    node.dirs.sort_by(|a, b| cmp_ci(&a.name, &b.name));
    node.files.sort_by(|a, b| cmp_ci(&a.name, &b.name));
    for dir in &mut node.dirs {
        sort_node(dir);
    }
}

/// Case-insensitive name compare with a deterministic case-sensitive tiebreak.
fn cmp_ci(a: &str, b: &str) -> Ordering {
    a.to_lowercase().cmp(&b.to_lowercase()).then_with(|| a.cmp(b))
}

/// Case-insensitive whole-path compare (for the flat quick-open list).
fn cmp_paths_ci(a: &Path, b: &Path) -> Ordering {
    cmp_ci(&a.to_string_lossy(), &b.to_string_lossy())
}

// ---------------------------------------------------------------------------------------------
// git status (subprocess in git_status; parsing is pure)
// ---------------------------------------------------------------------------------------------

/// Run `git -C <root> status --porcelain=v1 -z` ONCE. Not a repo / git missing → empty map,
/// silently (no tint, no error spam).
/// Read the current branch from `.git/HEAD` (symbolic ref → branch name, detached → 8-char
/// short hash, missing/unreadable → None). Uncached — [`Workspace::git_branch`] serves the
/// cached value.
fn read_git_branch(root: &Path) -> Option<String> {
    let head = std::fs::read_to_string(root.join(".git/HEAD")).ok()?;
    let head = head.trim();
    if let Some(rest) = head.strip_prefix("ref: refs/heads/") {
        return Some(rest.to_string());
    }
    Some(head.chars().take(8).collect()) // detached: short hash
}

fn git_status(root: &Path) -> HashMap<PathBuf, GitState> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain=v1", "-z"])
        .output();
    match out {
        Ok(o) if o.status.success() => parse_porcelain(&o.stdout),
        _ => HashMap::new(),
    }
}

/// Parse `status --porcelain=v1 -z` output: NUL-separated `XY path` records; rename/copy records
/// are followed by a second NUL-terminated ORIGINAL path (consumed and dropped — it no longer
/// exists on disk). State: `??`/added → untracked-green, any `D` → deleted, else modified.
fn parse_porcelain(bytes: &[u8]) -> HashMap<PathBuf, GitState> {
    let mut map = HashMap::new();
    let mut records = bytes.split(|&b| b == 0);
    while let Some(rec) = records.next() {
        if rec.len() < 4 || rec[2] != b' ' {
            continue; // trailing empty chunk / malformed record
        }
        let xy = &rec[..2];
        let path = PathBuf::from(String::from_utf8_lossy(&rec[3..]).as_ref());
        // Rename/copy: the NEXT chunk is the original path — swallow it.
        if xy.contains(&b'R') || xy.contains(&b'C') {
            let _ = records.next();
        }
        let state = if xy == b"??" {
            GitState::Untracked
        } else if xy.contains(&b'D') {
            GitState::Deleted
        } else if xy.contains(&b'A') {
            GitState::Untracked // staged-new file: green like untracked (JetBrains-style)
        } else {
            GitState::Modified // M / T / U / R / C …
        };
        map.insert(path, state);
    }
    map
}

/// Every ancestor directory of every changed path — these get the amber ' •' suffix.
fn dirty_dirs(git: &HashMap<PathBuf, GitState>) -> HashSet<PathBuf> {
    ancestor_dirs(git.keys())
}

/// Every ancestor directory (workspace root "" excluded) of the given relative paths.
fn ancestor_dirs<'a>(paths: impl Iterator<Item = &'a PathBuf>) -> HashSet<PathBuf> {
    let mut set = HashSet::new();
    for path in paths {
        let mut cur = path.parent();
        while let Some(dir) = cur {
            if dir.as_os_str().is_empty() {
                break;
            }
            set.insert(dir.to_path_buf());
            cur = dir.parent();
        }
    }
    set
}

// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(list: &[(&str, bool)]) -> Vec<(PathBuf, bool)> {
        list.iter().map(|(p, d)| (PathBuf::from(p), *d)).collect()
    }

    #[test]
    fn tree_nests_and_sorts_dirs_first_case_insensitive() {
        let tree = build_tree(&entries(&[
            ("src/main.rs", false),
            ("README.md", false),
            ("src", true),
            ("Cargo.toml", false),
            ("assets", true),
            ("src/lib.rs", false),
            ("Zebra.txt", false),
            ("apple.txt", false),
        ]));
        let dir_names: Vec<&str> = tree.dirs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(dir_names, ["assets", "src"]);
        let file_names: Vec<&str> = tree.files.iter().map(|f| f.name.as_str()).collect();
        // case-insensitive: apple < Cargo < README < Zebra
        assert_eq!(file_names, ["apple.txt", "Cargo.toml", "README.md", "Zebra.txt"]);
        let src = &tree.dirs[1];
        assert_eq!(src.rel, PathBuf::from("src"));
        let src_files: Vec<&str> = src.files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(src_files, ["lib.rs", "main.rs"]);
        assert_eq!(src.files[1].rel, PathBuf::from("src/main.rs"));
    }

    #[test]
    fn tree_creates_intermediate_dirs_without_explicit_entries() {
        let tree = build_tree(&entries(&[("a/b/c.rs", false)]));
        assert_eq!(tree.dirs.len(), 1);
        let a = &tree.dirs[0];
        assert_eq!(a.name, "a");
        assert_eq!(a.rel, PathBuf::from("a"));
        let b = &a.dirs[0];
        assert_eq!(b.rel, PathBuf::from("a/b"));
        assert_eq!(b.files[0].name, "c.rs");
        assert_eq!(b.files[0].rel, PathBuf::from("a/b/c.rs"));
    }

    #[test]
    fn tree_deduplicates_repeat_entries() {
        let tree = build_tree(&entries(&[
            ("a", true),
            ("a", true),
            ("a/x.rs", false),
            ("a/x.rs", false),
        ]));
        assert_eq!(tree.dirs.len(), 1);
        assert_eq!(tree.dirs[0].files.len(), 1);
    }

    #[test]
    fn read_git_branch_shapes() {
        let dir = std::env::temp_dir().join(format!("cauldron-head-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(read_git_branch(&dir), None, "no .git at all");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        assert_eq!(read_git_branch(&dir), None, ".git without HEAD");
        std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        assert_eq!(read_git_branch(&dir).as_deref(), Some("feature/x"));
        std::fs::write(dir.join(".git/HEAD"), "0123456789abcdef0123\n").unwrap();
        assert_eq!(read_git_branch(&dir).as_deref(), Some("01234567"), "detached: short hash");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn git_branch_is_cached_and_invalidated_by_refresh_and_ttl() {
        let dir = std::env::temp_dir().join(format!("cauldron-branch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        // open() runs a sync refresh: no HEAD yet -> None cached.
        let mut ws = Workspace::open(dir.clone());
        assert_eq!(ws.git_branch(), None);
        std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/rune\n").unwrap();
        // CACHED: the on-disk change is invisible until an invalidation point.
        assert_eq!(ws.git_branch(), None, "no fs read on the accessor");
        ws.refresh();
        assert_eq!(ws.git_branch().as_deref(), Some("rune"), "sync refresh re-reads");
        // TTL lane (the per-frame poll_refresh call): a fresh stamp serves the cache even
        // though HEAD changed on disk; an aged stamp re-reads.
        std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/other\n").unwrap();
        let _ = ws.poll_refresh();
        assert_eq!(ws.git_branch().as_deref(), Some("rune"), "TTL not lapsed → cached");
        ws.branch_read = Some(Instant::now() - BRANCH_TTL * 2);
        let _ = ws.poll_refresh();
        assert_eq!(ws.git_branch().as_deref(), Some("other"), "TTL lapsed → re-read");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn porcelain_maps_states() {
        let raw = b" M src/main.rs\0?? new.txt\0 D gone.rs\0A  added.rs\0AD flash.rs\0MM both.rs\0";
        let map = parse_porcelain(raw);
        assert_eq!(map.get(Path::new("src/main.rs")), Some(&GitState::Modified));
        assert_eq!(map.get(Path::new("new.txt")), Some(&GitState::Untracked));
        assert_eq!(map.get(Path::new("gone.rs")), Some(&GitState::Deleted));
        assert_eq!(map.get(Path::new("added.rs")), Some(&GitState::Untracked));
        assert_eq!(map.get(Path::new("flash.rs")), Some(&GitState::Deleted)); // D wins over A
        assert_eq!(map.get(Path::new("both.rs")), Some(&GitState::Modified));
        assert_eq!(map.len(), 6);
    }

    #[test]
    fn porcelain_rename_consumes_original_path() {
        let raw = b"R  new_name.rs\0old_name.rs\0 M after.rs\0";
        let map = parse_porcelain(raw);
        assert_eq!(map.get(Path::new("new_name.rs")), Some(&GitState::Modified));
        assert!(!map.contains_key(Path::new("old_name.rs")));
        // parsing stays in sync after the double record
        assert_eq!(map.get(Path::new("after.rs")), Some(&GitState::Modified));
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn porcelain_empty_and_garbage_safe() {
        assert!(parse_porcelain(b"").is_empty());
        assert!(parse_porcelain(b"\0\0").is_empty());
        assert!(parse_porcelain(b"xy").is_empty());
    }

    #[test]
    fn iml_exclude_folders_parse() {
        let xml = r#"<?xml version="1.0"?>
<module type="EMPTY_MODULE" version="4">
  <component name="NewModuleRootManager">
    <content url="file://$MODULE_DIR$">
      <sourceFolder url="file://$MODULE_DIR$/crates/app/src" isTestSource="false" />
      <excludeFolder url="file://$MODULE_DIR$/target" />
      <excludeFolder url="file://$MODULE_DIR$/../build" />
      <excludeFolder url="file://$MODULE_DIR$/.cache/clangd" />
    </content>
  </component>
</module>"#;
        let ex = scan_exclude_folders(xml);
        assert_eq!(
            ex,
            vec![PathBuf::from("target"), PathBuf::from("build"), PathBuf::from(".cache/clangd")]
        );
    }

    #[test]
    fn read_idea_name_and_excludes_from_fixture() {
        let dir = std::env::temp_dir().join(format!("cauldron-idea-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".idea")).unwrap();
        std::fs::write(dir.join(".idea/.name"), "My Project\n").unwrap();
        std::fs::write(
            dir.join(".idea/proj.iml"),
            r#"<module><component><content url="file://$MODULE_DIR$">
               <excludeFolder url="file://$MODULE_DIR$/target" /></content></component></module>"#,
        )
        .unwrap();
        let meta = read_idea(&dir);
        assert_eq!(meta.name.as_deref(), Some("My Project"));
        assert_eq!(meta.excludes, vec![PathBuf::from("target")]);
        // No .idea at all → clean default.
        assert_eq!(read_idea(&dir.join("nope")), IdeaMeta::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn excluded_dirs_hidden_from_walk() {
        let dir = std::env::temp_dir().join(format!("cauldron-walk-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("target/debug")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main(){}").unwrap();
        std::fs::write(dir.join("target/debug/junk.o"), "x").unwrap();
        let entries = walk_root(&dir, &[PathBuf::from("target")]);
        assert!(entries.iter().any(|(p, _)| p == Path::new("src/main.rs")));
        assert!(
            !entries.iter().any(|(p, _)| p.starts_with("target")),
            "excluded dir leaked into the walk: {entries:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn walk_files_universe_is_absolute_and_honors_relative_excludes() {
        // The historic SymbolIndex bug: ABSOLUTE walked paths were compared against
        // workspace-RELATIVE excludes, so excludes never matched. The canonical universe
        // producer applies relative excludes during the walk and hands back absolute paths.
        let dir = std::env::temp_dir().join(format!("cauldron-universe-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("target/debug")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main(){}").unwrap();
        std::fs::write(dir.join("target/debug/junk.rs"), "fn j(){}").unwrap();
        let files = walk_files(&dir, &[PathBuf::from("target")]);
        assert!(files.iter().all(|p| p.is_absolute()), "universe paths are absolute");
        assert!(files.contains(&dir.join("src/main.rs")));
        assert!(
            !files.iter().any(|p| p.starts_with(dir.join("target"))),
            "file under an excluded dir leaked into the universe: {files:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn problem_files_propagate_to_ancestor_dirs() {
        let dir = std::env::temp_dir().join(format!("cauldron-problem-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src/deep")).unwrap();
        std::fs::write(dir.join("src/deep/bad.py"), "x: int = 'oops'\n").unwrap();
        let mut ws = Workspace::open(dir.clone());
        // Feed ABSOLUTE paths (the app's diag-store keyspace); one outside the root is dropped.
        let abs = vec![dir.join("src/deep/bad.py"), PathBuf::from("/elsewhere/x.rs")];
        ws.set_problem_files(abs.iter());
        assert!(ws.problem_files.contains(Path::new("src/deep/bad.py")));
        assert_eq!(ws.problem_files.len(), 1);
        assert!(ws.problem_dirs.contains(Path::new("src")));
        assert!(ws.problem_dirs.contains(Path::new("src/deep")));
        assert_eq!(ws.problem_dirs.len(), 2);
        // Clearing works too (errors fixed → squiggles gone).
        ws.set_problem_files(std::iter::empty());
        assert!(ws.problem_files.is_empty() && ws.problem_dirs.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refresh_async_swaps_tree_off_thread() {
        let dir = std::env::temp_dir().join(format!("cauldron-async-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main(){}").unwrap();
        let mut ws = Workspace::open(dir.clone());
        assert_eq!(ws.all_files().len(), 1);
        // A file appears externally (codegen / git checkout)…
        std::fs::write(dir.join("src/newborn.rs"), "pub fn n(){}").unwrap();
        assert_eq!(ws.all_files().len(), 1, "sync state untouched before the kick");
        ws.refresh_async(&egui::Context::default());
        // …and the worker delivers the new universe via the channel.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !ws.poll_refresh() {
            assert!(std::time::Instant::now() < deadline, "refresh worker never delivered");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(ws.all_files().len(), 2);
        assert!(ws.all_files().contains(&dir.join("src/newborn.rs")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Issue #2 review: kicks while a worker is INFLIGHT must queue, not stack workers — and
    /// the queued kick's (fresher) universe must still land. Also covers `contains` (the
    /// incremental lanes' universe-membership gate).
    #[test]
    fn refresh_kicks_while_inflight_queue_and_still_deliver_the_newest_universe() {
        let dir = std::env::temp_dir().join(format!("cauldron-inflight-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main(){}").unwrap();
        let mut ws = Workspace::open(dir.clone());
        let ctx = egui::Context::default();

        ws.refresh_async(&ctx);
        assert!(ws.refresh_inflight, "first kick spawns");
        // A storm of kicks while the worker runs: all queue, none spawn a second worker.
        std::fs::write(dir.join("src/second.rs"), "pub fn s(){}").unwrap();
        ws.refresh_async(&ctx);
        ws.refresh_async(&ctx);
        assert!(ws.refresh_queued.is_some(), "kick while inflight is queued");

        // Drain to completion: the first worker's result is seq-stale (dropped), the queued
        // re-spawn delivers the post-change universe.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !ws.poll_refresh() {
            assert!(std::time::Instant::now() < deadline, "queued refresh never delivered");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(ws.all_files().contains(&dir.join("src/second.rs")));
        assert!(!ws.refresh_inflight || ws.refresh_queued.is_none());

        // The membership gate agrees with the universe.
        assert!(ws.contains(&dir.join("src/main.rs")));
        assert!(ws.contains(&dir.join("src/second.rs")));
        assert!(!ws.contains(&dir.join("src/nope.rs")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dirty_dirs_covers_all_ancestors() {
        let mut git = HashMap::new();
        git.insert(PathBuf::from("src/deep/x.rs"), GitState::Modified);
        git.insert(PathBuf::from("top.rs"), GitState::Untracked);
        let dirty = dirty_dirs(&git);
        assert!(dirty.contains(Path::new("src")));
        assert!(dirty.contains(Path::new("src/deep")));
        assert_eq!(dirty.len(), 2); // root ("") never included
    }
}
