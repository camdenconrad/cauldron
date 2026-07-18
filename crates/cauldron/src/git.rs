//! Git panel — the dock's git tab body: branch + ahead/behind header, STAGED / CHANGES file
//! rows with per-file stage / unstage / discard, a commit box, and push / pull. The dock owns
//! the tab header; [`GitPanel::ui`] draws only the body and returns a clicked file's ABSOLUTE
//! path so the app opens it in an editor tab.
//!
//! Every git invocation (`git -C <root> …`) runs on a background thread — results come back
//! over an mpsc channel + `request_repaint`, the same worker shape as [`crate::runner`] and
//! [`crate::search`]; nothing here ever blocks the frame. Parsing is pure functions tested
//! with fixture strings (no process spawns in tests).
//!
//! Status model: porcelain parsing follows `workspace::parse_porcelain`'s conventions
//! (NUL-split records, rename original consumed) but keeps BOTH columns — a path that is
//! staged AND has worktree edits (e.g. `MM`) yields TWO rows, one in the Staged section and
//! one in Changes, each carrying the raw index+worktree chars in
//! [`Entry::staged_state`] / [`Entry::worktree_state`].

#![allow(dead_code)] // the integrator wires `mod git;` + GitPanel into the dock next

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use egui::{Align2, Color32, FontId, Pos2, Rect, Rounding, Sense, Vec2};

use crate::style::{colors, hairline, panel_header_inline, sizes};

/// Dense row height (the tree uses 20; git rows pack tighter).
const ROW_H: f32 = 18.0;
/// The square stage/unstage glyph slot on a row's right edge.
const ACTION_W: f32 = 16.0;
/// Left inset for headers/rows/footer widgets.
const PAD: f32 = 8.0;
/// Visible-panel auto-refresh cadence.
const AUTO_REFRESH_SECS: u64 = 5;

/// One row of the status list. A path with changes in BOTH columns appears twice — once with
/// `staged: true` (state = index char) and once with `staged: false` (state = worktree char).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Repo-root-relative path (git's keyspace; join with the root for the editor).
    pub rel: PathBuf,
    /// Which section this row belongs to.
    pub staged: bool,
    /// Summary char for THIS row: index char for staged rows, worktree char for unstaged,
    /// `?` for untracked.
    pub state: char,
    /// Raw porcelain index column (X), kept on both rows of a two-column path.
    pub staged_state: char,
    /// Raw porcelain worktree column (Y), kept on both rows.
    pub worktree_state: char,
    /// Unmerged (UU/AA/DD/one-sided U): shown in the Conflicts section, never Staged —
    /// the raw columns previously parsed as a bogus staged row.
    pub conflicted: bool,
}

/// What background workers send back. Each spawned job ends with exactly one `Status`
/// (actions chain a re-gather after the command), which is what `inflight` counts down on.
enum Msg {
    Status {
        /// Spawn sequence — stale results (an older job finishing late) are dropped.
        seq: u64,
        branch: Option<String>,
        /// `(ahead, behind)` vs `@{upstream}`; `None` = no upstream configured.
        upstream: Option<(u32, u32)>,
        entries: Vec<Entry>,
        /// Local branches (most-recent first) for the switcher menu.
        branches: Vec<String>,
        stashes: Vec<Stash>,
    },
    ActionDone {
        output: Option<String>,
        error: Option<String>,
        /// Commit success clears the message box.
        was_commit: bool,
    },
    /// AI-generated commit message (None = request failed). Outside the seq/inflight
    /// accounting: it mutates only the draft box, never repo state.
    AiCommitMsg(Option<String>),
}

/// Collect the staged diff + recent subjects and build the commit-message prompt. None when
/// git fails or nothing is staged (the button is gated on staged rows, but a race is cheap
/// to tolerate).
fn staged_diff_prompt(root: &Path) -> Option<String> {
    let run = |args: &[&str]| -> Option<String> {
        let out = Command::new("git").arg("-C").arg(root).args(args).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let stat = run(&["diff", "--cached", "--stat"])?;
    let diff = run(&["diff", "--cached", "-U3"])?;
    if diff.trim().is_empty() {
        return None;
    }
    // Recent subjects teach the model the repo's message style (conventional commits, etc.).
    let recent = run(&["log", "--no-merges", "--format=%s", "-8"]).unwrap_or_default();
    Some(commit_prompt(&stat, &diff, &recent))
}

/// Pure prompt builder — unit-tested below. The diff is capped so a huge staged change can't
/// blow the request (or a local model's context) up; the --stat block keeps the full picture.
fn commit_prompt(stat: &str, diff: &str, recent_subjects: &str) -> String {
    const DIFF_CAP: usize = 12_000;
    let mut diff = diff;
    let mut truncated = "";
    if diff.len() > DIFF_CAP {
        let mut end = DIFF_CAP;
        while !diff.is_char_boundary(end) {
            end -= 1;
        }
        diff = &diff[..end];
        truncated = "\n(diff truncated)";
    }
    let style = if recent_subjects.trim().is_empty() {
        String::new()
    } else {
        format!("Recent commit subjects in this repo (match their style):\n{}\n\n", recent_subjects.trim())
    };
    format!(
        "Write a git commit message for the STAGED changes below. First line: imperative \
         summary, at most 72 characters. Optionally one blank line and a short body (wrap at \
         72). Reply with ONLY the commit message — no fences, no commentary.\n\n{style}\
         Stat:\n{stat}\nDiff:\n{diff}{truncated}"
    )
}

/// One `git stash list` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stash {
    /// The reflog selector, e.g. `stash@{0}` — what apply/pop/drop take.
    pub refname: String,
    /// Unix seconds.
    pub time: i64,
    /// The stash subject ("WIP on main: …" or the -m message).
    pub subject: String,
}

/// Parse `git stash list --pretty=format:%gd%x1f%at%x1f%gs%x1e`. Separator-based like the
/// history parser; malformed records are skipped, never panicked on.
fn parse_stash_list(text: &str) -> Vec<Stash> {
    text.split('\x1e')
        .filter_map(|rec| {
            let rec = rec.trim_start_matches(['\n', '\r']);
            let mut f = rec.split('\x1f');
            let refname = f.next()?.trim().to_string();
            if !refname.starts_with("stash@{") {
                return None;
            }
            Some(Stash {
                refname,
                time: f.next()?.trim().parse().ok()?,
                subject: f.next().unwrap_or("").trim().to_string(),
            })
        })
        .collect()
}

