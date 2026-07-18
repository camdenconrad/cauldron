//! Side-by-side git diff viewer (JetBrains-style) — the central-panel takeover shown when a
//! changed file is clicked in the Git panel (or via the palette's "Show Diff").
//!
//! The pipeline is deliberately split so the interesting part is PURE and unit-tested:
//! `git diff --no-color -U3 [--cached|HEAD] -- <file>` text → [`parse_unified`] → hunks →
//! [`align_rows`] → aligned left/right row pairs (JetBrains block pairing: within a hunk, the run
//! of `-` lines pairs index-wise against the run of `+` lines, the shorter side padded). The UI
//! is one virtualized `ScrollArea::show_rows` over those pairs — both panes live in the same row
//! widget, so scroll sync is structural, not simulated.
//!
//! Per-hunk STAGE / UNSTAGE / REVERT works by patch reconstruction: the diff's file header
//! (preamble) plus ONE hunk, both kept verbatim from git's own output (including `\ No newline`
//! continuations and CRLF bytes), piped back through `git apply [--cached] [-R]`. Verbatim
//! round-tripping is what makes the patch apply cleanly to deletions, mode changes and renames
//! without us re-deriving headers.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use egui::{Color32, RichText};

use crate::style::colors;

// Tinted row backgrounds — the gutter palette (moss/red) at wash strength.
const BG_ADDED: Color32 = Color32::from_rgba_premultiplied(16, 24, 12, 44);
const BG_REMOVED: Color32 = Color32::from_rgba_premultiplied(30, 10, 8, 44);
const BG_PAD: Color32 = Color32::from_rgba_premultiplied(10, 10, 10, 18);
/// How long an armed "Really revert?" stays armed before disarming itself.
const REVERT_ARM_SECS: f64 = 3.0;

/// Which two trees the view compares — and therefore which hunk operations make sense.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffMode {
    /// Worktree vs index (`git diff`): hunks can be STAGED or REVERTED.
    Unstaged,
    /// Index vs HEAD (`git diff --cached`): hunks can be UNSTAGED.
    Staged,
    /// Worktree+index vs HEAD (`git diff HEAD`): read-only overview.
    Head,
}

impl DiffMode {
    fn label(self) -> &'static str {
        match self {
            DiffMode::Unstaged => "Unstaged",
            DiffMode::Staged => "Staged",
            DiffMode::Head => "vs HEAD",
        }
    }

    fn empty_text(self) -> &'static str {
        match self {
            DiffMode::Unstaged => "no unstaged changes",
            DiffMode::Staged => "nothing staged for this file",
            DiffMode::Head => "no changes against HEAD",
        }
    }
}

/// A hunk operation resolved from a header-row button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HunkOp {
    /// worktree → index (`git apply --cached`), Unstaged mode.
    Stage,
    /// index → back out (`git apply --cached -R`), Staged mode.
    Unstage,
    /// worktree → discard (`git apply -R`), Unstaged mode. Destructive; two-step confirmed.
    Revert,
}

// =================================================================================================
// pure model
// =================================================================================================

/// One side of an aligned diff row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    /// 1-based line number in that side's file.
    pub ln: usize,
    pub text: String,
    pub kind: CellKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellKind {
    Context,
    Removed,
    Added,
}

/// One visual row of the side-by-side view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// `@@ -a,b +c,d @@ …` separator; carries the index into [`DiffView`]'s hunks so the row can
    /// host that hunk's action buttons.
    HunkHeader { text: String, hunk: usize },
    /// Left (old) and/or right (new) cell; `None` = the padded empty half.
    Pair(Option<Cell>, Option<Cell>),
}

/// A parsed `@@` hunk: old/new start lines and the raw body lines (with their +/-/space prefix).
#[derive(Debug, Clone)]
pub struct Hunk {
    pub old_start: usize,
    pub new_start: usize,
    pub header: String,
    /// Body lines with prefixes AND `\ No newline` continuations kept VERBATIM — patch
    /// reconstruction needs byte-faithful lines; display filtering happens in [`align_rows`].
    pub lines: Vec<String>,
}

/// Everything before the first `@@` — `diff --git`, `index`, `---`/`+++`, mode/rename lines —
/// kept verbatim for patch reconstruction. Empty when there are no hunks.
pub fn split_preamble(text: &str) -> &str {
    match text.find("\n@@") {
        Some(i) => &text[..i + 1],
        None => "",
    }
}

/// Parse ONE file's `git diff --no-color` output into hunks. Everything before the first `@@` is
/// header chatter (diff --git, index, ---/+++, mode changes) and is skipped. Returns an empty
/// list for binary diffs ("Binary files … differ") and any other hunk-less output — the caller
/// renders a placeholder. Never panics on garbage.
pub fn parse_unified(text: &str) -> Vec<Hunk> {
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut in_hunk = false;
    for line in text.lines() {
        // A new file section ends the current hunk: without this, a concatenated second file's
        // `--- a/...` / `+++ b/...` headers (first byte '-'/'+') would be swallowed as body rows.
        if line.starts_with("diff --git ") {
            in_hunk = false;
            continue;
        }
        if let Some(rest) = line.strip_prefix("@@") {
            // "@@ -12,3 +14,4 @@ optional context"
            let (old_start, new_start) = parse_hunk_starts(rest).unwrap_or((0, 0));
            hunks.push(Hunk {
                old_start,
                new_start,
                header: line.to_string(),
                lines: Vec::new(),
            });
            in_hunk = true;
            continue;
        }
        if !in_hunk {
            continue;
        }
        let Some(h) = hunks.last_mut() else { continue };
        match line.as_bytes().first() {
            // Body lines AND "\ No newline at end of file" continuations are kept verbatim —
            // the latter must survive into reconstructed patches or `git apply` rejects them.
            Some(b'+') | Some(b'-') | Some(b' ') | Some(b'\\') => h.lines.push(line.to_string()),
            // An empty line inside a hunk is a context line whose content is empty ("" after the
            // space prefix is sometimes emitted bare by tools). Treat as empty context.
            None => h.lines.push(" ".to_string()),
            _ => {} // any other chatter ends up ignored, defensively
        }
    }
    hunks
}

