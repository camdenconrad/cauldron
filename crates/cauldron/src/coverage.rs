//! Test coverage — the per-line gutter marks (covered moss / uncovered ember) fed by LCOV.
//!
//! LCOV is the one format everything can emit: `cargo llvm-cov --lcov`, coverage.py's
//! `coverage lcov`, coverlet's lcov output. The parser is PURE and unit-tested; the runner is a
//! background thread (blame/history shape) that executes the framework's coverage command and
//! parses the resulting file. Marks are a snapshot of the run — they go stale as you edit, like
//! every IDE's coverage view; re-run to refresh.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};

use crate::testrun::Framework;

/// Per-file, 0-based line → was it executed at least once.
pub type FileCoverage = Vec<(usize, bool)>;

/// Parse LCOV text: `SF:<path>` opens a file section, `DA:<line>,<hits>` records a line,
/// `end_of_record` closes it. Relative SF paths resolve against `root`. Garbage-safe.
pub fn parse_lcov(text: &str, root: &Path) -> HashMap<PathBuf, FileCoverage> {
    let mut out: HashMap<PathBuf, FileCoverage> = HashMap::new();
    let mut cur: Option<(PathBuf, FileCoverage)> = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(sf) = line.strip_prefix("SF:") {
            let p = PathBuf::from(sf.trim());
            let abs = if p.is_absolute() { p } else { root.join(p) };
            cur = Some((abs, Vec::new()));
        } else if let Some(da) = line.strip_prefix("DA:") {
            if let Some((_, lines)) = cur.as_mut() {
                let mut it = da.split(',');
                if let (Some(l), Some(hits)) = (it.next(), it.next()) {
                    if let (Ok(l), Ok(hits)) = (l.trim().parse::<usize>(), hits.trim().parse::<u64>())
                    {
                        if let Some(l0) = l.checked_sub(1) {
                            lines.push((l0, hits > 0));
                        }
                    }
                }
            }
        } else if line == "end_of_record" {
            if let Some((path, mut lines)) = cur.take() {
                lines.sort_unstable_by_key(|(l, _)| *l);
                lines.dedup_by_key(|(l, _)| *l);
                if !lines.is_empty() {
                    out.insert(path, lines);
                }
            }
        }
    }
    out
}

/// `(covered, total)` instrumented lines for a status summary.
pub fn totals(cov: &HashMap<PathBuf, FileCoverage>) -> (usize, usize) {
    let mut covered = 0;
    let mut total = 0;
    for lines in cov.values() {
        total += lines.len();
        covered += lines.iter().filter(|(_, c)| *c).count();
    }
    (covered, total)
}

// =================================================================================================
// background runner
// =================================================================================================

enum Msg {
    Done(HashMap<PathBuf, FileCoverage>),
    Failed(String),
}

pub struct CoverageService {
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    pub running: bool,
    /// The last successful run's marks (empty until then).
    pub files: HashMap<PathBuf, FileCoverage>,
    /// One-shot: a run just finished (the app pushes marks to open views + reports).
    pub finished: Option<Result<(usize, usize), String>>,
}

impl Default for CoverageService {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self { tx, rx, running: false, files: HashMap::new(), finished: None }
    }
}

impl CoverageService {
    /// Run the project's tests under coverage in the background. Framework support:
    /// cargo (via cargo-llvm-cov) and pytest (via coverage.py); anything else fails with a
    /// human explanation.
    pub fn run(&mut self, root: &Path, ctx: &egui::Context) {
        if self.running {
            return;
        }
        self.running = true;
        let tx = self.tx.clone();
        let root = root.to_path_buf();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("coverage".into())
            .spawn(move || {
                let msg = run_coverage(&root);
                let _ = tx.send(msg);
                ctx.request_repaint();
            })
            .ok();
    }

    /// Drain the result (call once per frame).
    pub fn pump(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            self.running = false;
            match msg {
                Msg::Done(files) => {
                    let t = totals(&files);
                    self.files = files;
                    self.finished = Some(Ok(t));
                }
                Msg::Failed(e) => self.finished = Some(Err(e)),
            }
        }
    }
}

