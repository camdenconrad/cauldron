//! GitHub Pull Requests — the JetBrains PR tool-window analog, backed by the `gh` CLI.
//!
//! Left: the repo's open PRs (#N title — author, age, draft/review badge). Right: the selected
//! PR's meta, CI checks, and changed files; clicking a file opens THAT FILE's PR diff in the
//! diff viewer (via `gh pr diff` split per file — no checkout needed). [Checkout] switches the
//! worktree to the PR branch; [↗] opens it in the browser.
//!
//! House layering: PURE parsers over `gh --json` output (unit-tested; serde_json Value-walking,
//! no derive), one background thread per fetch (blame/history shape), seq-guarded list results.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};

use egui::RichText;

use crate::style::{colors, sizes};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pr {
    pub number: u64,
    pub title: String,
    pub author: String,
    pub head_ref: String,
    pub is_draft: bool,
    /// Unix seconds (from updatedAt).
    pub updated: i64,
    /// APPROVED / CHANGES_REQUESTED / REVIEW_REQUIRED / "" (none).
    pub review: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    /// gh buckets: pass / fail / pending / skipping / cancel.
    pub bucket: String,
}

// =================================================================================================
// pure parsers
// =================================================================================================

/// Parse `gh pr list --json number,title,author,headRefName,isDraft,updatedAt,reviewDecision`.
pub fn parse_pr_list(json: &str) -> Vec<Pr> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else { return Vec::new() };
    let Some(arr) = v.as_array() else { return Vec::new() };
    arr.iter()
        .filter_map(|p| {
            Some(Pr {
                number: p.get("number")?.as_u64()?,
                title: p.get("title")?.as_str()?.to_string(),
                author: p
                    .get("author")
                    .and_then(|a| a.get("login"))
                    .and_then(|l| l.as_str())
                    .unwrap_or("?")
                    .to_string(),
                head_ref: p
                    .get("headRefName")
                    .and_then(|h| h.as_str())
                    .unwrap_or("")
                    .to_string(),
                is_draft: p.get("isDraft").and_then(|d| d.as_bool()).unwrap_or(false),
                updated: p
                    .get("updatedAt")
                    .and_then(|u| u.as_str())
                    .and_then(iso8601_to_unix)
                    .unwrap_or(0),
                review: p
                    .get("reviewDecision")
                    .and_then(|r| r.as_str())
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect()
}

/// Parse `gh pr checks N --json name,bucket`.
pub fn parse_checks(json: &str) -> Vec<Check> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else { return Vec::new() };
    let Some(arr) = v.as_array() else { return Vec::new() };
    arr.iter()
        .filter_map(|c| {
            Some(Check {
                name: c.get("name")?.as_str()?.to_string(),
                bucket: c.get("bucket").and_then(|b| b.as_str()).unwrap_or("").to_string(),
            })
        })
        .collect()
}

/// `2026-07-16T12:34:56Z` → unix seconds. UTC only (gh emits Z); returns None on anything else.
/// Days-from-civil (Howard Hinnant's algorithm) — no chrono dependency for one timestamp format.
pub fn iso8601_to_unix(s: &str) -> Option<i64> {
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-');
    let (y, m, day): (i64, i64, i64) =
        (d.next()?.parse().ok()?, d.next()?.parse().ok()?, d.next()?.parse().ok()?);
    let mut t = time.split(':');
    let (hh, mm, ss): (i64, i64, i64) =
        (t.next()?.parse().ok()?, t.next()?.parse().ok()?, t.next()?.parse().ok()?);
    let days_in = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
            if leap { 29 } else { 28 }
        }
        _ => return None,
    };
    if !(1..=12).contains(&m)
        || !(1..=days_in).contains(&day)
        || !(0..24).contains(&hh)
        || !(0..60).contains(&mm)
        || !(0..61).contains(&ss)
    {
        return None; // 61: leap seconds appear in the wild
    }
    let y_adj = if m <= 2 { y - 1 } else { y };
    let era = if y_adj >= 0 { y_adj } else { y_adj - 399 } / 400;
    let yoe = y_adj - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3_600 + mm * 60 + ss)
}