/// A row/section interaction collected during painting and executed after the borrow ends.
enum RowAct {
    /// Resolve a conflict wholesale: `git checkout --ours/--theirs -- <p>` then `git add`.
    TakeSide(PathBuf, &'static str),
    /// The file was fixed by hand (markers edited out): `git add` marks it resolved.
    MarkResolved(PathBuf),
    Stage(PathBuf),
    Unstage(PathBuf),
    Discard(PathBuf),
    StageAll,
    UnstageAll,
    /// `git stash push` of everything (tracked changes; untracked stay put, like bare git).
    StashPush,
    StashApply(String),
    StashPop(String),
    StashDrop(String),
}

/// The git tool-window body. Construct with `GitPanel::default()`; call [`GitPanel::ui`] when
/// the tab is visible (it self-refreshes at most every [`AUTO_REFRESH_SECS`]) and
/// [`GitPanel::refresh`] to kick an immediate re-read (e.g. after the editor saves a file).
pub struct GitPanel {
    /// The repo root the current state was gathered from; a root change resets + refreshes.
    root: Option<PathBuf>,
    branch: Option<String>,
    /// `(ahead, behind)` vs upstream; `None` when there is no upstream (tolerated quietly).
    upstream: Option<(u32, u32)>,
    entries: Vec<Entry>,
    /// Local branches for the switcher; refreshed with status.
    branches: Vec<String>,
    /// Draft name while the "new branch" prompt is open (`None` = closed).
    new_branch: Option<String>,
    /// One-shot: the new-branch box grabs focus only on the frame it opens (an unconditional
    /// per-frame request_focus would re-steal focus from the editor every frame).
    branch_focus_pending: bool,
    commit_msg: String,
    /// Footer feedback from the last action (push/pull/commit/…): success output vs error.
    last_output: Option<String>,
    last_error: Option<String>,
    rx: Receiver<Msg>,
    tx: Sender<Msg>,
    /// Background jobs still running; `> 0` = busy (spinner, actions gated).
    inflight: usize,
    /// Bumped per spawned job; only the latest job's `Status` is applied.
    seq: u64,
    /// One-shot: a repo-mutating action (commit/pull/push/checkout/stage/discard) completed
    /// since the app last asked — blame/history caches are stale.
    repo_changed: bool,
    stashes: Vec<Stash>,
    /// Two-step drop confirm: the refname armed for dropping (any other click disarms).
    armed_drop: Option<String>,
    last_refresh: Option<Instant>,
    /// An AI commit-message request is out (spinner by the button; one at a time).
    ai_msg_inflight: bool,
}

impl Default for GitPanel {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            root: None,
            branch: None,
            upstream: None,
            entries: Vec::new(),
            branches: Vec::new(),
            new_branch: None,
            branch_focus_pending: false,
            commit_msg: String::new(),
            last_output: None,
            last_error: None,
            rx,
            tx,
            inflight: 0,
            seq: 0,
            repo_changed: false,
            stashes: Vec::new(),
            armed_drop: None,
            last_refresh: None,
            ai_msg_inflight: false,
        }
    }
}

impl GitPanel {
    /// True when the panel has never loaded or its data is older than the refresh cadence —
    /// the integrator can check this when the tab becomes visible ([`GitPanel::ui`] also
    /// auto-refreshes on the same staleness test, so calling it is enough).
    pub fn needs_refresh_on_show(&self) -> bool {
        self.last_refresh.is_none_or(|t| t.elapsed() >= Duration::from_secs(AUTO_REFRESH_SECS))
    }

    /// Kick a background re-read of status + branch + ahead/behind. Never blocks; results
    /// arrive over the channel and repaint the UI.
    pub fn refresh(&mut self, root: &Path, ctx: &egui::Context) {
        self.root = Some(root.to_path_buf());
        self.seq += 1;
        self.inflight += 1;
        self.last_refresh = Some(Instant::now());
        let seq = self.seq;
        let root = root.to_path_buf();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new().name("cauldron-git".into()).spawn(move || {
            let _ = tx.send(gather_status(seq, &root));
            ctx.request_repaint();
        });
        if spawned.is_err() {
            self.inflight -= 1;
        }
    }

    /// Run one git action (`git -C <root> <args…>`) on a background thread, then chain a
    /// status re-gather so the panel refreshes itself when the action lands.
    fn run_action(
        &mut self,
        root: &Path,
        ctx: &egui::Context,
        args: Vec<OsString>,
        was_commit: bool,
    ) {
        self.seq += 1;
        self.inflight += 1;
        self.last_refresh = Some(Instant::now());
        self.last_output = None;
        self.last_error = None;
        let seq = self.seq;
        let root = root.to_path_buf();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new().name("cauldron-git".into()).spawn(move || {
            let result = Command::new("git").arg("-C").arg(&root).args(&args).output();
            let (output, error) = summarize_output(result);
            let _ = tx.send(Msg::ActionDone { output, error, was_commit });
            ctx.request_repaint();
            let _ = tx.send(gather_status(seq, &root));
            ctx.request_repaint();
        });
        if spawned.is_err() {
            self.inflight -= 1;
            self.last_error = Some("failed to spawn git worker thread".into());
        }
    }

    /// [`Self::run_action`] for a SEQUENCE of git commands on one worker thread, aborting at
    /// the first failure (conflict resolution: checkout a side, then add).
    fn run_action_seq(&mut self, root: &Path, ctx: &egui::Context, cmds: Vec<Vec<OsString>>) {
        self.seq += 1;
        self.inflight += 1;
        self.last_refresh = Some(Instant::now());
        self.last_output = None;
        self.last_error = None;
        let seq = self.seq;
        let root = root.to_path_buf();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new().name("cauldron-git".into()).spawn(move || {
            let mut output = None;
            let mut error = None;
            for args in &cmds {
                let result = Command::new("git").arg("-C").arg(&root).args(args).output();
                let (o, e) = summarize_output(result);
                output = o;
                if e.is_some() {
                    error = e;
                    break;
                }
            }
            let _ = tx.send(Msg::ActionDone { output, error, was_commit: false });
            ctx.request_repaint();
            let _ = tx.send(gather_status(seq, &root));
            ctx.request_repaint();
        });
        if spawned.is_err() {
            self.inflight -= 1;
            self.last_error = Some("failed to spawn git worker thread".into());
        }
    }

