//! Commit history — the JetBrains-style Log tab: a paginated commit list on the left, the
//! selected commit's message + changed files on the right. Clicking a changed file hands
//! `(abs path, sha)` back to the app, which opens that commit's diff in the diff viewer.
//!
//! Same layering as blame/git: PURE parsers over `git log` / `git show --name-status` output
//! (unit-tested, separator-based so no format ambiguity), and a background service (thread +
//! mpsc) so neither the log nor a commit's file list ever stalls a frame.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};

use egui::{Color32, RichText};

use crate::style::{colors, sizes};

/// Commits fetched per page (the "Load more" step).
const PAGE: usize = 200;
const ROW_H: f32 = 20.0;

// =================================================================================================
// pure parsers
// =================================================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub sha: String,
    pub author: String,
    /// Unix seconds (author time).
    pub time: i64,
    pub subject: String,
}

impl Commit {
    pub fn short(&self) -> &str {
        &self.sha[..self.sha.len().min(8)]
    }
}

/// Parse `git log --pretty=format:%H%x1f%an%x1f%at%x1f%s%x1e` output. Unit (0x1f) and record
/// (0x1e) separators cannot appear in shas/names/subjects, so parsing is unambiguous.
/// Garbage-safe: malformed records are skipped.
pub fn parse_log(text: &str) -> Vec<Commit> {
    text.split('\x1e')
        .filter_map(|rec| {
            let rec = rec.trim_start_matches(['\n', '\r']);
            let mut f = rec.split('\x1f');
            let sha = f.next()?.trim().to_string();
            if sha.len() < 7 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            Some(Commit {
                sha,
                author: f.next()?.to_string(),
                time: f.next()?.trim().parse().ok()?,
                subject: f.next().unwrap_or("").to_string(),
            })
        })
        .collect()
}

/// One changed file in a commit: `(status char, repo-relative path)`. Renames/copies
/// (`R100\told\tnew`) report the NEW path with status 'R'/'C'.
pub fn parse_name_status(text: &str) -> Vec<(char, PathBuf)> {
    text.lines()
        .filter_map(|l| {
            let mut cols = l.split('\t');
            let status = cols.next()?.trim();
            let first = status.chars().next()?;
            if !first.is_ascii_uppercase() {
                return None;
            }
            let path = match first {
                'R' | 'C' => cols.nth(1)?, // old \t NEW
                _ => cols.next()?,
            };
            (!path.is_empty()).then(|| (first, PathBuf::from(path)))
        })
        .collect()
}

fn status_color(c: char) -> Color32 {
    match c {
        'A' => colors::MOSS(),
        'D' => colors::ERROR(),
        'R' | 'C' => colors::PLUM(),
        _ => colors::AMBER(), // M, T, …
    }
}

// =================================================================================================
// background service
// =================================================================================================

enum Msg {
    /// A log page arrived: (skip it was fetched at, commits).
    Page(usize, Vec<Commit>),
    /// A commit's changed files arrived.
    Files(String, Vec<(char, PathBuf)>),
}

/// A repo action chosen from a commit's right-click menu, handed to the app to run through
/// the git machinery (with the git + history panels refreshed after).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryAction {
    /// `git cherry-pick <sha>` — replay this commit onto the current branch.
    CherryPick(String),
    /// `git revert --no-edit <sha>` — a new commit that undoes this one.
    Revert(String),
    /// `git reset --soft <sha>` — move HEAD here, keep the index and working tree (safe).
    SoftReset(String),
}

pub struct HistoryPanel {
    commits: Vec<Commit>,
    /// Index into `commits` of the selected row.
    selected: Option<usize>,
    /// A chosen commit action awaiting the app (drained via [`Self::take_action`]).
    pending_action: Option<HistoryAction>,
    /// sha → changed files. `None` = fetch in flight; `Some(vec![])` = genuinely no files
    /// (empty commit). The two must stay distinguishable — an empty Vec doubling as the
    /// loading sentinel left every clean merge commit on "loading files…" forever.
    files: HashMap<String, Option<Vec<(char, PathBuf)>>>,
    /// True while a log page / file list is being fetched.
    loading: bool,
    /// All pages exhausted (a short page came back).
    exhausted: bool,
    /// The root the current state was gathered from; a change resets everything.
    root: Option<PathBuf>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
}