/// `-12,3 +14,4 @@ …` → (12, 14). Counts (`,3`) are not needed: line numbers are re-derived by
/// walking the body. A count of 0 (empty-side hunk) still parses.
fn parse_hunk_starts(rest: &str) -> Option<(usize, usize)> {
    let mut old = None;
    let mut new = None;
    for tok in rest.split_whitespace() {
        if let Some(n) = tok.strip_prefix('-') {
            old = n.split(',').next()?.parse::<usize>().ok();
        } else if let Some(n) = tok.strip_prefix('+') {
            new = n.split(',').next()?.parse::<usize>().ok();
        }
        if old.is_some() && new.is_some() {
            break;
        }
    }
    Some((old?, new?))
}

/// Build the aligned side-by-side rows from parsed hunks (plus `(added, removed)` totals).
///
/// Within a hunk, runs of `-` and `+` lines are collected and paired block-wise: `-` line i sits
/// beside `+` line i, and the longer block's tail pads the other side with `None` — exactly how
/// JetBrains/GitHub split views read. Context lines flush any pending block first.
pub fn align_rows(hunks: &[Hunk]) -> (Vec<Row>, usize, usize) {
    let mut rows: Vec<Row> = Vec::new();
    let (mut added, mut removed) = (0usize, 0usize);
    for (hi, h) in hunks.iter().enumerate() {
        rows.push(Row::HunkHeader { text: h.header.clone(), hunk: hi });
        let mut old_ln = h.old_start;
        let mut new_ln = h.new_start;
        let mut dels: Vec<Cell> = Vec::new();
        let mut adds: Vec<Cell> = Vec::new();
        let flush = |rows: &mut Vec<Row>, dels: &mut Vec<Cell>, adds: &mut Vec<Cell>| {
            let n = dels.len().max(adds.len());
            let mut d = dels.drain(..);
            let mut a = adds.drain(..);
            for _ in 0..n {
                rows.push(Row::Pair(d.next(), a.next()));
            }
        };
        for line in &h.lines {
            let (prefix, body) = line.split_at(1);
            let body = body.strip_suffix('\r').unwrap_or(body); // CRLF worktrees
            match prefix.as_bytes()[0] {
                b'-' => {
                    removed += 1;
                    dels.push(Cell { ln: old_ln, text: body.to_string(), kind: CellKind::Removed });
                    old_ln += 1;
                }
                b'+' => {
                    added += 1;
                    adds.push(Cell { ln: new_ln, text: body.to_string(), kind: CellKind::Added });
                    new_ln += 1;
                }
                // "\ No newline at end of file": patch metadata, not a display row.
                b'\\' => {}
                _ => {
                    flush(&mut rows, &mut dels, &mut adds);
                    rows.push(Row::Pair(
                        Some(Cell { ln: old_ln, text: body.to_string(), kind: CellKind::Context }),
                        Some(Cell { ln: new_ln, text: body.to_string(), kind: CellKind::Context }),
                    ));
                    old_ln += 1;
                    new_ln += 1;
                }
            }
        }
        flush(&mut rows, &mut dels, &mut adds);
    }
    (rows, added, removed)
}

/// Rows for a file git has no diff for because it is UNTRACKED: everything is an addition.
pub fn all_added_rows(content: &str) -> (Vec<Row>, usize, usize) {
    let rows: Vec<Row> = content
        .lines()
        .enumerate()
        .map(|(i, l)| {
            Row::Pair(
                None,
                Some(Cell {
                    ln: i + 1,
                    text: l.strip_suffix('\r').unwrap_or(l).to_string(),
                    kind: CellKind::Added,
                }),
            )
        })
        .collect();
    let n = rows.len();
    (rows, n, 0)
}

/// Reconstruct an applicable single-hunk patch: the file's verbatim preamble + the hunk's header
/// and verbatim body, newline-terminated. Fed back through `git apply`.
pub fn patch_for_hunk(preamble: &str, h: &Hunk) -> String {
    let mut p = String::with_capacity(preamble.len() + h.header.len() + h.lines.len() * 40);
    p.push_str(preamble);
    p.push_str(&h.header);
    p.push('\n');
    for l in &h.lines {
        p.push_str(l);
        p.push('\n');
    }
    p
}

/// Split a MULTI-FILE unified diff (`git diff`, `gh pr diff`) into per-file chunks:
/// `(new-side rel path, full chunk text incl. its own headers)`. The name comes from the
/// `+++ b/<path>` line (falling back to the `diff --git` line's b-path; deletions report the
/// a-path). Pure + garbage-safe.
pub fn split_file_diffs(text: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut cur: Option<(Option<String>, String)> = None;
    let flush = |out: &mut Vec<(String, String)>, cur: Option<(Option<String>, String)>| {
        if let Some((name, chunk)) = cur {
            if let Some(n) = name {
                out.push((n, chunk));
            }
        }
    };
    let mut in_hunk = false;
    for line in text.lines() {
        if line.starts_with("diff --git ") {
            flush(&mut out, cur.take());
            in_hunk = false;
            // Provisional name from the b-side of the git header. Quoted (spaces/unicode) and
            // bare forms both handled; the +++ header below overrides it definitively.
            let name = diff_git_b_path(&line["diff --git ".len()..]);
            cur = Some((name, String::new()));
        }
        if let Some((name, chunk)) = cur.as_mut() {
            if line.starts_with("@@") {
                in_hunk = true;
            }
            // Header +++/--- lines only exist before the first @@ — an ADDED body line that
            // happens to start with "+++ " must not rename the chunk.
            if !in_hunk {
                if let Some(rest) = line.strip_prefix("+++ ") {
                    if let Some(p) = header_path(rest) {
                        *name = Some(p);
                    }
                } else if let Some(rest) = line.strip_prefix("--- ") {
                    if name.is_none() {
                        if let Some(p) = header_path(rest) {
                            *name = Some(p);
                        }
                    }
                }
            }
            chunk.push_str(line);
            chunk.push('\n');
        }
    }
    flush(&mut out, cur.take());
    out
}