    /// Drain worker messages; called at the top of every [`GitPanel::ui`].
    fn pump(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Status { seq, branch, upstream, entries, branches, stashes } => {
                    self.inflight = self.inflight.saturating_sub(1);
                    if seq == self.seq {
                        self.branch = branch;
                        self.upstream = upstream;
                        self.entries = entries;
                        self.branches = branches;
                        self.stashes = stashes;
                    }
                }
                Msg::ActionDone { output, error, was_commit } => {
                    if was_commit && error.is_none() {
                        self.commit_msg.clear();
                    }
                    if error.is_none() {
                        // Commit / pull / push / checkout / stage / discard landed: repo state
                        // moved under any blame/history caches the app holds.
                        self.repo_changed = true;
                    }
                    self.last_output = output;
                    self.last_error = error;
                }
                Msg::AiCommitMsg(text) => {
                    self.ai_msg_inflight = false;
                    match text {
                        Some(t) => self.commit_msg = t,
                        None => {
                            self.last_error =
                                Some("AI message failed — backend unreachable or no reply".into())
                        }
                    }
                }
            }
        }
    }

    /// Ask the AI backend for a commit message from the STAGED diff (background thread; the
    /// reply lands in the draft box via [`Msg::AiCommitMsg`] — editable, never auto-committed).
    fn generate_commit_msg(&mut self, root: &Path, ctx: &egui::Context) {
        if self.ai_msg_inflight {
            return;
        }
        self.ai_msg_inflight = true;
        self.last_error = None;
        let root = root.to_path_buf();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new().name("cauldron-git-ai".into()).spawn(move || {
            let text = staged_diff_prompt(&root).and_then(|prompt| {
                crate::ai::ask(crate::ai::OAUTH_SYSTEM, &prompt, "claude-haiku-4-5-20251001", 300, None)
            });
            let text = text
                .map(|t| crate::ai::unfence(&t).trim().to_string())
                .filter(|t| !t.is_empty());
            let _ = tx.send(Msg::AiCommitMsg(text));
            ctx.request_repaint();
        });
        if spawned.is_err() {
            self.ai_msg_inflight = false;
        }
    }

    /// One-shot: did a repo-mutating action complete since last asked? The app drops its
    /// blame/history caches when true.
    pub fn take_repo_changed(&mut self) -> bool {
        std::mem::take(&mut self.repo_changed)
    }

    /// The panel body. Returns the ABSOLUTE path of a clicked file row so the app opens it.
    pub fn ui(&mut self, ui: &mut egui::Ui, root: &Path) -> Option<(PathBuf, bool)> {
        self.pump();
        if self.root.as_deref() != Some(root) {
            // Workspace switched under us — drop the old repo's state before showing anything.
            self.branch = None;
            self.upstream = None;
            self.entries.clear();
            self.branches.clear();
            self.new_branch = None;
            self.last_output = None;
            self.last_error = None;
            self.commit_msg.clear();
            self.refresh(root, ui.ctx());
        } else if self.inflight == 0 && self.needs_refresh_on_show() {
            self.refresh(root, ui.ctx());
        }
        // Wake ourselves for the next staleness check even when no input events arrive.
        ui.ctx().request_repaint_after(Duration::from_secs(AUTO_REFRESH_SECS));

        let busy = self.inflight > 0;
        // (path, was-a-STAGED-row) — drives which diff mode the app opens.
        let mut open: Option<(PathBuf, bool)> = None;
        let mut act: Option<RowAct> = None;

        // ---- header: ⎇ branch▾ · ↑ahead ↓behind ··· ↻ [Pull] [Push] -------------------------
        // The branch is a switcher: the menu lists local branches (checkout on click) and opens a
        // "new branch" prompt. A dirty tree can make checkout fail — that surfaces in the footer
        // like any other action, rather than being pre-empted here.
        let mut switch_to: Option<String> = None;
        ui.horizontal(|ui| {
            ui.add_space(PAD);
            let branch_label = format!("⎇ {} ▾", self.branch.as_deref().unwrap_or("—"));
            ui.menu_button(branch_label, |ui| {
                if ui.button("＋ New branch…").clicked_by(egui::PointerButton::Primary) {
                    self.new_branch = Some(String::new());
                    self.branch_focus_pending = true;
                    ui.close_menu();
                }
                if !self.branches.is_empty() {
                    ui.separator();
                }
                egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                    for b in &self.branches {
                        let current = Some(b.as_str()) == self.branch.as_deref();
                        let mark = if current { "● " } else { "   " };
                        if ui
                            .add_enabled(!current, egui::Button::new(format!("{mark}{b}")))
                            .clicked_by(egui::PointerButton::Primary)
                        {
                            switch_to = Some(b.clone());
                            ui.close_menu();
                        }
                    }
                });
            });
            if let Some((ahead, behind)) = self.upstream {
                ui.colored_label(colors::TEXT_FAINT(), format!("·  ↑{ahead} ↓{behind}"));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add_enabled(!busy, egui::Button::new("Push").small()).clicked_by(egui::PointerButton::Primary) {
                    self.run_action(root, ui.ctx(), git_args(&["push"]), false);
                }
                let dirty = !self.entries.is_empty();
                if ui
                    .add_enabled(!busy && dirty, egui::Button::new("Stash").small())
                    .on_hover_text("git stash push — set changes aside")
                    .clicked_by(egui::PointerButton::Primary)
                {
                    act = Some(RowAct::StashPush);
                }
                if ui.add_enabled(!busy, egui::Button::new("Pull").small()).clicked_by(egui::PointerButton::Primary) {
                    self.run_action(root, ui.ctx(), git_args(&["pull", "--ff-only"]), false);
                }
                if busy {
                    ui.spinner();
                } else if ui.small_button("↻").on_hover_text("Refresh").clicked_by(egui::PointerButton::Primary) {
                    self.refresh(root, ui.ctx());
                }
            });
        });
        // Chosen from the switcher menu — `git checkout <branch>`. A failure (dirty tree, etc.)
        // lands in the footer.
        if let Some(b) = switch_to {
            self.run_action(root, ui.ctx(), git_args(&["checkout", b.as_str()]), false);
        }
        // New-branch prompt (opened from the menu). Draft is taken by &mut for the field, then
        // dropped before run_action to avoid borrowing self twice.
        let mut create: Option<String> = None;
        let mut close_prompt = false;
        let mut focus_pending = std::mem::take(&mut self.branch_focus_pending);
        if let Some(draft) = self.new_branch.as_mut() {
            ui.horizontal(|ui| {
                ui.add_space(PAD);
                ui.colored_label(colors::TEXT_FAINT(), "new branch:");
                let resp = ui.add(
                    egui::TextEdit::singleline(draft).desired_width(180.0).hint_text("name"),
                );
                if std::mem::take(&mut focus_pending) {
                    resp.request_focus();
                }
                let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                if (ui.button("Create").clicked_by(egui::PointerButton::Primary) || enter) && !draft.trim().is_empty() {
                    create = Some(draft.trim().to_string());
                }
                if ui.button("Cancel").clicked_by(egui::PointerButton::Primary) || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    close_prompt = true;
                }
            });
        }
        if let Some(name) = create {
            self.run_action(root, ui.ctx(), git_args(&["checkout", "-b", name.as_str()]), false);
            self.new_branch = None;
        } else if close_prompt {
            self.new_branch = None;
        }
        hairline(ui);

        // ---- STAGED / CHANGES lists (footer height reserved below) --------------------------
        let footer_h =
            if self.last_output.is_some() || self.last_error.is_some() { 140.0 } else { 100.0 };
        let list_h = (ui.available_height() - footer_h).max(40.0);
        egui::ScrollArea::vertical().auto_shrink([false, false]).max_height(list_h).show(
            ui,
            |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                let staged_count = self.entries.iter().filter(|e| e.staged).count();
                let conflict_count = self.entries.iter().filter(|e| e.conflicted).count();
                let changed_count = self.entries.len() - staged_count - conflict_count;

                // ---- Conflicts first: they block the merge, nothing else matters until they
                // are gone. Ours/Theirs resolve wholesale; hand-edited files use Resolved.
                if conflict_count > 0 {
                    section_header(ui, "Conflicts", conflict_count, None);
                    for e in self.entries.iter().filter(|e| e.conflicted) {
                        let row = entry_row(ui, e, "✓");
                        if row.open {
                            open = Some((root.join(&e.rel), false));
                        }
                        // ✓ = "I fixed the markers by hand" → git add.
                        if row.action {
                            act = Some(RowAct::MarkResolved(e.rel.clone()));
                        }
                        let rel = &e.rel;
                        row.resp.context_menu(|ui| {
                            if ui.button("Take OURS (current branch)").clicked_by(egui::PointerButton::Primary) {
                                act = Some(RowAct::TakeSide(rel.clone(), "--ours"));
                                ui.close_menu();
                            }
                            if ui.button("Take THEIRS (incoming)").clicked_by(egui::PointerButton::Primary) {
                                act = Some(RowAct::TakeSide(rel.clone(), "--theirs"));
                                ui.close_menu();
                            }
                            if ui.button("Mark resolved (keep as-is)").clicked_by(egui::PointerButton::Primary) {
                                act = Some(RowAct::MarkResolved(rel.clone()));
                                ui.close_menu();
                            }
                        });
                    }
                }

                let unstage_all = (staged_count > 0 && !busy).then_some("Unstage all");
                if section_header(ui, "Staged", staged_count, unstage_all) {
                    act = Some(RowAct::UnstageAll);
                }
                for e in self.entries.iter().filter(|e| e.staged) {
                    let row = entry_row(ui, e, "−");
                    if row.open {
                        open = Some((root.join(&e.rel), true));
                    }
                    if row.action {
                        act = Some(RowAct::Unstage(e.rel.clone()));
                    }
                }
                if staged_count == 0 {
                    faint_row(ui, "nothing staged");
                }

                let stage_all = (changed_count > 0 && !busy).then_some("Stage all");
                if section_header(ui, "Changes", changed_count, stage_all) {
                    act = Some(RowAct::StageAll);
                }
                for e in self.entries.iter().filter(|e| !e.staged && !e.conflicted) {
                    let row = entry_row(ui, e, "+");
                    if row.open {
                        open = Some((root.join(&e.rel), false));
                    }
                    if row.action {
                        act = Some(RowAct::Stage(e.rel.clone()));
                    }
                    // Discard is destructive → two-step confirm submenu, like the tree's
                    // Delete. Untracked rows get no discard (checkout can't touch them).
                    if e.state != '?' {
                        let rel = &e.rel;
                        let name = rel
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| rel.display().to_string());
                        row.resp.context_menu(|ui| {
                            ui.menu_button("Discard changes", |ui| {
                                if ui.button(format!("Yes, discard {name}")).clicked_by(egui::PointerButton::Primary) {
                                    act = Some(RowAct::Discard(rel.clone()));
                                    ui.close_menu();
                                }
                            });
                        });
                    }
                }
                if changed_count == 0 {
                    faint_row(ui, "no changes");
                }

                // ---- STASHES: apply / pop / drop (drop is two-step confirmed) ----------------
                if !self.stashes.is_empty() {
                    section_header(ui, "Stashes", self.stashes.len(), None);
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    for st in &self.stashes {
                        ui.horizontal(|ui| {
                            ui.add_space(PAD);
                            ui.colored_label(
                                colors::TEXT_FAINT(),
                                egui::RichText::new(&st.refname).monospace().size(11.0),
                            );
                            let label = format!(
                                "{} — {}",
                                st.subject,
                                crate::blame::relative_time(now, st.time)
                            );
                            ui.colored_label(colors::TEXT_MUTED(), egui::RichText::new(label).size(sizes::FONT_TREE));
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    let armed = self.armed_drop.as_deref() == Some(st.refname.as_str());
                                    let drop_label = if armed { "Really drop?" } else { "Drop" };
                                    if ui
                                        .add_enabled(!busy, egui::Button::new(drop_label).small())
                                        .clicked_by(egui::PointerButton::Primary)
                                    {
                                        if armed {
                                            self.armed_drop = None;
                                            act = Some(RowAct::StashDrop(st.refname.clone()));
                                        } else {
                                            self.armed_drop = Some(st.refname.clone());
                                        }
                                    }
                                    if ui
                                        .add_enabled(!busy, egui::Button::new("Apply").small())
                                        .on_hover_text("apply, keep the stash")
                                        .clicked_by(egui::PointerButton::Primary)
                                    {
                                        self.armed_drop = None;
                                        act = Some(RowAct::StashApply(st.refname.clone()));
                                    }
                                    if ui
                                        .add_enabled(!busy, egui::Button::new("Pop").small())
                                        .on_hover_text("apply and drop")
                                        .clicked_by(egui::PointerButton::Primary)
                                    {
                                        self.armed_drop = None;
                                        act = Some(RowAct::StashPop(st.refname.clone()));
                                    }
                                },
                            );
                        });
                    }
                }
            },
        );

        // ---- footer: commit box + [Commit] + last output/error ------------------------------
        ui.add_space(4.0);
        hairline(ui);
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.add_space(PAD);
            let w = ui.available_width() - PAD;
            ui.add(
                egui::TextEdit::multiline(&mut self.commit_msg)
                    .desired_rows(3)
                    .desired_width(w)
                    .hint_text("commit message"),
            );
        });
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.add_space(PAD);
            let staged_any = self.entries.iter().any(|e| e.staged);
            let can_commit = !busy && staged_any && !self.commit_msg.trim().is_empty();
            if ui.add_enabled(can_commit, egui::Button::new("Commit").small()).clicked_by(egui::PointerButton::Primary) {
                let mut args = git_args(&["commit", "-m"]);
                args.push(OsString::from(self.commit_msg.trim()));
                self.run_action(root, ui.ctx(), args, true);
            }
            // Draft a message from the staged diff. Fills the box for editing — never commits.
            let can_ai = !busy && staged_any && !self.ai_msg_inflight;
            if ui
                .add_enabled(can_ai, egui::Button::new("✨ AI message").small())
                .on_hover_text("Draft a commit message from the staged changes (Settings ▸ AI picks the backend)")
                .clicked_by(egui::PointerButton::Primary)
            {
                self.generate_commit_msg(root, ui.ctx());
            }
            if self.ai_msg_inflight {
                ui.spinner();
            }
        });
        let footer_msg = self
            .last_error
            .as_ref()
            .map(|e| (e.clone(), colors::ERROR()))
            .or_else(|| self.last_output.as_ref().map(|o| (o.clone(), colors::TEXT_FAINT())));
        if let Some((text, color)) = footer_msg {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.add_space(PAD);
                ui.label(
                    egui::RichText::new(text)
                        .size(sizes::FONT_PANEL_HEADER)
                        .monospace()
                        .color(color),
                );
            });
        }

        // ---- apply the collected action (after all self.entries borrows have ended) ---------
        if let Some(a) = act {
            if !busy {
                // Conflict resolution needs TWO commands in order (checkout side → add);
                // the generic single-command match below can't express it.
                if let RowAct::TakeSide(rel, side) = &a {
                    let mut co = git_args(&["checkout", side, "--"]);
                    co.push(crate::diffview::literal_pathspec(rel));
                    let mut add = git_args(&["add", "--"]);
                    add.push(crate::diffview::literal_pathspec(rel));
                    self.run_action_seq(root, ui.ctx(), vec![co, add]);
                    return open;
                }
                if let RowAct::MarkResolved(rel) = &a {
                    let mut add = git_args(&["add", "--"]);
                    add.push(crate::diffview::literal_pathspec(rel));
                    self.run_action(root, ui.ctx(), add, false);
                    return open;
                }
                let args = match a {
                    RowAct::TakeSide(..) | RowAct::MarkResolved(..) => unreachable!("handled above"),
                    // `:(literal)` on every per-file pathspec: raw names still glob after `--`,
                    // so `log[1].txt` would stage/discard `log1.txt` too — and Discard is
                    // destructive (crate::diffview::literal_pathspec documents the hazard).
                    RowAct::Stage(rel) => {
                        let mut v = git_args(&["add", "--"]);
                        v.push(crate::diffview::literal_pathspec(&rel));
                        v
                    }
                    RowAct::Unstage(rel) => {
                        let mut v = git_args(&["restore", "--staged", "--"]);
                        v.push(crate::diffview::literal_pathspec(&rel));
                        v
                    }
                    RowAct::Discard(rel) => {
                        let mut v = git_args(&["checkout", "--"]);
                        v.push(crate::diffview::literal_pathspec(&rel));
                        v
                    }
                    RowAct::StageAll => git_args(&["add", "-A"]),
                    RowAct::UnstageAll => git_args(&["restore", "--staged", "--", "."]),
                    RowAct::StashPush => git_args(&["stash", "push"]),
                    RowAct::StashApply(r) => {
                        let mut v = git_args(&["stash", "apply"]);
                        v.push(r.into());
                        v
                    }
                    RowAct::StashPop(r) => {
                        let mut v = git_args(&["stash", "pop"]);
                        v.push(r.into());
                        v
                    }
                    RowAct::StashDrop(r) => {
                        let mut v = git_args(&["stash", "drop"]);
                        v.push(r.into());
                        v
                    }
                };
                self.run_action(root, ui.ctx(), args, false);
            }
        }
        open
    }
}