fn run_coverage(root: &Path) -> Msg {
    let out_path = root.join(".cauldron").join("coverage.lcov");
    let _ = std::fs::create_dir_all(out_path.parent().unwrap());
    let _ = std::fs::remove_file(&out_path);
    let fw = Framework::detect(root);
    let status = match fw {
        Framework::CargoTest => {
            // cargo-llvm-cov is the maintained coverage front-end for stable Rust.
            let probe_ok = std::process::Command::new("cargo")
                .current_dir(root)
                .args(["llvm-cov", "--version"])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if !probe_ok {
                return Msg::Failed(
                    "cargo-llvm-cov not found — install it: cargo install cargo-llvm-cov".into(),
                );
            }
            std::process::Command::new("cargo")
                .current_dir(root)
                .args(["llvm-cov", "test", "--workspace", "--lcov", "--output-path"])
                .arg(&out_path)
                .output()
        }
        Framework::Pytest => {
            // Through the project's resolved interpreter (the venv when one exists), never a
            // bare `coverage` from $PATH: on PEP-668 distros the deps installer puts coverage
            // and the project's imports venv-only, so the $PATH copy was 'command not found'
            // or couldn't import the project.
            let py = shquote(&crate::runconfig::python_bin(root));
            std::process::Command::new("sh")
                .current_dir(root)
                .arg("-c")
                .arg(format!(
                    "{py} -m coverage run -m pytest && {py} -m coverage lcov -o {}",
                    shquote(&out_path.display().to_string())
                ))
                .output()
        }
        other => {
            return Msg::Failed(format!(
                "coverage runs are wired for cargo and pytest projects (this looks like {other:?})",
            ));
        }
    };
    match status {
        Err(e) => Msg::Failed(format!("coverage run failed to start: {e}")),
        Ok(o) => {
            let text = match std::fs::read_to_string(&out_path) {
                Ok(t) => t,
                Err(_) => {
                    let err = String::from_utf8_lossy(&o.stderr);
                    let tail: String = err.lines().rev().take(4).collect::<Vec<_>>().join(" · ");
                    return Msg::Failed(if tail.is_empty() {
                        "coverage produced no lcov output".into()
                    } else {
                        format!("coverage failed: {tail}")
                    });
                }
            };
            let files = parse_lcov(&text, root);
            if files.is_empty() {
                Msg::Failed("lcov output parsed to zero files".into())
            } else {
                Msg::Done(files)
            }
        }
    }
}

/// Single-quote for `sh -c` interpolation.
fn shquote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

// =================================================================================================
// tests
// =================================================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lcov_sections() {
        let root = Path::new("/repo");
        let text = "\
TN:
SF:src/lib.rs
DA:1,5
DA:2,0
DA:10,1
end_of_record
SF:/abs/other.rs
DA:3,0
end_of_record
SF:skipped_no_end.rs
DA:1,1
";
        let cov = parse_lcov(text, root);
        assert_eq!(cov.len(), 2, "unterminated section dropped");
        let lib = &cov[&PathBuf::from("/repo/src/lib.rs")];
        assert_eq!(lib, &vec![(0, true), (1, false), (9, true)]);
        let other = &cov[&PathBuf::from("/abs/other.rs")];
        assert_eq!(other, &vec![(2, false)]);
        assert_eq!(totals(&cov), (2, 4));
    }

    #[test]
    fn lcov_parser_survives_garbage() {
        assert!(parse_lcov("", Path::new("/")).is_empty());
        assert!(parse_lcov("DA:1,1\nend_of_record\n", Path::new("/")).is_empty());
        assert!(parse_lcov("SF:f\nDA:zero,one\nDA:0,1\nend_of_record\n", Path::new("/")).is_empty());
        // Duplicate DA lines dedup; line 0 (invalid 1-based) is skipped via checked_sub.
        let cov = parse_lcov("SF:f\nDA:2,1\nDA:2,0\nend_of_record\n", Path::new("/r"));
        assert_eq!(cov[&PathBuf::from("/r/f")].len(), 1);
    }
}
