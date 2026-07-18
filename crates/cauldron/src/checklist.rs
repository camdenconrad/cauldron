//! cFE CONTRIBUTION CHECKLIST — "PR readiness" panel. Runs the real cFE CI gates locally
//! against the current branch's changes (vs the merge-base with origin/main / main / master):
//! clang-format drift, commit-message conventions, doxygen on new functions, unit-test
//! coupling, trailing whitespace/tabs, CONTRIBUTING.md pointer.
//!
//! Worker shape: the cider PTY template — ONE std::thread computing everything, results in a
//! shared Arc<Mutex<..>>, egui::Context::request_repaint() when done. No async, no tokio.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::style::{self, colors};

// =================================================================================================
// data model
// =================================================================================================

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Pass,
    Warn,
    Fail,
    Skip,
}

impl Status {
    fn glyph(self) -> &'static str {
        match self {
            Status::Pass => "✓",
            Status::Warn => "⚠",
            Status::Fail => "✗",
            Status::Skip => "–",
        }
    }
    fn color(self) -> egui::Color32 {
        match self {
            Status::Pass => colors::MOSS(),
            Status::Warn => colors::WARN(),
            Status::Fail => colors::ERROR(),
            Status::Skip => colors::TEXT_FAINT(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct CheckResult {
    pub name: String,
    pub status: Status,
    pub detail: String,
    /// Findings that point at a concrete file+line (1-based).
    pub locations: Vec<(PathBuf, usize)>,
}

// =================================================================================================
// the panel
// =================================================================================================

pub struct Checklist {
    results: Arc<Mutex<Vec<CheckResult>>>,
    running: Arc<AtomicBool>,
    /// Ran at least once (so the empty state can say "run it" vs "all clear").
    started: bool,
}

impl Default for Checklist {
    fn default() -> Self {
        Self {
            results: Arc::new(Mutex::new(Vec::new())),
            running: Arc::new(AtomicBool::new(false)),
            started: false,
        }
    }
}

impl Checklist {
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// Kick off ONE background thread that computes every check against `root` (a git repo).
    /// A run already in flight is left alone.
    pub fn run(&mut self, root: PathBuf, ctx: egui::Context) {
        if self.running.swap(true, Ordering::SeqCst) {
            return;
        }
        self.started = true;
        let results = Arc::clone(&self.results);
        let running = Arc::clone(&self.running);
        std::thread::spawn(move || {
            let computed = run_checks(&root);
            *results.lock().unwrap_or_else(|e| e.into_inner()) = computed;
            running.store(false, Ordering::SeqCst);
            ctx.request_repaint();
        });
    }

    /// Render the results. Returns `Some((file, line))` when the user clicks a finding that
    /// points at a location — the caller opens it in the editor.
    pub fn ui(&mut self, ui: &mut egui::Ui) -> Option<(PathBuf, usize)> {
        let mut open: Option<(PathBuf, usize)> = None;
        ui.horizontal(|ui| {
            style::panel_header_inline(ui, "PR readiness");
            if self.is_running() {
                ui.spinner();
            }
        });
        style::hairline(ui);

        let results = self.results.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if results.is_empty() {
            let msg = if self.is_running() {
                "running checks…"
            } else if self.started {
                "no results"
            } else {
                "not run yet"
            };
            ui.colored_label(colors::TEXT_FAINT(), msg);
            return None;
        }

        egui::ScrollArea::vertical()
            .id_salt("checklist_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (i, r) in results.iter().enumerate() {
                    let header = format!("{}  {}", r.status.glyph(), r.name);
                    egui::CollapsingHeader::new(
                        egui::RichText::new(header).color(r.status.color()),
                    )
                    .id_salt(("checklist_item", i))
                    .default_open(r.status == Status::Fail)
                    .show(ui, |ui| {
                        if !r.detail.is_empty() {
                            ui.colored_label(colors::TEXT_MUTED(), &r.detail);
                        }
                        for (path, line) in &r.locations {
                            let label = format!("{}:{}", path.display(), line);
                            let resp = ui.add(
                                egui::Label::new(
                                    egui::RichText::new(label).color(colors::ACCENT_HI()),
                                )
                                .sense(egui::Sense::click()),
                            );
                            if resp.clicked_by(egui::PointerButton::Primary) {
                                open = Some((path.clone(), *line));
                            }
                        }
                    });
                }
            });
        open
    }
}

// =================================================================================================
// the checks — pure functions over `git -C root …` + file reads. All headless-testable.
// =================================================================================================

/// Run `git -C root args…`; Some(stdout) on exit 0.
fn git(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(root).args(args).output().ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        None
    }
}