// ---------------------------------------------------------------------------------------------
// row + section painting (pure egui; no fs / subprocess)
// ---------------------------------------------------------------------------------------------

/// What one painted row reported back.
struct RowOut {
    /// The row body was clicked — open the file.
    open: bool,
    /// The +/− glyph was clicked — stage/unstage this row.
    action: bool,
    /// The row body's response (context menus).
    resp: egui::Response,
}

/// One dense 18px status row: colored state char, root-relative path, and a +/− action slot
/// on the right. Full-row hover wash; the path clips against the action slot.
fn entry_row(ui: &mut egui::Ui, e: &Entry, glyph: &str) -> RowOut {
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::click());
    let slot = Rect::from_center_size(
        Pos2::new(rect.right() - 4.0 - ACTION_W * 0.5, rect.center().y),
        Vec2::splat(ACTION_W),
    );
    let slot_resp = ui.interact(slot, resp.id.with("act"), Sense::click());
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        if resp.hovered() || slot_resp.hovered() {
            p.rect_filled(rect, 0.0, colors::HOVER_WASH());
        }
        p.text(
            Pos2::new(rect.left() + PAD, rect.center().y),
            Align2::LEFT_CENTER,
            e.state,
            FontId::monospace(12.0),
            state_color(e.state),
        );
        let text_left = rect.left() + PAD + 14.0;
        let clip = Rect::from_min_max(
            Pos2::new(text_left, rect.top()),
            Pos2::new(slot.left() - 4.0, rect.bottom()),
        );
        p.with_clip_rect(clip).text(
            Pos2::new(text_left, rect.center().y),
            Align2::LEFT_CENTER,
            e.rel.display().to_string(),
            FontId::proportional(sizes::FONT_TREE),
            if resp.hovered() { colors::TEXT() } else { colors::TEXT_MUTED() },
        );
        if slot_resp.hovered() {
            p.rect_filled(slot.shrink(1.0), Rounding::same(3.0), colors::HOVER_WASH());
        }
        p.text(
            slot.center(),
            Align2::CENTER_CENTER,
            glyph,
            FontId::monospace(12.0),
            if slot_resp.hovered() { colors::TEXT() } else { colors::TEXT_FAINT() },
        );
    }
    let action = slot_resp.clicked_by(egui::PointerButton::Primary);
    RowOut { open: resp.clicked_by(egui::PointerButton::Primary) && !action, action, resp }
}