fn review_badge(review: &str, draft: bool) -> (&'static str, egui::Color32) {
    if draft {
        return ("draft", colors::TEXT_FAINT());
    }
    match review {
        "APPROVED" => ("✓ approved", colors::MOSS()),
        "CHANGES_REQUESTED" => ("± changes", colors::AMBER()),
        "REVIEW_REQUIRED" => ("• review", colors::TEXT_FAINT()),
        _ => ("", colors::TEXT_FAINT()),
    }
}

fn bucket_color(bucket: &str) -> egui::Color32 {
    match bucket {
        "pass" => colors::MOSS(),
        "fail" => colors::ERROR(),
        "pending" => colors::AMBER(),
        _ => colors::TEXT_FAINT(),
    }
}

// =================================================================================================
// background service + panel
// =================================================================================================

enum Msg {
    List(u64, Result<Vec<Pr>, String>),
    /// `gh pr diff N` split into per-file chunks with +/− counts. Carries the list seq it was
    /// fetched under — a refresh clears the caches, and a stale in-flight result must not
    /// repopulate them.
    Diff(u64, u64, Result<Vec<FileChunk>, String>),
    Checks(u64, u64, Vec<Check>),
    CheckoutDone(u64, Result<(), String>),
}

#[derive(Debug, Clone)]
pub struct FileChunk {
    pub rel: String,
    pub chunk: String,
    pub added: usize,
    pub removed: usize,
}

/// What the panel resolved this frame — the app acts on it.
pub enum PrAction {
    /// Open one PR file's diff in the viewer: (rel path, chunk text, "PR #N" label).
    OpenFileDiff(String, String, String),
    /// A checkout completed — repo state moved (refresh git/blame/history, reload buffers).
    CheckedOut,
}

pub struct PrPanel {
    prs: Vec<Pr>,
    selected: Option<usize>,
    /// None = fetch in flight; Some(files) = loaded (possibly empty = no textual changes).
    diffs: HashMap<u64, Option<Vec<FileChunk>>>,
    /// PRs whose diff fetch FAILED — parked (↻ clears) so a failure can't respawn gh per frame.
    diff_failed: std::collections::HashSet<u64>,
    checks: HashMap<u64, Vec<Check>>,
    error: Option<String>,
    loading: bool,
    checkout_busy: bool,
    root: Option<PathBuf>,
    seq: u64,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
}

impl Default for PrPanel {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            prs: Vec::new(),
            selected: None,
            diffs: HashMap::new(),
            diff_failed: std::collections::HashSet::new(),
            checks: HashMap::new(),
            error: None,
            loading: false,
            checkout_busy: false,
            root: None,
            seq: 0,
            tx,
            rx,
        }
    }
}