pub fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The diff base: merge-base of HEAD with origin/main → main → master, else HEAD~1.
fn find_base(root: &Path) -> Option<String> {
    let head = git(root, &["rev-parse", "HEAD"]).map(|s| s.trim().to_string());
    let dirty = git(root, &["status", "--porcelain"])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    for r in ["origin/main", "main", "master"] {
        if let Some(mb) = git(root, &["merge-base", "HEAD", r]) {
            let mb = mb.trim().to_string();
            // Skip refs where the merge-base IS HEAD and the tree is clean — we'd diff
            // nothing (e.g. sitting on main itself); fall through to HEAD~1 instead.
            // With worktree changes the base is still useful (diff shows them).
            if head.as_deref() != Some(mb.as_str()) || dirty {
                return Some(mb);
            }
        }
    }
    git(root, &["rev-parse", "HEAD~1"]).map(|s| s.trim().to_string())
}

/// One added line from the diff: (file, new-line-number, text).
type AddedLine = (PathBuf, usize, String);

/// Parse `git diff` unified output into per-file added lines.
fn parse_added_lines(diff: &str) -> Vec<AddedLine> {
    let mut out = Vec::new();
    let mut file: Option<PathBuf> = None;
    let mut new_line = 0usize;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            file = rest
                .strip_prefix("b/")
                .filter(|p| *p != "/dev/null")
                .map(PathBuf::from);
        } else if let Some(rest) = line.strip_prefix("@@ ") {
            // @@ -a,b +c,d @@
            if let Some(plus) = rest.split_whitespace().find(|t| t.starts_with('+')) {
                let num = plus[1..].split(',').next().unwrap_or("0");
                new_line = num.parse().unwrap_or(0);
            }
        } else if let Some(text) = line.strip_prefix('+') {
            if !line.starts_with("+++") {
                if let Some(f) = &file {
                    out.push((f.clone(), new_line, text.to_string()));
                }
                new_line += 1;
            }
        } else if !line.starts_with('-') && !line.starts_with('\\') {
            new_line += 1;
        }
    }
    out
}

fn changed_files(root: &Path, base: &str) -> Vec<PathBuf> {
    git(root, &["diff", "--name-only", base])
        .map(|s| s.lines().map(PathBuf::from).collect())
        .unwrap_or_default()
}

fn is_c_or_h(p: &Path) -> bool {
    matches!(p.extension().and_then(|e| e.to_str()), Some("c") | Some("h"))
}

/// Nearest `.clang-format` walking up from `file`'s directory to `root` (inclusive).
fn nearest_clang_format(root: &Path, file: &Path) -> Option<PathBuf> {
    let mut dir = root.join(file);
    dir.pop();
    loop {
        let cand = dir.join(".clang-format");
        if cand.is_file() {
            return Some(cand);
        }
        if dir == *root || !dir.pop() || !dir.starts_with(root) {
            return None;
        }
    }
}

/// Heuristic: an added line at column 0 that looks like a C function DEFINITION opener.
fn looks_like_c_fn_def(line: &str) -> bool {
    let Some(first) = line.chars().next() else { return false };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    let t = line.trim_end();
    if t.ends_with(';') || t.ends_with(',') || !t.contains('(') {
        return false;
    }
    let head = t.split('(').next().unwrap_or("");
    let first_word = head.split_whitespace().next().unwrap_or("");
    const NOT_FN: &[&str] = &[
        "if", "for", "while", "switch", "return", "else", "do", "case", "typedef", "struct",
        "enum", "union", "#define", "sizeof",
    ];
    if NOT_FN.contains(&first_word) {
        return false;
    }
    // Definition (not a call/prototype): needs a return-type word before the name, i.e. the
    // part before '(' has >= 2 identifier-ish tokens (e.g. "int32 CFE_ES_DoThing").
    let tokens: Vec<&str> = head.split_whitespace().collect();
    if tokens.len() < 2 {
        return false;
    }
    // Ends with ')' or '{' (or ") {"): opener line of a definition.
    t.ends_with('{') || t.ends_with(')')
}