/// A `+++ `/`--- ` header value → the clean path. Handles `b/path`, `"b/path with spaces"`
/// (git core.quotepath C-quoting), and `/dev/null` (→ None, an add/delete side).
fn header_path(rest: &str) -> Option<String> {
    let s = rest.trim();
    if s == "/dev/null" {
        return None;
    }
    let unq = if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        unquote_c(&s[1..s.len() - 1])
    } else {
        s.to_string()
    };
    let path = unq.strip_prefix("a/").or_else(|| unq.strip_prefix("b/")).unwrap_or(&unq);
    Some(path.to_string())
}

/// The b-side path from a `diff --git <a> <b>` remainder. Splits on the top-level space between
/// the two tokens, honoring quotes so `"a/x y" "b/x y"` isn't split mid-name.
fn diff_git_b_path(rest: &str) -> Option<String> {
    let rest = rest.trim();
    // Quoted b-side: the last `"b/…"` run.
    if let Some(open) = rest.rfind("\"b/") {
        let after = &rest[open + 1..];
        if let Some(close) = after.find('"') {
            return Some(unquote_c(&after[..close]).strip_prefix("b/").map(str::to_string).unwrap_or_default());
        }
    }
    // Bare b-side: the last ` b/` token.
    rest.rsplit(" b/").next().map(|s| s.trim().to_string())
}