impl Default for HistoryPanel {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            commits: Vec::new(),
            selected: None,
            pending_action: None,
            files: HashMap::new(),
            loading: false,
            exhausted: false,
            root: None,
            tx,
            rx,
        }
    }
}

impl HistoryPanel {
    /// A repo-mutating action landed (commit/pull/checkout): lazily refetch on next show.
    pub fn mark_stale(&mut self) {
        self.root = None;
    }

    /// The commit action chosen from a right-click menu, once (the app runs it through git).
    pub fn take_action(&mut self) -> Option<HistoryAction> {
        self.pending_action.take()
    }

    /// Drop everything and refetch the first page (↻ button, project/branch switch).
    pub fn refresh(&mut self, root: &Path, ctx: &egui::Context) {
        self.commits.clear();
        self.selected = None;
        self.files.clear();
        self.exhausted = false;
        self.root = Some(root.to_path_buf());
        self.fetch_page(root, 0, ctx);
    }

    fn fetch_page(&mut self, root: &Path, skip: usize, ctx: &egui::Context) {
        if self.loading {
            return;
        }
        self.loading = true;
        let tx = self.tx.clone();
        let root = root.to_path_buf();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("git-log".into())
            .spawn(move || {
                let out = std::process::Command::new("git")
                    .arg("-C")
                    .arg(&root)
                    .args([
                        "log",
                        "--no-color",
                        "--pretty=format:%H\x1f%an\x1f%at\x1f%s\x1e",
                        &format!("-n{PAGE}"),
                        &format!("--skip={skip}"),
                    ])
                    .output();
                let commits = out
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|o| parse_log(&String::from_utf8_lossy(&o.stdout)))
                    .unwrap_or_default();
                let _ = tx.send(Msg::Page(skip, commits));
                ctx.request_repaint();
            });
        if spawned.is_err() {
            self.loading = false; // no worker → no Msg::Page will ever clear it
        }
    }

    fn fetch_files(&mut self, root: &Path, sha: &str, ctx: &egui::Context) {
        if self.files.contains_key(sha) {
            return;
        }
        self.files.insert(sha.to_string(), None); // fetch in flight
        let tx = self.tx.clone();
        let root = root.to_path_buf();
        let sha = sha.to_string();
        let sha2 = sha.clone();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("git-show".into())
            .spawn(move || {
                // -m --first-parent: a clean merge commit has NO combined diff, so plain `show`
                // prints nothing — list its changes against the first parent instead (what
                // JetBrains shows). No-op for ordinary commits.
                let out = std::process::Command::new("git")
                    .arg("-C")
                    .arg(&root)
                    .args([
                        "show",
                        "--no-color",
                        "--name-status",
                        "--format=",
                        "-m",
                        "--first-parent",
                        &sha,
                    ])
                    .output();
                let files = out
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|o| parse_name_status(&String::from_utf8_lossy(&o.stdout)))
                    .unwrap_or_default();
                let _ = tx.send(Msg::Files(sha, files));
                ctx.request_repaint();
            });
        if spawned.is_err() {
            self.files.remove(&sha2); // no worker → the None placeholder would never resolve
        }
    }

    fn pump(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Page(skip, mut commits) => {
                    self.loading = false;
                    if commits.len() < PAGE {
                        self.exhausted = true;
                    }
                    // Defensive: a page landing out of order must not duplicate rows.
                    if skip == self.commits.len() {
                        self.commits.append(&mut commits);
                    }
                }
                Msg::Files(sha, files) => {
                    self.files.insert(sha, Some(files));
                }
            }
        }
    }

    /// Draw the tab body. Returns `(abs file, sha)` when a changed file is clicked — the app
    /// opens that commit's diff.
    pub fn ui(&mut self, ui: &mut egui::Ui, root: &Path) -> Option<(PathBuf, String)> {
        if self.root.as_deref() != Some(root) {
            self.refresh(root, ui.ctx());
        }
        self.pump();
        let mut open: Option<(PathBuf, String)> = None;

        ui.horizontal(|ui| {
            ui.add_space(6.0);
            crate::style::panel_header_inline(ui, "Log");
            ui.colored_label(colors::TEXT_FAINT(), format!("{}", self.commits.len()));
            if ui.small_button("↻").clicked_by(egui::PointerButton::Primary) {
                self.refresh(root, ui.ctx());
            }
            if self.loading {
                ui.spinner();
            }
        });
        ui.separator();

        let avail = ui.available_height();
        ui.horizontal_top(|ui| {
            // --- left: the commit list (virtualized) ------------------------------------------
            let list_w = (ui.available_width() * 0.55).max(280.0);
            ui.allocate_ui(egui::vec2(list_w, avail), |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                egui::ScrollArea::vertical().id_salt("hist-list").auto_shrink([false, false]).show_rows(
                    ui,
                    ROW_H,
                    self.commits.len() + 1, // +1 for the load-more / end row
                    |ui, range| {
                        for i in range {
                            if i >= self.commits.len() {
                                if self.exhausted {
                                    ui.colored_label(colors::TEXT_FAINT(), "  — end of history —");
                                } else if ui
                                    .add_enabled(!self.loading, egui::Button::new("Load more…").small())
                                    .clicked_by(egui::PointerButton::Primary)
                                {
                                    let skip = self.commits.len();
                                    self.fetch_page(root, skip, ui.ctx());
                                }
                                continue;
                            }
                            let c = &self.commits[i];
                            let selected = self.selected == Some(i);
                            let label = format!(
                                "{}  {}  — {}, {}",
                                c.short(),
                                c.subject,
                                c.author,
                                crate::blame::relative_time(now, c.time),
                            );
                            let text = if selected {
                                RichText::new(label).size(sizes::FONT_TREE).color(colors::ACCENT_HI())
                            } else {
                                RichText::new(label).size(sizes::FONT_TREE).color(colors::TEXT_MUTED())
                            };
                            let sha = c.sha.clone();
                            let subject = c.subject.clone();
                            let resp = ui.selectable_label(selected, text);
                            if resp.clicked_by(egui::PointerButton::Primary) {
                                self.selected = Some(i);
                                self.fetch_files(root, &sha, ui.ctx());
                            }
                            resp.context_menu(|ui| {
                                if ui.button("Cherry-pick onto current branch").clicked_by(egui::PointerButton::Primary) {
                                    self.pending_action = Some(HistoryAction::CherryPick(sha.clone()));
                                    ui.close_menu();
                                }
                                if ui.button("Revert this commit").clicked_by(egui::PointerButton::Primary) {
                                    self.pending_action = Some(HistoryAction::Revert(sha.clone()));
                                    ui.close_menu();
                                }
                                if ui
                                    .button("Soft-reset HEAD to here")
                                    .on_hover_text("Move the branch to this commit; keep all changes staged (safe)")
                                    .clicked_by(egui::PointerButton::Primary)
                                {
                                    self.pending_action = Some(HistoryAction::SoftReset(sha.clone()));
                                    ui.close_menu();
                                }
                                ui.separator();
                                if ui.button("Copy SHA").clicked_by(egui::PointerButton::Primary) {
                                    ui.output_mut(|o| o.copied_text = sha.clone());
                                    ui.close_menu();
                                }
                                if ui.button("Copy subject").clicked_by(egui::PointerButton::Primary) {
                                    ui.output_mut(|o| o.copied_text = subject.clone());
                                    ui.close_menu();
                                }
                            });
                        }
                    },
                );
            });

            ui.separator();

            // --- right: selected commit details ------------------------------------------------
            ui.vertical(|ui| {
                let Some(sel) = self.selected else {
                    ui.colored_label(colors::TEXT_FAINT(), "select a commit");
                    return;
                };
                let Some(c) = self.commits.get(sel) else { return };
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(&c.subject).color(colors::TEXT()).strong());
                });
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                ui.colored_label(
                    colors::TEXT_FAINT(),
                    format!(
                        "{} · {}, {}",
                        c.short(),
                        c.author,
                        crate::blame::relative_time(now, c.time)
                    ),
                );
                ui.add_space(4.0);
                match self.files.get(&c.sha) {
                    None | Some(None) => {
                        ui.colored_label(colors::TEXT_FAINT(), "loading files…");
                    }
                    Some(Some(files)) if files.is_empty() => {
                        ui.colored_label(colors::TEXT_FAINT(), "no changed files");
                    }
                    Some(Some(files)) => {
                        let sha = c.sha.clone();
                        egui::ScrollArea::vertical()
                            .id_salt("hist-files")
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.spacing_mut().item_spacing.y = 0.0;
                                for (st, rel) in files {
                                    ui.horizontal(|ui| {
                                        ui.add_space(2.0);
                                        ui.label(
                                            RichText::new(st.to_string())
                                                .monospace()
                                                .size(11.0)
                                                .color(status_color(*st)),
                                        );
                                        if ui
                                            .selectable_label(
                                                false,
                                                RichText::new(rel.display().to_string())
                                                    .size(sizes::FONT_TREE)
                                                    .color(colors::TEXT_MUTED()),
                                            )
                                            .clicked_by(egui::PointerButton::Primary)
                                        {
                                            open = Some((root.join(rel), sha.clone()));
                                        }
                                    });
                                }
                            });
                    }
                }
            });
        });
        open
    }
}