// ---- check 1: clang-format drift ---------------------------------------------------------------
fn check_format(root: &Path, base: &str) -> CheckResult {
    let name = "format-check (clang-format)".to_string();
    let files: Vec<PathBuf> = changed_files(root, base).into_iter().filter(|p| is_c_or_h(p)).collect();
    if files.is_empty() {
        return CheckResult {
            name,
            status: Status::Skip,
            detail: "no changed .c/.h files".into(),
            locations: vec![],
        };
    }
    let clang_ok = Command::new("clang-format")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !clang_ok {
        return CheckResult {
            name,
            status: Status::Skip,
            detail: "clang-format not installed".into(),
            locations: vec![],
        };
    }
    let mut locations = Vec::new();
    let mut drifted = Vec::new();
    let mut checked = 0usize;
    for f in &files {
        let abs = root.join(f);
        if nearest_clang_format(root, f).is_none() || !abs.is_file() {
            continue;
        }
        checked += 1;
        let Ok(out) = Command::new("clang-format").arg("--style=file").arg(&abs).output() else {
            continue;
        };
        if !out.status.success() {
            continue;
        }
        let formatted = String::from_utf8_lossy(&out.stdout);
        let Ok(orig) = std::fs::read_to_string(&abs) else { continue };
        if orig != formatted {
            let drift_line = orig
                .lines()
                .zip(formatted.lines())
                .position(|(a, b)| a != b)
                .map(|i| i + 1)
                .unwrap_or_else(|| orig.lines().count().min(formatted.lines().count()) + 1);
            drifted.push(format!("{} (first drift line {})", f.display(), drift_line));
            locations.push((f.clone(), drift_line));
        }
    }
    if checked == 0 {
        CheckResult {
            name,
            status: Status::Skip,
            detail: "no .clang-format found for changed files".into(),
            locations: vec![],
        }
    } else if drifted.is_empty() {
        CheckResult { name, status: Status::Pass, detail: format!("{checked} file(s) clean"), locations: vec![] }
    } else {
        CheckResult {
            name,
            status: Status::Fail,
            detail: format!("formatting drift:\n{}", drifted.join("\n")),
            locations,
        }
    }
}

// ---- check 2: commit-message conventions --------------------------------------------------------
fn check_commit_messages(root: &Path, base: &str) -> CheckResult {
    let name = "commit messages".to_string();
    let log = git(root, &["log", "--format=%s%x01%b%x02", &format!("{base}..HEAD")]).unwrap_or_default();
    let commits: Vec<&str> = log.split('\u{2}').map(str::trim).filter(|c| !c.is_empty()).collect();
    if commits.is_empty() {
        return CheckResult {
            name,
            status: Status::Skip,
            detail: "no commits unique to this branch".into(),
            locations: vec![],
        };
    }
    let mut status = Status::Pass;
    let mut notes = Vec::new();
    let mut any_issue_ref = false;
    for c in &commits {
        let mut parts = c.splitn(2, '\u{1}');
        let subject = parts.next().unwrap_or("").trim();
        let body = parts.next().unwrap_or("");
        if subject.len() > 72 {
            status = Status::Fail;
            notes.push(format!("subject > 72 chars: \"{}…\"", truncate_chars(subject, 40)));
        }
        // Imperative-ish (warn only): first word shouldn't end in -ed / -ing.
        let first = subject.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
        if first.ends_with("ed") || first.ends_with("ing") {
            if status == Status::Pass {
                status = Status::Warn;
            }
            notes.push(format!("subject may not be imperative: \"{subject}\""));
        }
        if references_issue(subject) || references_issue(body) {
            any_issue_ref = true;
        }
    }
    if !any_issue_ref {
        if status == Status::Pass {
            status = Status::Warn;
        }
        notes.push("no commit references an issue (\"Fix #n\" / \"Fixes #n\") — cFS PRs must reference an issue".into());
    }
    let detail = if notes.is_empty() {
        format!("{} commit(s) look good", commits.len())
    } else {
        notes.join("\n")
    };
    CheckResult { name, status, detail, locations: vec![] }
}

/// Char-safe prefix (byte-slicing user text can split a multibyte char and panic).
fn truncate_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// "Fix #12", "Fixes #7", "Closes #3", "Resolves #9" style references.
fn references_issue(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    for kw in ["fix", "fixes", "fixed", "close", "closes", "closed", "resolve", "resolves", "resolved"] {
        let mut rest = lower.as_str();
        while let Some(pos) = rest.find(kw) {
            let after = &rest[pos + kw.len()..];
            let after = after.trim_start();
            if let Some(num) = after.strip_prefix('#') {
                if num.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                    return true;
                }
            }
            rest = &rest[pos + kw.len()..];
        }
    }
    false
}