/// Decode git's C-style quoting inside a `"…"` path: `\"`, `\\`, `\t`, `\n`, `\r`, and
/// `\NNN` octal (how quotepath renders non-ASCII bytes). Unknown escapes pass through.
fn unquote_c(s: &str) -> String {
    let mut bytes: Vec<u8> = Vec::with_capacity(s.len());
    let mut it = s.bytes().peekable();
    while let Some(b) = it.next() {
        if b != b'\\' {
            bytes.push(b);
            continue;
        }
        match it.next() {
            Some(b'"') => bytes.push(b'"'),
            Some(b'\\') => bytes.push(b'\\'),
            Some(b't') => bytes.push(b'\t'),
            Some(b'n') => bytes.push(b'\n'),
            Some(b'r') => bytes.push(b'\r'),
            Some(d @ b'0'..=b'7') => {
                // Up to three octal digits.
                let mut val = (d - b'0') as u32;
                for _ in 0..2 {
                    match it.peek() {
                        Some(&nd @ b'0'..=b'7') => {
                            val = val * 8 + (nd - b'0') as u32;
                            it.next();
                        }
                        _ => break,
                    }
                }
                bytes.push(val as u8);
            }
            Some(other) => {
                bytes.push(b'\\');
                bytes.push(other);
            }
            None => bytes.push(b'\\'),
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// A read-only DiffView over EXTERNALLY-SUPPLIED unified diff text (a PR file chunk): no hunk
/// buttons, no mode toggle — `label` shows in the header chip.
pub fn from_diff_text(root: &Path, rel: &str, text: &str, label: &str) -> DiffView {
    let mut v = DiffView::from_git_output(
        root.join(rel),
        rel.to_string(),
        DiffMode::Head,
        text,
        None,
    );
    if v.is_empty() {
        v.empty_reason = Some("no textual changes in this file".into());
    }
    v.commit_label = Some(label.to_string());
    v
}

fn git_diff_args(mode: DiffMode) -> Vec<&'static str> {
    // --no-ext-diff: a configured diff.external/GIT_EXTERNAL_DIFF would replace or empty the
    // unified output this parser depends on.
    match mode {
        DiffMode::Unstaged => vec!["diff", "--no-color", "--no-ext-diff", "-U3", "--"],
        DiffMode::Staged => vec!["diff", "--no-color", "--no-ext-diff", "-U3", "--cached", "--"],
        DiffMode::Head => vec!["diff", "--no-color", "--no-ext-diff", "-U3", "HEAD", "--"],
    }
}

/// Apply `view.hunks[idx]` per `op` by piping the reconstructed patch through `git apply`.
/// Returns git's stderr on failure.
pub fn apply_hunk(root: &Path, view: &DiffView, idx: usize, op: HunkOp) -> Result<(), String> {
    let Some(h) = view.hunks.get(idx) else { return Err("hunk vanished".into()) };
    let patch = patch_for_hunk(&view.preamble, h);
    let args: &[&str] = match op {
        HunkOp::Stage => &["apply", "--cached"],
        HunkOp::Unstage => &["apply", "--cached", "-R"],
        HunkOp::Revert => &["apply", "-R"],
    };
    let mut child = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(patch.as_bytes()).map_err(|e| e.to_string())?;
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// The diff ONE COMMIT made to ONE file (`git show <sha> -- <file>`, vs its parents; root
/// commits show as all-added). Read-only: no hunk buttons, no mode toggle — the header shows the
/// short sha instead.
pub fn open_commit(root: &Path, abs: &Path, sha: &str) -> Option<DiffView> {
    let rel = abs.strip_prefix(root).ok()?.to_path_buf();
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["show", "--no-color", "--no-ext-diff", "--format=", "-U3", sha, "--"])
        .arg(literal_pathspec(&rel))
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut v = DiffView::from_git_output(
        abs.to_path_buf(),
        rel.display().to_string(),
        DiffMode::Head,
        &text,
        None,
    );
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        v.empty_reason = Some(if err.is_empty() { "git show failed".into() } else { err });
    } else if v.is_empty() {
        v.empty_reason = Some("this commit did not change this file textually".into());
    }
    v.commit_label = Some(sha[..sha.len().min(8)].to_string());
    Some(v)
}

// =================================================================================================
// UI
// =================================================================================================

/// What the header/hunk buttons resolved to this frame. The view never spawns git itself — the
/// app applies and rebuilds.
pub enum DiffAction {
    Close,
    OpenInEditor(PathBuf),
    SwitchMode(DiffMode),
    Hunk(usize, HunkOp),
}

pub struct DiffView {
    /// Absolute path (Open in Editor target).
    pub path: PathBuf,
    /// Repo-relative display name.
    rel: String,
    mode: DiffMode,
    /// Verbatim file header for patch reconstruction (empty when hunk-less).
    preamble: String,
    /// Parsed hunks, verbatim bodies — indexes match [`Row::HunkHeader`]'s `hunk`.
    hunks: Vec<Hunk>,
    rows: Vec<Row>,
    added: usize,
    removed: usize,
    /// Widest cell text (chars) — drives the horizontal scroll extent.
    max_chars: usize,
    /// Digits of the largest line number — sizes the per-side number gutter.
    num_digits: usize,
    /// `path.is_file()` cached at construction (per-frame stats are wasteful).
    openable: bool,
    /// Whether hunks came from git (buttons make sense) vs the untracked all-added fallback.
    from_git: bool,
    /// True when the diff came back hunk-less (binary file / no changes in this mode).
    empty_reason: Option<String>,
    /// Two-step revert: (hunk index, armed-at time). Disarms after [`REVERT_ARM_SECS`].
    armed_revert: Option<(usize, f64)>,
    /// Historical view: the short sha whose commit this diff shows. Replaces the mode toggle
    /// (switching a historical diff to worktree modes mid-view would be disorienting).
    commit_label: Option<String>,
}

impl DiffView {
    /// Build from raw `git diff` output for one file. `untracked_content` supplies the file body
    /// ONLY when the caller verified the file is genuinely untracked (see [`open_mode`]) — a
    /// tracked file with hunk-less output (unchanged, mode-only change, binary) must show its
    /// placeholder, never a fabricated all-added view.
    pub fn from_git_output(
        path: PathBuf,
        rel: String,
        mode: DiffMode,
        diff_text: &str,
        untracked_content: Option<&str>,
    ) -> Self {
        let hunks = parse_unified(diff_text);
        let preamble = split_preamble(diff_text).to_string();
        let from_git = !hunks.is_empty();
        let (rows, added, removed) = if hunks.is_empty() {
            match untracked_content {
                Some(c) => all_added_rows(c),
                None => (Vec::new(), 0, 0),
            }
        } else {
            align_rows(&hunks)
        };
        let empty_reason = if rows.is_empty() {
            Some(if diff_text.contains("Binary files") || diff_text.contains("GIT binary patch") {
                "binary file — no textual diff".to_string()
            } else if diff_text.contains("\nold mode ") || diff_text.starts_with("old mode ") {
                "file mode change only — no textual diff".to_string()
            } else {
                mode.empty_text().to_string()
            })
        } else {
            None
        };
        let max_chars = rows
            .iter()
            .map(|r| match r {
                Row::HunkHeader { text, .. } => text.chars().count(),
                Row::Pair(l, rr) => l
                    .as_ref()
                    .map(|c| c.text.chars().count())
                    .unwrap_or(0)
                    .max(rr.as_ref().map(|c| c.text.chars().count()).unwrap_or(0)),
            })
            .max()
            .unwrap_or(0);
        let max_ln = rows
            .iter()
            .filter_map(|r| match r {
                Row::Pair(l, rr) => Some(
                    l.as_ref().map(|c| c.ln).unwrap_or(0).max(rr.as_ref().map(|c| c.ln).unwrap_or(0)),
                ),
                _ => None,
            })
            .max()
            .unwrap_or(1);
        let num_digits = max_ln.max(1).ilog10() as usize + 1;
        let openable = path.is_file();
        Self {
            path,
            rel,
            mode,
            preamble,
            hunks,
            rows,
            added,
            removed,
            max_chars,
            num_digits,
            openable,
            from_git,
            empty_reason,
            armed_revert: None,
            commit_label: None,
        }
    }

    pub fn mode(&self) -> DiffMode {
        self.mode
    }

    /// No rows at all (hunk-less diff without an untracked fallback) — the placeholder view.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Render the full takeover. `allow_esc` is false while a picker overlay is stacked above so
    /// one Escape doesn't rip through both layers.
    pub fn ui(&mut self, ui: &mut egui::Ui, allow_esc: bool) -> Option<DiffAction> {
        let mut action: Option<DiffAction> = None;
        let kb_free = ui.ctx().memory(|m| m.focused().is_none());
        if allow_esc && kb_free && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            return Some(DiffAction::Close);
        }
        ui.spacing_mut().item_spacing.y = 0.0;
        let now = ui.input(|i| i.time);
        if let Some((_, t)) = self.armed_revert {
            if now - t > REVERT_ARM_SECS {
                self.armed_revert = None;
            } else {
                // egui is reactive: without a scheduled frame the armed label would sit past its
                // expiry until the next mouse move.
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(300));
            }
        }

        // --- header bar -----------------------------------------------------------------------
        ui.horizontal(|ui| {
            ui.add_space(6.0);
            ui.label(RichText::new("Diff").color(colors::TEXT_FAINT()).size(12.0));
            ui.label(RichText::new(&self.rel).color(colors::TEXT()).monospace());
            ui.label(RichText::new(format!("+{}", self.added)).color(colors::MOSS()).monospace());
            ui.label(RichText::new(format!("−{}", self.removed)).color(colors::ERROR()).monospace());
            ui.add_space(8.0);
            match &self.commit_label {
                Some(short) => {
                    ui.label(
                        RichText::new(format!("@ {short}")).color(colors::AMBER()).monospace(),
                    );
                }
                None => {
                    for m in [DiffMode::Unstaged, DiffMode::Staged, DiffMode::Head] {
                        if ui
                            .selectable_label(self.mode == m, m.label())
                            .clicked_by(egui::PointerButton::Primary)
                            && self.mode != m
                        {
                            action = Some(DiffAction::SwitchMode(m));
                        }
                    }
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("✕").on_hover_text("Close (Esc)").clicked_by(egui::PointerButton::Primary) {
                    action = Some(DiffAction::Close);
                }
                if self.openable
                    && ui
                        .button("Open in Editor")
                        .clicked_by(egui::PointerButton::Primary)
                {
                    action = Some(DiffAction::OpenInEditor(self.path.clone()));
                }
            });
        });
        ui.separator();

        if let Some(reason) = &self.empty_reason {
            ui.centered_and_justified(|ui| {
                ui.colored_label(colors::TEXT_FAINT(), reason);
            });
            return action;
        }

        // --- body: one virtualized scroll over aligned row pairs --------------------------------
        let row_h = ui.text_style_height(&egui::TextStyle::Monospace) + 2.0;
        let num_w = (self.num_digits as f32 * 8.0 + 14.0).max(44.0);
        // Real horizontal extent: egui caps a ScrollArea's content ui at the viewport width, so
        // rows must be allocated at the CONTENT width for ::both() to ever overflow. Monospace →
        // one glyph measures all.
        let font = egui::TextStyle::Monospace.resolve(ui.style());
        let char_w = ui.fonts(|f| f.glyph_width(&font, 'm')).max(1.0);
        let need_half = num_w + 4.0 + self.max_chars as f32 * char_w + 8.0;
        let hunk_buttons =
            self.from_git && self.mode != DiffMode::Head && self.commit_label.is_none();
        egui::ScrollArea::both().auto_shrink([false, false]).show_rows(
            ui,
            row_h,
            self.rows.len(),
            |ui, range| {
                let full_w = ui.available_width().max(ui.min_rect().width());
                let half_w = ((full_w - 2.0) / 2.0).max(120.0).max(need_half);
                let row_w = half_w * 2.0 + 2.0;
                for i in range {
                    match &self.rows[i] {
                        Row::HunkHeader { text, hunk } => {
                            let hunk = *hunk;
                            let (rect, _) = ui.allocate_exact_size(
                                egui::vec2(row_w, row_h),
                                egui::Sense::hover(),
                            );
                            ui.painter().rect_filled(rect, 0.0, colors::BG_RAISED());
                            ui.painter().text(
                                egui::pos2(rect.left() + 8.0, rect.center().y),
                                egui::Align2::LEFT_CENTER,
                                text,
                                egui::TextStyle::Monospace.resolve(ui.style()),
                                colors::TEXT_FAINT(),
                            );
                            if hunk_buttons {
                                if let Some(act) = self.hunk_buttons_ui(ui, rect, hunk, now) {
                                    action = Some(act);
                                }
                            }
                        }
                        Row::Pair(l, r) => {
                            let (rect, _) = ui.allocate_exact_size(
                                egui::vec2(row_w, row_h),
                                egui::Sense::hover(),
                            );
                            let left_rect = egui::Rect::from_min_max(
                                rect.min,
                                egui::pos2(rect.left() + half_w, rect.bottom()),
                            );
                            let right_rect = egui::Rect::from_min_max(
                                egui::pos2(rect.left() + half_w + 2.0, rect.top()),
                                rect.max,
                            );
                            Self::paint_cell(ui, left_rect, l.as_ref(), num_w);
                            // center divider
                            ui.painter().line_segment(
                                [
                                    egui::pos2(rect.left() + half_w + 1.0, rect.top()),
                                    egui::pos2(rect.left() + half_w + 1.0, rect.bottom()),
                                ],
                                egui::Stroke::new(1.0, colors::BORDER()),
                            );
                            Self::paint_cell(ui, right_rect, r.as_ref(), num_w);
                        }
                    }
                }
            },
        );
        action
    }

    /// The right-aligned action buttons on one hunk-header row. Revert is a two-step confirm:
    /// first click arms ("Really revert?"), a second click within the window executes. Buttons
    /// pin to the viewport's right edge so they stay visible under horizontal scroll.
    fn hunk_buttons_ui(
        &mut self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        hunk: usize,
        now: f64,
    ) -> Option<DiffAction> {
        let mut action = None;
        let btn_h = rect.height() - 2.0;
        let mut right = ui.clip_rect().right().min(rect.right()) - 6.0;
        let mut place = |ui: &mut egui::Ui, label: &str, color: Color32| -> bool {
            let btn_w = label.len() as f32 * 7.0 + 16.0; // generous monospace-ish estimate
            let r = egui::Rect::from_min_max(
                egui::pos2(right - btn_w, rect.top() + 1.0),
                egui::pos2(right, rect.top() + 1.0 + btn_h),
            );
            right -= btn_w + 4.0;
            let b = egui::Button::new(RichText::new(label).size(11.0))
                .small()
                .fill(Color32::TRANSPARENT)
                .stroke(egui::Stroke::new(1.0, color));
            ui.put(r, b).clicked_by(egui::PointerButton::Primary)
        };
        match self.mode {
            DiffMode::Unstaged => {
                let armed = matches!(self.armed_revert, Some((h, _)) if h == hunk);
                let label = if armed { "Really revert?" } else { "Revert" };
                if place(ui, label, colors::ERROR()) {
                    if armed {
                        self.armed_revert = None;
                        action = Some(DiffAction::Hunk(hunk, HunkOp::Revert));
                    } else {
                        self.armed_revert = Some((hunk, now));
                    }
                }
                if place(ui, "Stage", colors::MOSS()) {
                    action = Some(DiffAction::Hunk(hunk, HunkOp::Stage));
                }
            }
            DiffMode::Staged => {
                if place(ui, "Unstage", colors::AMBER()) {
                    action = Some(DiffAction::Hunk(hunk, HunkOp::Unstage));
                }
            }
            DiffMode::Head => {}
        }
        action
    }

    /// One half-row: tinted background, right-aligned line number, then the text.
    fn paint_cell(ui: &egui::Ui, rect: egui::Rect, cell: Option<&Cell>, num_w: f32) {
        let painter = ui.painter();
        match cell {
            None => {
                painter.rect_filled(rect, 0.0, BG_PAD);
            }
            Some(c) => {
                let bg = match c.kind {
                    CellKind::Context => Color32::TRANSPARENT,
                    CellKind::Removed => BG_REMOVED,
                    CellKind::Added => BG_ADDED,
                };
                if bg != Color32::TRANSPARENT {
                    painter.rect_filled(rect, 0.0, bg);
                }
                let font = egui::TextStyle::Monospace.resolve(ui.style());
                let num_color = match c.kind {
                    CellKind::Context => colors::TEXT_FAINT().gamma_multiply(0.7),
                    CellKind::Removed => colors::ERROR().gamma_multiply(0.8),
                    CellKind::Added => colors::MOSS().gamma_multiply(0.8),
                };
                painter.text(
                    egui::pos2(rect.left() + num_w - 6.0, rect.center().y),
                    egui::Align2::RIGHT_CENTER,
                    c.ln,
                    font.clone(),
                    num_color,
                );
                let clip = painter.with_clip_rect(rect);
                clip.text(
                    egui::pos2(rect.left() + num_w + 4.0, rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    &c.text,
                    font,
                    colors::TEXT(),
                );
            }
        }
    }
}