fn gh(root: &Path, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("gh")
        .current_dir(root)
        .args(args)
        .output()
        .map_err(|e| format!("gh not runnable: {e} — install the GitHub CLI"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        let tail: String = err.lines().take(2).collect::<Vec<_>>().join(" · ");
        Err(if tail.is_empty() { "gh failed".into() } else { tail })
    }
}

impl PrPanel {
    pub fn refresh(&mut self, root: &Path, ctx: &egui::Context) {
        self.seq += 1;
        let seq = self.seq;
        self.loading = true;
        self.error = None;
        self.root = Some(root.to_path_buf());
        let (tx, root, ctx) = (self.tx.clone(), root.to_path_buf(), ctx.clone());
        std::thread::Builder::new()
            .name("gh-pr-list".into())
            .spawn(move || {
                let res = gh(
                    &root,
                    &[
                        "pr", "list", "--limit", "50", "--json",
                        "number,title,author,headRefName,isDraft,updatedAt,reviewDecision",
                    ],
                )
                .map(|j| parse_pr_list(&j));
                let _ = tx.send(Msg::List(seq, res));
                ctx.request_repaint();
            })
            .ok();
    }

    fn fetch_details(&mut self, root: &Path, number: u64, ctx: &egui::Context) {
        if !self.diffs.contains_key(&number) && !self.diff_failed.contains(&number) {
            self.diffs.insert(number, None); // loading
            let seq = self.seq;
            let (tx, root, ctx) = (self.tx.clone(), root.to_path_buf(), ctx.clone());
            std::thread::Builder::new()
                .name("gh-pr-diff".into())
                .spawn(move || {
                    let n = number.to_string();
                    let res = gh(&root, &["pr", "diff", &n]).map(|text| {
                        crate::diffview::split_file_diffs(&text)
                            .into_iter()
                            .map(|(rel, chunk)| {
                                // Header +++/--- lines only exist BEFORE the first @@; inside a
                                // hunk, "+++i;" is a genuine added line and must count.
                                let (mut added, mut removed) = (0, 0);
                                let mut in_hunk = false;
                                for l in chunk.lines() {
                                    if l.starts_with("@@") {
                                        in_hunk = true;
                                        continue;
                                    }
                                    if !in_hunk {
                                        continue;
                                    }
                                    if l.starts_with('+') {
                                        added += 1;
                                    } else if l.starts_with('-') {
                                        removed += 1;
                                    }
                                }
                                FileChunk { rel, chunk, added, removed }
                            })
                            .collect()
                    });
                    let _ = tx.send(Msg::Diff(number, seq, res));
                    ctx.request_repaint();
                })
                .ok();
        }
        if !self.checks.contains_key(&number) {
            self.checks.insert(number, Vec::new());
            let seq = self.seq;
            let (tx, root, ctx) = (self.tx.clone(), root.to_path_buf(), ctx.clone());
            std::thread::Builder::new()
                .name("gh-pr-checks".into())
                .spawn(move || {
                    let n = number.to_string();
                    // Checks failing (no CI configured) is normal — empty list, not an error.
                    let checks = gh(&root, &["pr", "checks", &n, "--json", "name,bucket"])
                        .map(|j| parse_checks(&j))
                        .unwrap_or_default();
                    let _ = tx.send(Msg::Checks(number, seq, checks));
                    ctx.request_repaint();
                })
                .ok();
        }
    }

    fn pump(&mut self) -> Option<PrAction> {
        let mut action = None;
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::List(seq, res) => {
                    if seq != self.seq {
                        continue; // stale fetch from an older refresh
                    }
                    self.loading = false;
                    match res {
                        Ok(prs) => {
                            self.prs = prs;
                            self.selected = None;
                            self.diffs.clear();
                            self.diff_failed.clear();
                            self.checks.clear();
                        }
                        Err(e) => self.error = Some(e),
                    }
                }
                Msg::Diff(n, seq, res) => {
                    if seq != self.seq {
                        // Stale generation. Drop the `None` loading placeholder fetch_details
                        // left behind, or re-selecting this PR would skip the re-fetch
                        // (contains_key true) and wedge on "loading" forever — the case where
                        // the refresh's List errored so `diffs` was never cleared.
                        if matches!(self.diffs.get(&n), Some(None)) {
                            self.diffs.remove(&n);
                        }
                        continue;
                    }
                    match res {
                        Ok(files) => {
                            self.diffs.insert(n, Some(files));
                        }
                        Err(e) => {
                            // Park it: retrying every frame while selected would respawn gh
                            // endlessly. ↻ clears the parking.
                            self.diffs.remove(&n);
                            self.diff_failed.insert(n);
                            self.error = Some(e);
                        }
                    }
                }
                Msg::Checks(n, seq, checks) => {
                    if seq != self.seq {
                        // Same wedge as Diff: drop the empty-Vec placeholder so a re-select
                        // re-fetches instead of skipping on contains_key.
                        self.checks.remove(&n);
                        continue;
                    }
                    self.checks.insert(n, checks);
                }
                Msg::CheckoutDone(_, res) => {
                    self.checkout_busy = false;
                    match res {
                        Ok(()) => action = Some(PrAction::CheckedOut),
                        Err(e) => self.error = Some(e),
                    }
                }
            }
        }
        action
    }

    /// Draw the tab body; returns an action for the app to perform.
    pub fn ui(&mut self, ui: &mut egui::Ui, root: &Path) -> Option<PrAction> {
        if self.root.as_deref() != Some(root) {
            self.refresh(root, ui.ctx());
        }
        let mut action = self.pump();

        ui.horizontal(|ui| {
            ui.add_space(6.0);
            crate::style::panel_header_inline(ui, "Pull Requests");
            ui.colored_label(colors::TEXT_FAINT(), format!("{}", self.prs.len()));
            if ui.small_button("↻").clicked_by(egui::PointerButton::Primary) {
                self.refresh(root, ui.ctx());
            }
            if self.loading || self.checkout_busy {
                ui.spinner();
            }
            if let Some(e) = &self.error {
                ui.colored_label(colors::ERROR(), RichText::new(e).size(11.5));
            }
        });
        ui.separator();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let avail = ui.available_height();
        ui.horizontal_top(|ui| {
            // --- left: PR list ---------------------------------------------------------------
            let list_w = (ui.available_width() * 0.5).max(280.0);
            ui.allocate_ui(egui::vec2(list_w, avail), |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                egui::ScrollArea::vertical().id_salt("pr-list").auto_shrink([false, false]).show(
                    ui,
                    |ui| {
                        if self.prs.is_empty() && !self.loading {
                            ui.colored_label(colors::TEXT_FAINT(), "no open pull requests");
                        }
                        for (i, pr) in self.prs.iter().enumerate() {
                            let selected = self.selected == Some(i);
                            let (badge, badge_color) = review_badge(&pr.review, pr.is_draft);
                            ui.horizontal(|ui| {
                                let label = format!(
                                    "#{} {} — {}, {}",
                                    pr.number,
                                    pr.title,
                                    pr.author,
                                    crate::blame::relative_time(now, pr.updated),
                                );
                                let text = if selected {
                                    RichText::new(label).size(sizes::FONT_TREE).color(colors::ACCENT_HI())
                                } else {
                                    RichText::new(label).size(sizes::FONT_TREE).color(colors::TEXT_MUTED())
                                };
                                if ui
                                    .selectable_label(selected, text)
                                    .clicked_by(egui::PointerButton::Primary)
                                {
                                    // Detail fetch happens after the list borrow ends (below).
                                    self.selected = Some(i);
                                }
                                if !badge.is_empty() {
                                    ui.label(RichText::new(badge).size(10.5).color(badge_color));
                                }
                            });
                        }
                    },
                );
            });
            ui.separator();

            // --- right: selected PR ----------------------------------------------------------
            ui.vertical(|ui| {
                let Some(sel) = self.selected else {
                    ui.colored_label(colors::TEXT_FAINT(), "select a pull request");
                    return;
                };
                let Some(pr) = self.prs.get(sel).cloned() else { return };
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(format!("#{} {}", pr.number, pr.title)).strong());
                });
                ui.horizontal(|ui| {
                    ui.colored_label(
                        colors::TEXT_FAINT(),
                        format!("{} → {}", pr.head_ref, pr.author),
                    );
                    if ui
                        .add_enabled(!self.checkout_busy, egui::Button::new("Checkout").small())
                        .on_hover_text("gh pr checkout — switch the worktree to this branch")
                        .clicked_by(egui::PointerButton::Primary)
                    {
                        self.checkout_busy = true;
                        let (tx, root2, ctx) =
                            (self.tx.clone(), root.to_path_buf(), ui.ctx().clone());
                        let n = pr.number;
                        let spawned = std::thread::Builder::new()
                            .name("gh-pr-checkout".into())
                            .spawn(move || {
                                let res =
                                    gh(&root2, &["pr", "checkout", &n.to_string()]).map(|_| ());
                                let _ = tx.send(Msg::CheckoutDone(n, res));
                                ctx.request_repaint();
                            });
                        if spawned.is_err() {
                            self.checkout_busy = false; // never wedge the button
                        }
                    }
                    if ui
                        .small_button("↗")
                        .on_hover_text("open in browser")
                        .clicked_by(egui::PointerButton::Primary)
                    {
                        let root2 = root.to_path_buf();
                        let n = pr.number;
                        std::thread::Builder::new()
                            .name("gh-pr-web".into())
                            .spawn(move || {
                                let _ = gh(&root2, &["pr", "view", &n.to_string(), "-w"]);
                            })
                            .ok();
                    }
                });
                // CI checks.
                if let Some(checks) = self.checks.get(&pr.number) {
                    if !checks.is_empty() {
                        ui.add_space(2.0);
                        for c in checks {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new("●").size(10.0).color(bucket_color(&c.bucket)),
                                );
                                ui.colored_label(
                                    colors::TEXT_MUTED(),
                                    RichText::new(&c.name).size(11.5),
                                );
                            });
                        }
                    }
                }
                ui.add_space(4.0);
                // Changed files → per-file diff.
                match self.diffs.get(&pr.number) {
                    None if self.diff_failed.contains(&pr.number) => {
                        ui.colored_label(colors::ERROR(), "diff unavailable — ↻ to retry");
                    }
                    None | Some(None) => {
                        ui.colored_label(colors::TEXT_FAINT(), "loading diff…");
                    }
                    Some(Some(files)) if files.is_empty() => {
                        ui.colored_label(colors::TEXT_FAINT(), "no textual changes");
                    }
                    Some(Some(files)) => {
                        egui::ScrollArea::vertical()
                            .id_salt("pr-files")
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.spacing_mut().item_spacing.y = 0.0;
                                for f in files {
                                    ui.horizontal(|ui| {
                                        ui.add_space(2.0);
                                        ui.label(
                                            RichText::new(format!("+{}", f.added))
                                                .monospace()
                                                .size(10.5)
                                                .color(colors::MOSS()),
                                        );
                                        ui.label(
                                            RichText::new(format!("−{}", f.removed))
                                                .monospace()
                                                .size(10.5)
                                                .color(colors::ERROR()),
                                        );
                                        if ui
                                            .selectable_label(
                                                false,
                                                RichText::new(&f.rel)
                                                    .size(sizes::FONT_TREE)
                                                    .color(colors::TEXT_MUTED()),
                                            )
                                            .clicked_by(egui::PointerButton::Primary)
                                        {
                                            action = Some(PrAction::OpenFileDiff(
                                                f.rel.clone(),
                                                f.chunk.clone(),
                                                format!("PR #{}", pr.number),
                                            ));
                                        }
                                    });
                                }
                            });
                    }
                }
            });
        });
        // Selecting a row defers detail fetching to here (needs &mut self outside the iter).
        if let Some(sel) = self.selected {
            if let Some(n) = self.prs.get(sel).map(|p| p.number) {
                let root = root.to_path_buf();
                let ctx = ui.ctx().clone();
                self.fetch_details(&root, n, &ctx);
            }
        }
        action
    }
}