// ---- check 3: doxygen on ADDED function definitions ---------------------------------------------
fn check_doxygen(root: &Path, added: &[AddedLine]) -> CheckResult {
    let name = "doxygen on new functions".to_string();
    let mut undocumented = Vec::new();
    let mut locations = Vec::new();
    let mut seen_any = false;
    for (file, line_no, text) in added {
        if file.extension().and_then(|e| e.to_str()) != Some("c") || !looks_like_c_fn_def(text) {
            continue;
        }
        seen_any = true;
        let abs = root.join(file);
        let Ok(content) = std::fs::read_to_string(&abs) else { continue };
        let lines: Vec<&str> = content.lines().collect();
        let idx = line_no.saturating_sub(1);
        let lo = idx.saturating_sub(10);
        // Scan upward; a `}` ends the previous function — doc blocks above it don't count.
        let mut documented = false;
        for l in lines[lo..idx.min(lines.len())].iter().rev() {
            if l.contains("/**") || l.contains("/*\\*") || l.contains("/*!") {
                documented = true;
                break;
            }
            if l.contains('}') {
                break;
            }
        }
        if !documented {
            let fname = text.split('(').next().unwrap_or("").split_whitespace().last().unwrap_or("?");
            undocumented.push(format!("{}:{} {}", file.display(), line_no, fname));
            locations.push((file.clone(), *line_no));
        }
    }
    if !seen_any {
        CheckResult { name, status: Status::Skip, detail: "no new C function definitions".into(), locations: vec![] }
    } else if undocumented.is_empty() {
        CheckResult { name, status: Status::Pass, detail: "all new functions documented".into(), locations: vec![] }
    } else {
        CheckResult {
            name,
            status: Status::Fail,
            detail: format!("undocumented new functions:\n{}", undocumented.join("\n")),
            locations,
        }
    }
}