// =================================================================================================
// tests
// =================================================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_separator_delimited_log() {
        let text = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\x1fAlice\x1f1700000000\x1ffix: the thing\x1e\nbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\x1fBob B\x1f1690000000\x1ffeat: subject with \x1e";
        let log = parse_log(text);
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].short(), "aaaaaaaa");
        assert_eq!(log[0].author, "Alice");
        assert_eq!(log[0].time, 1_700_000_000);
        assert_eq!(log[0].subject, "fix: the thing");
        assert_eq!(log[1].author, "Bob B");
    }

    #[test]
    fn log_parser_survives_garbage() {
        assert!(parse_log("").is_empty());
        assert!(parse_log("not-a-sha\x1fA\x1f1\x1fs\x1e").is_empty());
        assert!(parse_log("\x1e\x1e\x1e").is_empty());
    }

    #[test]
    fn parses_name_status_including_renames() {
        let text = "M\tsrc/main.rs\nA\tsrc/new.rs\nD\tgone.txt\nR100\told/name.rs\tnew/name.rs\nnonsense line\n";
        let files = parse_name_status(text);
        assert_eq!(
            files,
            vec![
                ('M', PathBuf::from("src/main.rs")),
                ('A', PathBuf::from("src/new.rs")),
                ('D', PathBuf::from("gone.txt")),
                ('R', PathBuf::from("new/name.rs")),
            ]
        );
    }

    /// End-to-end against real git: two commits parse in order; name-status matches; the
    /// commit-file diff opens through diffview::open_commit. Skips when git is unavailable.
    #[test]
    fn log_and_commit_diff_against_a_real_repo() {
        let dir = std::env::temp_dir().join(format!("cauldron-history-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .output()
                .map(|o| (o.status.success(), String::from_utf8_lossy(&o.stdout).into_owned()))
                .unwrap_or((false, String::new()))
        };
        if !git(&["init", "-q"]).0 {
            return;
        }
        let _ = git(&["config", "user.email", "t@t"]);
        let _ = git(&["config", "user.name", "T"]);
        std::fs::write(dir.join("f.txt"), "one\ntwo\n").unwrap();
        let _ = git(&["add", "."]);
        let _ = git(&["commit", "-qm", "first commit"]);
        std::fs::write(dir.join("f.txt"), "one\nTWO\n").unwrap();
        let _ = git(&["add", "."]);
        let _ = git(&["commit", "-qm", "second commit"]);

        let (ok, out) = git(&[
            "log",
            "--no-color",
            "--pretty=format:%H\x1f%an\x1f%at\x1f%s\x1e",
        ]);
        assert!(ok);
        let log = parse_log(&out);
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].subject, "second commit");
        assert_eq!(log[1].subject, "first commit");

        let (ok, out) = git(&["show", "--no-color", "--name-status", "--format=", &log[0].sha]);
        assert!(ok);
        assert_eq!(parse_name_status(&out), vec![('M', PathBuf::from("f.txt"))]);

        // The commit-file diff renders through the diff viewer (second commit touches f.txt).
        let v = crate::diffview::open_commit(&dir, &dir.join("f.txt"), &log[0].sha).unwrap();
        assert!(!v.is_empty(), "commit diff has rows");
        // Root commit (no parent) also works — everything shows as added.
        let v = crate::diffview::open_commit(&dir, &dir.join("f.txt"), &log[1].sha).unwrap();
        assert!(!v.is_empty(), "root commit diff has rows");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