/// `rel` as a LITERAL git pathspec. Raw names still glob after `--` (`log[1].txt` matches both
/// itself and `log1.txt`; a leading `:` triggers pathspec magic and can silently diff a
/// different file) — `:(literal)` turns all of that off.
pub(crate) fn literal_pathspec(rel: &Path) -> std::ffi::OsString {
    let mut spec = std::ffi::OsString::from(":(literal)");
    spec.push(rel.as_os_str());
    spec
}

/// Is `rel` tracked by git? Gates the untracked all-added fallback: a TRACKED file with
/// hunk-less diff output (unchanged, mode-only change, binary) must show its placeholder, not a
/// fabricated all-added view.
fn is_tracked(root: &Path, rel: &Path) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "--error-unmatch", "--"])
        .arg(literal_pathspec(rel))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run the mode's `git diff` for one file and build the viewer. Only genuinely UNTRACKED files
/// (checked via `git ls-files --error-unmatch`) fall back to the all-added worktree view.
pub fn open_mode(root: &Path, abs: &Path, mode: DiffMode) -> Option<DiffView> {
    let rel = abs.strip_prefix(root).ok()?.to_path_buf();
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(git_diff_args(mode))
        .arg(literal_pathspec(&rel))
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    // A failed git invocation (unborn HEAD in a fresh repo, corrupt index, …) must surface,
    // not masquerade as "no changes".
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let mut v = DiffView::from_git_output(
            abs.to_path_buf(),
            rel.display().to_string(),
            mode,
            "",
            None,
        );
        v.empty_reason = Some(if err.is_empty() { "git diff failed".into() } else { err });
        return Some(v);
    }
    let untracked = if mode != DiffMode::Staged
        && parse_unified(&text).is_empty()
        && !is_tracked(root, &rel)
    {
        std::fs::read_to_string(abs).ok()
    } else {
        None
    };
    Some(DiffView::from_git_output(
        abs.to_path_buf(),
        rel.display().to_string(),
        mode,
        &text,
        untracked.as_deref(),
    ))
}