// ---- check 4: unit tests touched -----------------------------------------------------------------
fn check_unit_tests(root: &Path, base: &str) -> CheckResult {
    let name = "unit tests touched".to_string();
    let files = changed_files(root, base);
    let mut src_modules: Vec<String> = Vec::new();
    let mut test_touched_modules: Vec<String> = Vec::new();
    for f in &files {
        let s = f.to_string_lossy().replace('\\', "/");
        if s.ends_with(".c") && (s.contains("/fsw/src/") || s.starts_with("fsw/src/")) {
            // Module = the path prefix before fsw/src/ ("" when fsw/ sits at repo root).
            let module = if let Some((pre, _)) = s.split_once("/fsw/src/") {
                pre.to_string()
            } else {
                String::new()
            };
            if !src_modules.contains(&module) {
                src_modules.push(module);
            }
        }
        let is_test = s.contains("unit-test") || s.contains("/ut-") || s.starts_with("ut-") || s.contains("/tests/");
        if is_test {
            // Module = leading path up to the test dir marker (best effort: first component).
            let module = s.split('/').next().unwrap_or("").to_string();
            test_touched_modules.push(module);
        }
    }
    if src_modules.is_empty() {
        return CheckResult {
            name,
            status: Status::Skip,
            detail: "no fsw/src code changed".into(),
            locations: vec![],
        };
    }
    let untested: Vec<&String> = src_modules
        .iter()
        .filter(|m| !test_touched_modules.iter().any(|t| m.starts_with(t.as_str()) || t.starts_with(m.as_str())))
        .collect();
    if untested.is_empty() {
        CheckResult { name, status: Status::Pass, detail: "changed modules also touch tests".into(), locations: vec![] }
    } else {
        CheckResult {
            name,
            status: Status::Warn,
            detail: format!(
                "code changed without test change in: {}",
                untested
                    .iter()
                    .map(|m| if m.is_empty() { "(repo root)" } else { m.as_str() })
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            locations: vec![],
        }
    }
}

// ---- check 5: trailing whitespace / tabs in added lines ------------------------------------------
fn check_whitespace(added: &[AddedLine]) -> CheckResult {
    let name = "whitespace (cFS style)".to_string();
    let mut locations = Vec::new();
    let mut notes = Vec::new();
    for (file, line, text) in added {
        let trailing = text.ends_with(' ') || text.ends_with('\t');
        let tab = text.contains('\t');
        if trailing || tab {
            let what = match (trailing, tab) {
                (true, true) => "trailing whitespace + tab",
                (true, false) => "trailing whitespace",
                _ => "tab",
            };
            notes.push(format!("{}:{} {what}", file.display(), line));
            locations.push((file.clone(), *line));
        }
    }
    if locations.is_empty() {
        CheckResult { name, status: Status::Pass, detail: "added lines are clean".into(), locations: vec![] }
    } else {
        CheckResult { name, status: Status::Fail, detail: notes.join("\n"), locations }
    }
}

// ---- check 6: CONTRIBUTING.md pointer -------------------------------------------------------------
fn check_contributing(root: &Path) -> CheckResult {
    let name = "CONTRIBUTING.md".to_string();
    if root.join("CONTRIBUTING.md").is_file() {
        CheckResult {
            name,
            status: Status::Skip,
            detail: "read it — CLA required for cFS".into(),
            locations: vec![],
        }
    } else {
        CheckResult { name, status: Status::Skip, detail: "no CONTRIBUTING.md in repo".into(), locations: vec![] }
    }
}

/// Compute all checks. Headless; safe to call from any thread.
pub fn run_checks(root: &Path) -> Vec<CheckResult> {
    if !git_available() || git(root, &["rev-parse", "HEAD"]).is_none() {
        return vec![CheckResult {
            name: "git".into(),
            status: Status::Skip,
            detail: "git unavailable or not a repository".into(),
            locations: vec![],
        }];
    }
    let Some(base) = find_base(root) else {
        return vec![CheckResult {
            name: "diff base".into(),
            status: Status::Skip,
            detail: "could not determine a merge base (origin/main / main / master / HEAD~1)".into(),
            locations: vec![],
        }];
    };
    let diff = git(root, &["diff", &base]).unwrap_or_default();
    let added = parse_added_lines(&diff);
    vec![
        check_format(root, &base),
        check_commit_messages(root, &base),
        check_doxygen(root, &added),
        check_unit_tests(root, &base),
        check_whitespace(&added),
        check_contributing(root),
    ]
}

// =================================================================================================
// tests — fixture git repo in a tmp dir; skip gracefully when git is unavailable.
// =================================================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    static N: AtomicUsize = AtomicUsize::new(0);

    fn sh(root: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(root)
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(ok, "git {args:?} failed");
    }

    /// Build the fixture: base commit on master, feature branch adding an undocumented C
    /// function with trailing whitespace + tab in app/fsw/src, no test change, no issue ref.
    fn fixture() -> Option<PathBuf> {
        if !git_available() {
            return None;
        }
        let dir = std::env::temp_dir().join(format!(
            "cauldron-checklist-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let src = dir.join("app/fsw/src");
        std::fs::create_dir_all(&src).ok()?;
        assert!(Command::new("git")
            .args(["init", "-b", "master"])
            .arg(&dir)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false));
        std::fs::write(src.join("foo.c"), "/** doc */\nint documented(void)\n{\n    return 0;\n}\n").ok()?;
        sh(&dir, &["add", "-A"]);
        sh(&dir, &["commit", "-m", "add base"]);
        sh(&dir, &["checkout", "-b", "feature"]);
        std::fs::write(
            src.join("foo.c"),
            "/** doc */\nint documented(void)\n{\n    return 0;\n}\n\nint32 NewThing(int x) \n{\n\treturn x;\n}\n",
        )
        .ok()?;
        sh(&dir, &["add", "-A"]);
        sh(&dir, &["commit", "-m", "Added stuff to the module"]);
        Some(dir)
    }

    fn get<'a>(results: &'a [CheckResult], name_part: &str) -> &'a CheckResult {
        results
            .iter()
            .find(|r| r.name.contains(name_part))
            .unwrap_or_else(|| panic!("missing check {name_part}"))
    }

    #[test]
    fn fixture_repo_checks() {
        let Some(dir) = fixture() else { return }; // git unavailable → skip
        let results = run_checks(&dir);
        assert_eq!(results.len(), 6, "{results:?}");

        // 1. format: no .clang-format in the fixture → Skip.
        assert_eq!(get(&results, "format-check").status, Status::Skip);

        // 2. commit message: non-imperative ("Added") + no issue ref → Warn.
        let cm = get(&results, "commit messages");
        assert_eq!(cm.status, Status::Warn, "{cm:?}");
        assert!(cm.detail.contains("issue"));

        // 3. doxygen: NewThing added without a doc block → Fail with a location.
        let dx = get(&results, "doxygen");
        assert_eq!(dx.status, Status::Fail, "{dx:?}");
        assert!(dx.detail.contains("NewThing"));
        assert!(!dx.locations.is_empty());

        // 4. fsw/src changed, no test files touched → Warn naming the module.
        let ut = get(&results, "unit tests");
        assert_eq!(ut.status, Status::Warn, "{ut:?}");
        assert!(ut.detail.contains("app"));

        // 5. trailing whitespace + tab in added lines → Fail with locations.
        let ws = get(&results, "whitespace");
        assert_eq!(ws.status, Status::Fail, "{ws:?}");
        assert!(!ws.locations.is_empty());

        // 6. no CONTRIBUTING.md → Skip.
        assert_eq!(get(&results, "CONTRIBUTING").status, Status::Skip);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn contributing_present_and_test_touch_clears_warn() {
        let Some(dir) = fixture() else { return };
        std::fs::write(dir.join("CONTRIBUTING.md"), "CLA required").unwrap();
        let tests = dir.join("app/unit-test");
        std::fs::create_dir_all(&tests).unwrap();
        std::fs::write(tests.join("foo_test.c"), "/* test */\n").unwrap();
        sh(&dir, &["add", "-A"]);
        sh(&dir, &["commit", "-m", "Add tests\n\nFixes #12"]);
        let results = run_checks(&dir);
        let c = get(&results, "CONTRIBUTING");
        assert_eq!(c.status, Status::Skip);
        assert!(c.detail.contains("CLA"));
        assert_eq!(get(&results, "unit tests").status, Status::Pass);
        // Issue now referenced, but "Added stuff" is still non-imperative → stays Warn.
        let cm = get(&results, "commit messages");
        assert!(!cm.detail.contains("no commit references"), "{cm:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn added_line_parser_and_fn_heuristic() {
        let diff = "\
diff --git a/x.c b/x.c
--- a/x.c
+++ b/x.c
@@ -1,2 +1,4 @@
 int keep;
+int32 NewFn(int a)
+{
 int tail;
";
        let added = parse_added_lines(diff);
        assert_eq!(added.len(), 2);
        assert_eq!(added[0], (PathBuf::from("x.c"), 2, "int32 NewFn(int a)".into()));
        assert!(looks_like_c_fn_def("int32 NewFn(int a)"));
        assert!(!looks_like_c_fn_def("    indented(x)"));
        assert!(!looks_like_c_fn_def("if (x)"));
        assert!(!looks_like_c_fn_def("CFE_ES_Call(x);"));
        assert!(!looks_like_c_fn_def("DoCall(x)")); // call, single token before '('
    }

    #[test]
    fn find_base_on_default_branch_falls_back_to_parent() {
        let Some(dir) = fixture() else { return };
        // Sitting ON master with a clean tree: merge-base(HEAD, master) == HEAD, which must
        // NOT be used as the base (empty diff) — fall back to HEAD~1.
        sh(&dir, &["checkout", "master"]);
        std::fs::write(dir.join("extra.c"), "int x;\n").unwrap();
        sh(&dir, &["add", "-A"]);
        sh(&dir, &["commit", "-m", "add extra"]);
        let base = find_base(&dir).expect("base");
        let head = git(&dir, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        let parent = git(&dir, &["rev-parse", "HEAD~1"]).unwrap().trim().to_string();
        assert_ne!(base, head);
        assert_eq!(base, parent);
        // With a dirty worktree, HEAD itself is a fine base again.
        std::fs::write(dir.join("extra.c"), "int x; int y;\n").unwrap();
        assert_eq!(find_base(&dir).as_deref(), Some(head.as_str()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn long_multibyte_subject_does_not_panic() {
        let Some(dir) = fixture() else { return };
        let subject = format!("Zäöü{}", "é".repeat(80)); // > 72 chars, multibyte around byte 40
        sh(&dir, &["commit", "--allow-empty", "-m", &subject]);
        let cm = check_commit_messages(&dir, &find_base(&dir).unwrap());
        assert_eq!(cm.status, Status::Fail, "{cm:?}");
        assert!(cm.detail.contains("72 chars"));
        assert_eq!(truncate_chars("héllo", 2), "hé");
        assert_eq!(truncate_chars("hi", 40), "hi");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn issue_reference_detection() {
        assert!(references_issue("Fix #12"));
        assert!(references_issue("this Fixes #7 properly"));
        assert!(references_issue("Resolves  #3"));
        assert!(!references_issue("fix the thing"));
        assert!(!references_issue("prefix #notanumber"));
    }
}