/// Section strip: 11px caps header + count, optional bulk-action small button on the right.
/// Returns true when that button was clicked.
fn section_header(ui: &mut egui::Ui, label: &str, count: usize, button: Option<&str>) -> bool {
    let mut clicked = false;
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.add_space(PAD);
        panel_header_inline(ui, label);
        ui.colored_label(colors::TEXT_FAINT(), format!("{count}"));
        if let Some(b) = button {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button(b).clicked_by(egui::PointerButton::Primary) {
                    clicked = true;
                }
            });
        }
    });
    ui.add_space(2.0);
    clicked
}

/// The empty-section placeholder line.
fn faint_row(ui: &mut egui::Ui, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(PAD + 14.0);
        ui.label(
            egui::RichText::new(text).size(sizes::FONT_PANEL_HEADER).color(colors::TEXT_FAINT()),
        );
    });
}

/// Status-char tint, matching the workspace tree's conventions (A/? moss, M amber, D red,
/// R/C plum). Status colors only — never the interaction accent.
fn state_color(state: char) -> Color32 {
    match state {
        'A' | '?' => colors::MOSS(),
        'M' | 'T' => colors::AMBER(),
        'D' | 'U' => colors::ERROR(),
        'R' | 'C' => colors::PLUM(),
        _ => colors::TEXT_MUTED(),
    }
}