// =================================================================================================
// tests — the pure parse/align pipeline
// =================================================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// split_file_diffs must name chunks correctly for bare, spaced-quoted, and unicode-octal
    /// paths — the old `" b/"` split + `"+++ b/"` prefix mangled quoted names.
    #[test]
    fn split_file_diffs_handles_quoted_paths() {
        let bare = "diff --git a/src/main.rs b/src/main.rs\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1 +1 @@\n-a\n+b\n";
        let d = split_file_diffs(bare);
        assert_eq!(d[0].0, "src/main.rs");

        let spaced = "diff --git \"a/my file.txt\" \"b/my file.txt\"\n--- \"a/my file.txt\"\n+++ \"b/my file.txt\"\n@@ -1 +1 @@\n-a\n+b\n";
        let d = split_file_diffs(spaced);
        assert_eq!(d[0].0, "my file.txt", "spaced quoted path unmangled");

        // Octal-escaped unicode (é = 0xC3 0xA9 → \303\251 under quotepath).
        let uni = "diff --git \"a/caf\\303\\251.txt\" \"b/caf\\303\\251.txt\"\n+++ \"b/caf\\303\\251.txt\"\n@@ -1 +1 @@\n+x\n";
        let d = split_file_diffs(uni);
        assert_eq!(d[0].0, "café.txt", "octal-escaped unicode decoded");
    }

    #[test]
    fn unquote_c_decodes_escapes() {
        assert_eq!(unquote_c("a\\tb"), "a\tb");
        assert_eq!(unquote_c("a\\\"b"), "a\"b");
        assert_eq!(unquote_c("caf\\303\\251"), "café");
        assert_eq!(unquote_c("plain"), "plain");
    }

    fn cell(ln: usize, text: &str, kind: CellKind) -> Option<Cell> {
        Some(Cell { ln, text: text.to_string(), kind })
    }

    fn hdr(text: &str, hunk: usize) -> Row {
        Row::HunkHeader { text: text.into(), hunk }
    }

    #[test]
    fn parse_and_align_a_modification_hunk() {
        let diff = "\
diff --git a/f.txt b/f.txt
index 000..111 100644
--- a/f.txt
+++ b/f.txt
@@ -1,3 +1,3 @@
 ctx
-old line
+new line
 tail
";
        let hunks = parse_unified(diff);
        assert_eq!(hunks.len(), 1);
        assert_eq!((hunks[0].old_start, hunks[0].new_start), (1, 1));
        let (rows, added, removed) = align_rows(&hunks);
        assert_eq!((added, removed), (1, 1));
        assert_eq!(
            rows,
            vec![
                hdr("@@ -1,3 +1,3 @@", 0),
                Row::Pair(cell(1, "ctx", CellKind::Context), cell(1, "ctx", CellKind::Context)),
                Row::Pair(
                    cell(2, "old line", CellKind::Removed),
                    cell(2, "new line", CellKind::Added),
                ),
                Row::Pair(cell(3, "tail", CellKind::Context), cell(3, "tail", CellKind::Context)),
            ]
        );
    }

    #[test]
    fn unbalanced_blocks_pad_the_short_side() {
        let diff = "\
@@ -1,2 +1,4 @@
-only removal
+add one
+add two
+add three
 ctx
";
        let (rows, added, removed) = align_rows(&parse_unified(diff));
        assert_eq!((added, removed), (3, 1));
        assert_eq!(
            &rows[1..4],
            &[
                Row::Pair(
                    cell(1, "only removal", CellKind::Removed),
                    cell(1, "add one", CellKind::Added),
                ),
                Row::Pair(None, cell(2, "add two", CellKind::Added)),
                Row::Pair(None, cell(3, "add three", CellKind::Added)),
            ]
        );
        // Context line numbers continue correctly after the unbalanced block.
        assert_eq!(
            rows[4],
            Row::Pair(cell(2, "ctx", CellKind::Context), cell(4, "ctx", CellKind::Context)),
        );
    }

    #[test]
    fn multiple_hunks_get_headers_and_correct_line_numbers() {
        let diff = "\
@@ -10,2 +10,2 @@ fn a()
 x
-b
+B
@@ -100,2 +100,2 @@ fn z()
 y
-c
+C
";
        let (rows, ..) = align_rows(&parse_unified(diff));
        assert!(matches!(&rows[0], Row::HunkHeader { text, hunk: 0 } if text.contains("fn a()")));
        assert!(matches!(&rows[3], Row::HunkHeader { text, hunk: 1 } if text.contains("fn z()")));
        assert_eq!(
            rows[4],
            Row::Pair(cell(100, "y", CellKind::Context), cell(100, "y", CellKind::Context)),
        );
    }

    #[test]
    fn no_newline_marker_and_crlf_are_absorbed() {
        let diff = "\
@@ -1 +1 @@
-old\r
+new
\\ No newline at end of file
";
        let hunks = parse_unified(diff);
        // Verbatim body keeps the marker (patch fidelity)…
        assert!(hunks[0].lines.iter().any(|l| l.starts_with('\\')));
        // …but display rows absorb it, and the \r is stripped.
        let (rows, added, removed) = align_rows(&hunks);
        assert_eq!((added, removed), (1, 1));
        assert_eq!(
            rows[1],
            Row::Pair(cell(1, "old", CellKind::Removed), cell(1, "new", CellKind::Added)),
        );
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn binary_and_garbage_yield_no_hunks() {
        assert!(parse_unified("Binary files a/x.png and b/x.png differ\n").is_empty());
        assert!(parse_unified("").is_empty());
        assert!(parse_unified("complete\nnonsense\n@@ broken @@\n+x\n").len() == 1); // header salvaged
        // Even a broken @@ header defaults starts to 0 without panicking.
        let (rows, ..) = align_rows(&parse_unified("@@ broken @@\n+x\n"));
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn untracked_file_shows_all_added() {
        let (rows, added, removed) = all_added_rows("a\nb\r\nc");
        assert_eq!((added, removed), (3, 0));
        assert_eq!(rows[1], Row::Pair(None, cell(2, "b", CellKind::Added)));
    }

    /// End-to-end against REAL git output: modify, delete, and untracked files all produce the
    /// right row shapes through [`open`]. Skips silently when git isn't available.
    #[test]
    fn open_against_a_real_repo() {
        let dir = std::env::temp_dir()
            .join(format!("cauldron-diffview-{}", std::process::id()));
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
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        if !git(&["init", "-q"]) {
            return; // no git on this box — skip
        }
        let _ = git(&["config", "user.email", "t@t"]);
        let _ = git(&["config", "user.name", "t"]);
        std::fs::write(dir.join("mod.txt"), "one\ntwo\nthree\n").unwrap();
        std::fs::write(dir.join("gone.txt"), "bye\n").unwrap();
        let _ = git(&["add", "."]);
        let _ = git(&["commit", "-qm", "init"]);

        // Modified file: one changed line.
        std::fs::write(dir.join("mod.txt"), "one\nTWO\nthree\n").unwrap();
        let v = open_mode(&dir, &dir.join("mod.txt"), DiffMode::Head).unwrap();
        assert_eq!((v.added, v.removed), (1, 1), "modify: one line each way");
        assert!(v.empty_reason.is_none());
        assert!(v.rows.iter().any(|r| matches!(
            r, Row::Pair(Some(l), Some(rr))
               if l.kind == CellKind::Removed && l.text == "two"
               && rr.kind == CellKind::Added && rr.text == "TWO")));

        // Deleted file: all-removed hunks parse from git (worktree file gone).
        std::fs::remove_file(dir.join("gone.txt")).unwrap();
        let v = open_mode(&dir, &dir.join("gone.txt"), DiffMode::Head).unwrap();
        assert_eq!((v.added, v.removed), (0, 1), "delete: one removed line");

        // Untracked file: all-added fallback.
        std::fs::write(dir.join("new.txt"), "a\nb\n").unwrap();
        let v = open_mode(&dir, &dir.join("new.txt"), DiffMode::Head).unwrap();
        assert_eq!((v.added, v.removed), (2, 0), "untracked: everything added");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn patch_reconstruction_is_verbatim() {
        let diff = "\
diff --git a/f.txt b/f.txt
index 000..111 100644
--- a/f.txt
+++ b/f.txt
@@ -1,2 +1,2 @@ fn ctx()
 keep
-old
+new
\\ No newline at end of file
";
        let pre = split_preamble(diff);
        assert!(pre.starts_with("diff --git"));
        assert!(pre.ends_with("+++ b/f.txt\n"));
        let hunks = parse_unified(diff);
        let patch = patch_for_hunk(pre, &hunks[0]);
        // Round trip: preamble + header + verbatim body (incl. the no-newline marker).
        assert_eq!(patch, diff);
    }

    /// Two concatenated file diffs (git can emit these for glob-y pathspecs): the second file's
    /// ---/+++ headers must not bleed into the first file's hunk as bogus rows.
    #[test]
    fn concatenated_file_diffs_do_not_bleed() {
        let diff = "\
diff --git a/a.txt b/a.txt
--- a/a.txt
+++ b/a.txt
@@ -1 +1 @@
-x
+X
diff --git a/b.txt b/b.txt
--- a/b.txt
+++ b/b.txt
@@ -1 +1 @@
-y
+Y
";
        let hunks = parse_unified(diff);
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].lines, vec!["-x", "+X"], "no ---/+++ bleed into hunk 0");
        let (_, added, removed) = align_rows(&hunks);
        assert_eq!((added, removed), (2, 2));
    }

    /// End-to-end against REAL git: two separate edits become two hunks; staging one via
    /// [`apply_hunk`] moves exactly that hunk to the index; reverting the other restores the
    /// worktree. Skips silently when git isn't available.
    #[test]
    fn stage_and_revert_hunks_against_a_real_repo() {
        let dir = std::env::temp_dir().join(format!("cauldron-hunks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let git = |args: &[&str]| -> Option<String> {
            let o = std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .output()
                .ok()?;
            o.status.success().then(|| String::from_utf8_lossy(&o.stdout).into_owned())
        };
        if git(&["init", "-q"]).is_none() {
            return; // no git on this box — skip
        }
        git(&["config", "user.email", "t@t"]).unwrap();
        git(&["config", "user.name", "t"]).unwrap();
        // 20 lines so two edits are far enough apart for -U3 to make two hunks.
        let base: String = (0..20).map(|i| format!("line {i}\n")).collect();
        std::fs::write(dir.join("f.txt"), &base).unwrap();
        git(&["add", "."]).unwrap();
        git(&["commit", "-qm", "init"]).unwrap();

        // Edit line 2 and line 17 — two hunks.
        let edited = base.replace("line 2\n", "LINE 2\n").replace("line 17\n", "LINE 17\n");
        std::fs::write(dir.join("f.txt"), &edited).unwrap();

        let v = open_mode(&dir, &dir.join("f.txt"), DiffMode::Unstaged).unwrap();
        assert_eq!(v.hunks.len(), 2, "two separate edits → two hunks");
        assert!(v.from_git);

        // Stage hunk 0 (the LINE 2 edit): the index gains it, the worktree diff loses it.
        apply_hunk(&dir, &v, 0, HunkOp::Stage).unwrap();
        let staged = git(&["diff", "--cached", "--", "f.txt"]).unwrap();
        assert!(staged.contains("+LINE 2") && !staged.contains("+LINE 17"));
        let v2 = open_mode(&dir, &dir.join("f.txt"), DiffMode::Unstaged).unwrap();
        assert_eq!(v2.hunks.len(), 1, "one unstaged hunk remains");

        // Revert the remaining hunk (LINE 17): the worktree goes back to "line 17".
        apply_hunk(&dir, &v2, 0, HunkOp::Revert).unwrap();
        let content = std::fs::read_to_string(dir.join("f.txt")).unwrap();
        assert!(content.contains("line 17") && !content.contains("LINE 17"));
        assert!(content.contains("LINE 2"), "staged edit survives in the worktree");

        // Staged mode sees the staged hunk; unstaging it empties the index diff.
        let vs = open_mode(&dir, &dir.join("f.txt"), DiffMode::Staged).unwrap();
        assert_eq!(vs.hunks.len(), 1);
        apply_hunk(&dir, &vs, 0, HunkOp::Unstage).unwrap();
        let staged = git(&["diff", "--cached", "--", "f.txt"]).unwrap();
        assert!(staged.trim().is_empty(), "index clean after unstage");

        // An UNCHANGED tracked file must show a placeholder, never the all-added fallback
        // (the review-confirmed tracked/untracked gate).
        git(&["add", "."]).unwrap();
        git(&["commit", "-qm", "settle"]).unwrap();
        let clean = open_mode(&dir, &dir.join("f.txt"), DiffMode::Head).unwrap();
        assert!(clean.is_empty(), "unchanged tracked file: placeholder, not all-added");

        // Glob-metachar filename diffs exactly itself (:(literal) pathspec).
        std::fs::write(dir.join("log1.txt"), "a\n").unwrap();
        std::fs::write(dir.join("log[1].txt"), "b\n").unwrap();
        git(&["add", "."]).unwrap();
        git(&["commit", "-qm", "globs"]).unwrap();
        std::fs::write(dir.join("log1.txt"), "a2\n").unwrap();
        std::fs::write(dir.join("log[1].txt"), "b2\n").unwrap();
        let g = open_mode(&dir, &dir.join("log[1].txt"), DiffMode::Unstaged).unwrap();
        assert_eq!(g.hunks.len(), 1, "bracket filename matches only itself");
        assert_eq!((g.added, g.removed), (1, 1));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn splits_multi_file_diffs_with_renames_and_deletions() {
        let text = "\
diff --git a/src/a.rs b/src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -1 +1 @@
-x
+X
diff --git a/old.txt b/new.txt
similarity index 90%
rename from old.txt
rename to new.txt
--- a/old.txt
+++ b/new.txt
@@ -1 +1 @@
-a
+b
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
--- a/gone.txt
+++ /dev/null
@@ -1 +0,0 @@
-bye
";
        let files = split_file_diffs(text);
        let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["src/a.rs", "new.txt", "gone.txt"]);
        assert!(files[0].1.contains("+X"));
        assert!(files[2].1.contains("-bye"));
        // Each chunk parses standalone through the normal pipeline.
        let hunks = parse_unified(&files[1].1);
        assert_eq!(hunks.len(), 1);
        assert!(split_file_diffs("").is_empty());
    }

    #[test]
    fn deletion_only_file_aligns_left() {
        let diff = "\
@@ -1,2 +0,0 @@
-gone one
-gone two
";
        let (rows, added, removed) = align_rows(&parse_unified(diff));
        assert_eq!((added, removed), (0, 2));
        assert_eq!(rows[1], Row::Pair(cell(1, "gone one", CellKind::Removed), None));
        assert_eq!(rows[2], Row::Pair(cell(2, "gone two", CellKind::Removed), None));
    }
}