// =================================================================================================
// tests
// =================================================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pr_list_json() {
        let json = r#"[
          {"number": 42, "title": "Fix the thing", "author": {"login": "alice"},
           "headRefName": "fix/thing", "isDraft": false,
           "updatedAt": "2026-07-15T10:00:00Z", "reviewDecision": "APPROVED"},
          {"number": 43, "title": "WIP", "author": {"login": "bob"},
           "headRefName": "wip", "isDraft": true,
           "updatedAt": "2026-07-16T00:00:00Z", "reviewDecision": null}
        ]"#;
        let prs = parse_pr_list(json);
        assert_eq!(prs.len(), 2);
        assert_eq!(prs[0].number, 42);
        assert_eq!(prs[0].author, "alice");
        assert_eq!(prs[0].review, "APPROVED");
        assert!(prs[1].is_draft);
        assert!(prs[0].updated > 1_700_000_000);
        assert!(parse_pr_list("garbage").is_empty());
        assert!(parse_pr_list("{}").is_empty());
    }

    #[test]
    fn parses_checks_json() {
        let json = r#"[{"name": "build", "bucket": "pass"}, {"name": "lint", "bucket": "fail"}]"#;
        let checks = parse_checks(json);
        assert_eq!(checks.len(), 2);
        assert_eq!(checks[0].bucket, "pass");
        assert!(parse_checks("[]").is_empty());
    }

    #[test]
    fn iso8601_conversions() {
        assert_eq!(iso8601_to_unix("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(iso8601_to_unix("2000-01-01T00:00:00Z"), Some(946_684_800));
        assert_eq!(iso8601_to_unix("2026-07-16T12:34:56Z"), Some(1_784_205_296));
        assert_eq!(iso8601_to_unix("not a date"), None);
        assert_eq!(iso8601_to_unix("2026-13-01T00:00:00Z"), None);
        // Real calendar validation: impossible dates/times are rejected, leap years honored.
        assert_eq!(iso8601_to_unix("2026-02-31T00:00:00Z"), None);
        assert_eq!(iso8601_to_unix("2026-02-29T00:00:00Z"), None); // 2026 not a leap year
        assert!(iso8601_to_unix("2024-02-29T00:00:00Z").is_some()); // 2024 is
        assert_eq!(iso8601_to_unix("2026-07-16T24:00:00Z"), None);
        assert_eq!(iso8601_to_unix("2026-07-16T12:99:00Z"), None);
    }
}