// ---------------------------------------------------------------------------------------------
// subprocess workers (background threads only) + their pure parsing
// ---------------------------------------------------------------------------------------------

/// `&str` args → owned `Command` args (paths get appended separately as `OsString`s).
fn git_args(parts: &[&str]) -> Vec<OsString> {
    parts.iter().map(OsString::from).collect()
}

/// Gather everything the panel shows, off-thread: porcelain status, branch from `.git/HEAD`
/// (same convention as `workspace::git_branch`), and ahead/behind vs upstream. Not a repo /
/// git missing / no upstream all degrade quietly (empty list / `None`s).
fn gather_status(seq: u64, root: &Path) -> Msg {
    let entries = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain=v1", "-z"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| parse_porcelain_entries(&o.stdout))
        .unwrap_or_default();
    let branch =
        std::fs::read_to_string(root.join(".git/HEAD")).ok().and_then(|s| parse_branch(&s));
    let upstream = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-list", "--left-right", "--count", "@{upstream}...HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| parse_ahead_behind(&String::from_utf8_lossy(&o.stdout)));
    let branches = list_branches(root);
    let stashes = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["stash", "list", "--pretty=format:%gd\x1f%at\x1f%gs\x1e"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| parse_stash_list(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default();
    Msg::Status { seq, branch, upstream, entries, branches, stashes }
}

/// Local branch names, current first then alphabetical — the switcher's menu. Empty on any error.
fn list_branches(root: &Path) -> Vec<String> {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["for-each-ref", "--format=%(refname:short)", "--sort=-committerdate", "refs/heads"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Fold a finished `git` command into footer feedback: `(output, error)` — success keeps the
/// (clipped) combined output when non-empty; failure surfaces stderr/stdout or the exit code.
fn summarize_output(result: std::io::Result<std::process::Output>) -> (Option<String>, Option<String>) {
    match result {
        Err(e) => (None, Some(format!("git failed to start: {e}"))),
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let stderr = String::from_utf8_lossy(&o.stderr);
            let combined = format!("{}\n{}", stdout.trim(), stderr.trim());
            let combined = combined.trim();
            if o.status.success() {
                let out = (!combined.is_empty()).then(|| clip_output(combined));
                (out, None)
            } else if combined.is_empty() {
                (None, Some(format!("git exited with {}", o.status)))
            } else {
                (None, Some(clip_output(combined)))
            }
        }
    }
}

/// Bound footer text: at most 3 lines / 300 chars, with a trailing ellipsis when clipped.
fn clip_output(s: &str) -> String {
    const MAX_LINES: usize = 3;
    const MAX_CHARS: usize = 300;
    let mut out = s.lines().take(MAX_LINES).collect::<Vec<_>>().join("\n");
    let mut truncated = s.lines().count() > MAX_LINES;
    if out.chars().count() > MAX_CHARS {
        out = out.chars().take(MAX_CHARS).collect();
        truncated = true;
    }
    if truncated {
        out.push_str(" …");
    }
    out
}

/// Parse `status --porcelain=v1 -z`: NUL-separated `XY path` records; rename/copy records are
/// followed by a second NUL-terminated ORIGINAL path (consumed and dropped, like the
/// workspace parser). Row model: index column ∉ {' ', '?'} → a staged row; worktree column
/// ∉ {' ', '?'} → an unstaged row; `??` → one unstaged `?` row. `!!` (ignored) is skipped.
fn parse_porcelain_entries(bytes: &[u8]) -> Vec<Entry> {
    let mut out = Vec::new();
    let mut records = bytes.split(|&b| b == 0);
    while let Some(rec) = records.next() {
        if rec.len() < 4 || rec[2] != b' ' {
            continue; // trailing empty chunk / malformed record
        }
        let (x, y) = (rec[0] as char, rec[1] as char);
        // Rename/copy: the NEXT chunk is the original path — swallow it before anything else.
        if x == 'R' || x == 'C' || y == 'R' || y == 'C' {
            let _ = records.next();
        }
        if x == '!' {
            continue; // ignored entries (only with --ignored) — not ours to show
        }
        let rel = PathBuf::from(String::from_utf8_lossy(&rec[3..]).as_ref());
        // Unmerged combinations (git-status(1)): both-modified UU, both-added AA,
        // both-deleted DD, and the one-sided U forms. ONE conflict row — the raw columns
        // used to satisfy the staged test below and paint a bogus Staged row.
        let conflict = x == 'U' || y == 'U' || (x == 'A' && y == 'A') || (x == 'D' && y == 'D');
        if conflict {
            out.push(Entry {
                rel,
                staged: false,
                state: 'U',
                staged_state: x,
                worktree_state: y,
                conflicted: true,
            });
            continue;
        }
        if x == '?' {
            out.push(Entry { rel, staged: false, state: '?', staged_state: x, worktree_state: y, conflicted: false });
            continue;
        }
        if x != ' ' {
            out.push(Entry {
                rel: rel.clone(),
                staged: true,
                state: x,
                staged_state: x,
                worktree_state: y,
                conflicted: false,
            });
        }
        if y != ' ' && y != '?' {
            out.push(Entry { rel, staged: false, state: y, staged_state: x, worktree_state: y, conflicted: false });
        }
    }
    out
}

/// Branch from `.git/HEAD` CONTENT (same convention as `workspace::git_branch`):
/// `ref: refs/heads/<name>` → the name; detached → the hash's first 8 chars; empty → None.
fn parse_branch(head: &str) -> Option<String> {
    let head = head.trim();
    if head.is_empty() {
        return None;
    }
    if let Some(rest) = head.strip_prefix("ref: refs/heads/") {
        return Some(rest.to_string());
    }
    Some(head.chars().take(8).collect())
}

/// Parse `git rev-list --left-right --count @{upstream}...HEAD` output (`"<left>\t<right>"`).
/// LEFT counts upstream-only commits (= behind), RIGHT counts HEAD-only (= ahead) — returned
/// as `(ahead, behind)`, the display order.
fn parse_ahead_behind(s: &str) -> Option<(u32, u32)> {
    let (left, right) = s.trim().split_once('\t')?;
    let behind: u32 = left.trim().parse().ok()?;
    let ahead: u32 = right.trim().parse().ok()?;
    Some((ahead, behind))
}

// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Unmerged combinations produce ONE Conflicts row — never a bogus Staged row (the old
    /// parser read UU's index column as "staged with state U").
    #[test]
    fn porcelain_conflicts_parse_as_conflict_rows() {
        let bytes = b"UU both.c\0AA added.c\0DD gone.c\0AU one_side.c\0M  staged.c\0".to_vec();
        let e = parse_porcelain_entries(&bytes);
        let conflicts: Vec<_> = e.iter().filter(|e| e.conflicted).collect();
        assert_eq!(conflicts.len(), 4);
        assert!(conflicts.iter().all(|c| !c.staged && c.state == 'U'));
        // The genuinely staged row still parses; no conflict path leaked into Staged.
        assert_eq!(e.iter().filter(|e| e.staged).count(), 1);
        assert!(e.iter().filter(|e| e.staged).all(|e| e.rel == PathBuf::from("staged.c")));
    }

    /// The commit prompt embeds stat/diff/style and survives a multibyte char at the cap.
    #[test]
    fn commit_prompt_shapes() {
        let p = commit_prompt(" a.rs | 2 +-\n", "diff --git a/a.rs b/a.rs\n+fn x() {}\n", "feat: one\nfix: two");
        assert!(p.contains("Recent commit subjects"));
        assert!(p.contains("feat: one"));
        assert!(p.contains("+fn x() {}"));
        assert!(!p.contains("truncated"));
        // No history → no style block.
        let p = commit_prompt("s", "d", "  ");
        assert!(!p.contains("Recent commit subjects"));
        // Oversized diff is cut at a char boundary, never panicking on multibyte.
        let big = format!("{}é", "x".repeat(11_999));
        let p = commit_prompt("s", &big, "");
        assert!(p.contains("(diff truncated)"));
    }

    /// Create a throwaway git repo with one commit, returning its root. `None` if git is absent.
    fn temp_repo(tag: &str) -> Option<PathBuf> {
        let dir = std::env::temp_dir().join(format!("cauldron-git-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).ok()?;
        let git = |args: &[&str]| {
            Command::new("git").arg("-C").arg(&dir).args(args).output().map(|o| o.status.success())
        };
        git(&["init", "-q"]).ok()?;
        let _ = git(&["config", "user.email", "t@t"]);
        let _ = git(&["config", "user.name", "t"]);
        std::fs::write(dir.join("f.txt"), "x").ok()?;
        let _ = git(&["add", "."]);
        let _ = git(&["commit", "-qm", "init"]);
        Some(dir)
    }

    #[test]
    fn stash_list_parses_and_skips_garbage() {
        let text = "stash@{0}\x1f1700000000\x1fWIP on main: abc fix thing\x1e\nstash@{1}\x1f1690000000\x1fOn feat: saved work\x1e";
        let st = parse_stash_list(text);
        assert_eq!(st.len(), 2);
        assert_eq!(st[0].refname, "stash@{0}");
        assert_eq!(st[0].time, 1_700_000_000);
        assert_eq!(st[0].subject, "WIP on main: abc fix thing");
        assert!(parse_stash_list("").is_empty());
        assert!(parse_stash_list("junk\x1fnope\x1e").is_empty());
    }

    #[test]
    fn stash_roundtrip_against_a_real_repo() {
        let Some(dir) = temp_repo("stash") else { return };
        let git = |args: &[&str]| {
            Command::new("git").arg("-C").arg(&dir).args(args).output().map(|o| o.status.success())
        };
        // Dirty the tracked file, stash it, list must parse one entry.
        std::fs::write(dir.join("f.txt"), "changed").unwrap();
        assert!(git(&["stash", "push"]).unwrap());
        let out = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["stash", "list", "--pretty=format:%gd\x1f%at\x1f%gs\x1e"])
            .output()
            .unwrap();
        let stashes = parse_stash_list(&String::from_utf8_lossy(&out.stdout));
        assert_eq!(stashes.len(), 1);
        assert_eq!(stashes[0].refname, "stash@{0}");
        // Worktree is clean again…
        assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), "x");
        // …and pop restores the change and empties the list.
        assert!(git(&["stash", "pop", "stash@{0}"]).unwrap());
        assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), "changed");
        let out = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["stash", "list", "--pretty=format:%gd\x1f%at\x1f%gs\x1e"])
            .output()
            .unwrap();
        assert!(parse_stash_list(&String::from_utf8_lossy(&out.stdout)).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_branches_returns_local_branches() {
        let Some(dir) = temp_repo("branches") else { return };
        let git = |args: &[&str]| {
            let _ = Command::new("git").arg("-C").arg(&dir).args(args).output();
        };
        git(&["branch", "feature-a"]);
        git(&["branch", "feature-b"]);
        let branches = list_branches(&dir);
        assert!(branches.contains(&"feature-a".to_string()), "{branches:?}");
        assert!(branches.contains(&"feature-b".to_string()), "{branches:?}");
        // The default branch (main/master) is present too — 3 total, no blanks.
        assert_eq!(branches.len(), 3, "{branches:?}");
        assert!(branches.iter().all(|b| !b.is_empty()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn checkout_switches_the_branch_gather_reports_it() {
        let Some(dir) = temp_repo("checkout") else { return };
        let _ = Command::new("git").arg("-C").arg(&dir).args(["checkout", "-qb", "dev"]).output();
        // gather_status reads .git/HEAD, so it must now report `dev`.
        let Msg::Status { branch, branches, .. } = gather_status(0, &dir) else {
            panic!("expected Status");
        };
        assert_eq!(branch.as_deref(), Some("dev"));
        assert!(branches.contains(&"dev".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn porcelain_worktree_only_is_one_unstaged_row() {
        let rows = parse_porcelain_entries(b" M src/main.rs\0");
        assert_eq!(rows.len(), 1);
        let e = &rows[0];
        assert_eq!(e.rel, PathBuf::from("src/main.rs"));
        assert!(!e.staged);
        assert_eq!(e.state, 'M');
        assert_eq!((e.staged_state, e.worktree_state), (' ', 'M'));
    }

    #[test]
    fn porcelain_index_only_is_one_staged_row() {
        let rows = parse_porcelain_entries(b"M  a.rs\0A  b.rs\0D  c.rs\0");
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|e| e.staged));
        let states: Vec<char> = rows.iter().map(|e| e.state).collect();
        assert_eq!(states, ['M', 'A', 'D']);
        assert_eq!((rows[0].staged_state, rows[0].worktree_state), ('M', ' '));
    }

    #[test]
    fn porcelain_both_columns_yields_two_rows() {
        let rows = parse_porcelain_entries(b"MM both.rs\0AM new_edited.rs\0");
        assert_eq!(rows.len(), 4);
        let both: Vec<&Entry> =
            rows.iter().filter(|e| e.rel == PathBuf::from("both.rs")).collect();
        assert_eq!(both.len(), 2);
        assert!(both[0].staged && both[0].state == 'M');
        assert!(!both[1].staged && both[1].state == 'M');
        // BOTH rows keep the raw two-column record.
        assert_eq!((both[0].staged_state, both[0].worktree_state), ('M', 'M'));
        assert_eq!((both[1].staged_state, both[1].worktree_state), ('M', 'M'));
        let ne: Vec<&Entry> =
            rows.iter().filter(|e| e.rel == PathBuf::from("new_edited.rs")).collect();
        assert!(ne[0].staged && ne[0].state == 'A');
        assert!(!ne[1].staged && ne[1].state == 'M');
    }

    #[test]
    fn porcelain_untracked_is_unstaged_question() {
        let rows = parse_porcelain_entries(b"?? new.txt\0");
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].staged);
        assert_eq!(rows[0].state, '?');
        assert_eq!(rows[0].rel, PathBuf::from("new.txt"));
    }

    #[test]
    fn porcelain_rename_consumes_original_and_stays_in_sync() {
        let rows = parse_porcelain_entries(b"R  new_name.rs\0old_name.rs\0 M after.rs\0");
        assert_eq!(rows.len(), 2);
        assert!(rows[0].staged);
        assert_eq!(rows[0].state, 'R');
        assert_eq!(rows[0].rel, PathBuf::from("new_name.rs"));
        assert!(rows.iter().all(|e| e.rel != PathBuf::from("old_name.rs")));
        // Parsing stays in sync after the double record.
        assert_eq!(rows[1].rel, PathBuf::from("after.rs"));
        assert!(!rows[1].staged);
    }

    #[test]
    fn porcelain_rename_with_worktree_edits_is_two_rows() {
        let rows = parse_porcelain_entries(b"RM ren.rs\0orig.rs\0");
        assert_eq!(rows.len(), 2);
        assert!(rows[0].staged && rows[0].state == 'R');
        assert!(!rows[1].staged && rows[1].state == 'M');
        assert!(rows.iter().all(|e| e.rel == PathBuf::from("ren.rs")));
    }

    #[test]
    fn porcelain_empty_and_garbage_safe() {
        assert!(parse_porcelain_entries(b"").is_empty());
        assert!(parse_porcelain_entries(b"\0\0").is_empty());
        assert!(parse_porcelain_entries(b"xy").is_empty());
        assert!(parse_porcelain_entries(b"!! ignored.o\0").is_empty());
    }

    #[test]
    fn branch_from_head_content() {
        assert_eq!(parse_branch("ref: refs/heads/rune\n").as_deref(), Some("rune"));
        assert_eq!(parse_branch("ref: refs/heads/feat/x\n").as_deref(), Some("feat/x"));
        assert_eq!(
            parse_branch("0123456789abcdef0123456789abcdef01234567\n").as_deref(),
            Some("01234567") // detached: short hash
        );
        assert_eq!(parse_branch(""), None);
        assert_eq!(parse_branch("   \n"), None);
    }

    #[test]
    fn ahead_behind_from_rev_list_output() {
        // `@{upstream}...HEAD`: LEFT = upstream-only (behind), RIGHT = HEAD-only (ahead).
        assert_eq!(parse_ahead_behind("2\t3\n"), Some((3, 2)));
        assert_eq!(parse_ahead_behind("0\t0"), Some((0, 0)));
        assert_eq!(parse_ahead_behind(""), None);
        assert_eq!(parse_ahead_behind("nope"), None);
        assert_eq!(parse_ahead_behind("a\tb\n"), None);
    }

    #[test]
    fn clip_output_bounds_footer_text() {
        assert_eq!(clip_output("ok"), "ok");
        let long: String = "line\n".repeat(10);
        let clipped = clip_output(&long);
        assert!(clipped.lines().count() <= 4);
        assert!(clipped.ends_with('…'));
    }
}
