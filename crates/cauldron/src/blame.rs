//! Inline git blame — the faint `author, time · summary` annotation at the end of the caret
//! line (GitLens / JetBrains "inline blame" style).
//!
//! Split the usual way: a PURE `git blame --line-porcelain` parser + a pure relative-time
//! formatter (both unit-tested), and a small background service (thread + mpsc, the GitPanel
//! shape) so blaming a big file never stalls a frame. The app caches per-path results and only
//! shows annotations for CLEAN buffers — a dirty buffer's lines have shifted relative to disk,
//! and a wrong attribution is worse than none.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};

/// Blame for one line of the file on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineBlame {
    pub author: String,
    /// Unix seconds of the author time.
    pub time: i64,
    pub summary: String,
    /// All-zero sha = not committed yet.
    pub uncommitted: bool,
}

/// Parse `git blame --line-porcelain` output into per-line records, indexed by 0-based FINAL
/// line number. `--line-porcelain` repeats the full header for every line, so each record is
/// self-contained: a sha line (`<sha> <orig> <final> [<n>]`), tag lines (`author …`,
/// `author-time …`, `summary …`), then the content line prefixed with a tab. Garbage-safe:
/// anything malformed is skipped, never panicked on.
pub fn parse_line_porcelain(text: &str) -> Vec<LineBlame> {
    let mut out: Vec<LineBlame> = Vec::new();
    let mut cur_line: Option<usize> = None; // 0-based final line of the record being built
    let mut author = String::new();
    let mut time = 0i64;
    let mut summary = String::new();
    let mut uncommitted = false;

    for line in text.lines() {
        if let Some(content_less) = line.strip_prefix('\t') {
            let _ = content_less; // the code line itself — closes the record
            if let Some(ln) = cur_line.take() {
                if out.len() <= ln {
                    out.resize(
                        ln + 1,
                        LineBlame {
                            author: String::new(),
                            time: 0,
                            summary: String::new(),
                            uncommitted: true,
                        },
                    );
                }
                out[ln] = LineBlame {
                    author: std::mem::take(&mut author),
                    time,
                    summary: std::mem::take(&mut summary),
                    uncommitted,
                };
            }
            continue;
        }
        // Header line of a new record: "<40-hex-sha> <orig> <final> [<n>]".
        let mut toks = line.split(' ');
        if let (Some(sha), Some(_orig), Some(fin)) = (toks.next(), toks.next(), toks.next()) {
            if (sha.len() == 40 || sha.len() == 64) && sha.bytes().all(|b| b.is_ascii_hexdigit())
            {
                if let Ok(fin) = fin.parse::<usize>() {
                    cur_line = fin.checked_sub(1);
                    uncommitted = sha.bytes().all(|b| b == b'0');
                    author.clear();
                    summary.clear();
                    time = 0;
                    continue;
                }
            }
        }
        if cur_line.is_none() {
            continue;
        }
        if let Some(v) = line.strip_prefix("author ") {
            author = v.to_string();
        } else if let Some(v) = line.strip_prefix("author-time ") {
            time = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("summary ") {
            summary = v.to_string();
        }
    }
    out
}

/// "just now" / "5 min ago" / "3 hours ago" / "yesterday" / "6 days ago" / "3 weeks ago" /
/// "4 months ago" / "2 years ago". `now` and `then` are unix seconds; a `then` in the future
/// (clock skew) reads as "just now".
pub fn relative_time(now: i64, then: i64) -> String {
    let s = now.saturating_sub(then);
    match s {
        i64::MIN..=59 => "just now".into(),
        60..=3_599 => format!("{} min ago", s / 60),
        3_600..=86_399 => {
            let h = s / 3_600;
            if h == 1 { "an hour ago".into() } else { format!("{h} hours ago") }
        }
        86_400..=172_799 => "yesterday".into(),
        172_800..=604_799 => format!("{} days ago", s / 86_400),
        604_800..=2_591_999 => {
            let w = s / 604_800;
            if w == 1 { "a week ago".into() } else { format!("{w} weeks ago") }
        }
        2_592_000..=31_535_999 => {
            let m = s / 2_592_000;
            match m {
                1 => "a month ago".into(),
                12.. => "a year ago".into(),
                _ => format!("{m} months ago"),
            }
        }
        _ => {
            let y = s / 31_536_000;
            if y == 1 { "a year ago".into() } else { format!("{y} years ago") }
        }
    }
}

/// The one-line annotation for a blamed line. Uncommitted lines are labeled plainly instead of
/// showing git's placeholder author.
pub fn annotation(b: &LineBlame, now: i64) -> String {
    if b.uncommitted {
        return "uncommitted changes".into();
    }
    let mut s = format!("{}, {}", b.author, relative_time(now, b.time));
    if !b.summary.is_empty() {
        s.push_str(" · ");
        s.push_str(&b.summary);
    }
    s
}

// =================================================================================================
// background service — GitPanel's thread + mpsc shape
// =================================================================================================

enum Msg {
    /// Blame finished for a path: per-line records (empty = failed / untracked → cached as
    /// "known absent" so we don't respawn every frame). Carries the request epoch: a Done from
    /// BEFORE an invalidate must be dropped, or stale (line-shifted) blame re-enters the cache —
    /// the same guard GitPanel::pump applies with its seq counter.
    Done(PathBuf, u64, Vec<LineBlame>),
}

/// Per-file blame state in the app cache.
pub enum FileBlame {
    /// Request in flight (the epoch it was issued at).
    Pending(u64),
    /// Parsed (empty = untracked/unreadable — a terminal state until invalidated).
    Ready(Vec<LineBlame>),
}

pub struct BlameService {
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    /// Bumped per request; a Done with a non-current epoch for its entry is dropped.
    epoch: u64,
    pub cache: HashMap<PathBuf, FileBlame>,
}

impl Default for BlameService {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self { tx, rx, epoch: 0, cache: HashMap::new() }
    }
}

impl BlameService {
    /// Kick a background blame for `abs` if nothing is cached or in flight.
    pub fn request(&mut self, root: &Path, abs: &Path, ctx: &egui::Context) {
        if self.cache.contains_key(abs) {
            return;
        }
        // Crude memory bound: blame for dozens of files is cheap, unbounded growth is not.
        if self.cache.len() > 64 {
            self.cache.clear();
        }
        self.epoch += 1;
        let epoch = self.epoch;
        self.cache.insert(abs.to_path_buf(), FileBlame::Pending(epoch));
        let tx = self.tx.clone();
        let root = root.to_path_buf();
        let abs = abs.to_path_buf();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("git-blame".into())
            .spawn(move || {
                let lines = blame_file(&root, &abs).unwrap_or_default();
                let _ = tx.send(Msg::Done(abs, epoch, lines));
                ctx.request_repaint();
            })
            .ok();
    }

    /// Drain finished blames into the cache (call once per frame). A Done only lands if its
    /// path still has a Pending entry from the SAME epoch — anything else (invalidated, cleared,
    /// superseded by a newer request) is stale and dropped.
    pub fn pump(&mut self) {
        while let Ok(Msg::Done(path, epoch, lines)) = self.rx.try_recv() {
            match self.cache.get(&path) {
                Some(FileBlame::Pending(e)) if *e == epoch => {
                    self.cache.insert(path, FileBlame::Ready(lines));
                }
                _ => {} // stale: entry invalidated/cleared/re-requested since this spawned
            }
        }
    }

    /// Disk content changed for `abs` (save, external reload, hunk revert) — drop its blame.
    pub fn invalidate(&mut self, abs: &Path) {
        self.cache.remove(abs);
    }

    /// Repo-wide change (project switch, branch checkout, commit) — drop everything.
    pub fn clear(&mut self) {
        self.cache.clear();
    }
}

/// Run `git blame --line-porcelain` for one file. `Err`/non-zero exit → None (untracked, new
/// repo, binary…): the caller caches the miss.
fn blame_file(root: &Path, abs: &Path) -> Option<Vec<LineBlame>> {
    let rel = abs.strip_prefix(root).ok()?;
    // Unlike `git diff`, blame's positional argument is a PATH, not a pathspec — `:(literal)`
    // is rejected ("no such path"), and bracketed filenames are already taken literally.
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["blame", "--line-porcelain", "--"])
        .arg(rel)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(parse_line_porcelain(&String::from_utf8_lossy(&out.stdout)))
}

// =================================================================================================
// tests
// =================================================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_line_porcelain_records() {
        let text = "\
1111111111111111111111111111111111111111 1 1 2
author Alice
author-mail <a@x>
author-time 1700000000
author-tz +0000
committer Alice
summary first change
filename f.txt
\tline one content
1111111111111111111111111111111111111111 2 2
author Alice
author-time 1700000000
summary first change
filename f.txt
\tline two content
0000000000000000000000000000000000000000 3 3 1
author Not Committed Yet
author-time 1700000500
summary Version of f.txt from f.txt
filename f.txt
\tdirty line
";
        let lines = parse_line_porcelain(text);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].author, "Alice");
        assert_eq!(lines[0].time, 1_700_000_000);
        assert_eq!(lines[0].summary, "first change");
        assert!(!lines[0].uncommitted);
        assert_eq!(lines[1].author, "Alice");
        assert!(lines[2].uncommitted, "all-zero sha = uncommitted");
    }

    #[test]
    fn parser_survives_garbage() {
        assert!(parse_line_porcelain("").is_empty());
        assert!(parse_line_porcelain("random\nnoise\n\tno header\n").is_empty());
        // Header with a bogus line number is skipped, not panicked on.
        let t = "1111111111111111111111111111111111111111 1 zero\nauthor A\n\tx\n";
        assert!(parse_line_porcelain(t).is_empty());
    }

    #[test]
    fn relative_times_read_naturally() {
        let now = 1_700_000_000;
        assert_eq!(relative_time(now, now - 10), "just now");
        assert_eq!(relative_time(now, now + 500), "just now"); // clock skew
        assert_eq!(relative_time(now, now - 300), "5 min ago");
        assert_eq!(relative_time(now, now - 3 * 3600), "3 hours ago");
        assert_eq!(relative_time(now, now - 3600), "an hour ago");
        assert_eq!(relative_time(now, now - 100_000), "yesterday");
        assert_eq!(relative_time(now, now - 4 * 86_400), "4 days ago");
        assert_eq!(relative_time(now, now - 8 * 86_400), "a week ago");
        assert_eq!(relative_time(now, now - 40 * 86_400), "a month ago");
        assert_eq!(relative_time(now, now - 800 * 86_400), "2 years ago");
    }

    #[test]
    fn annotation_formats_and_flags_uncommitted() {
        let now = 1_700_000_000;
        let b = LineBlame {
            author: "Alice".into(),
            time: now - 300,
            summary: "fix the thing".into(),
            uncommitted: false,
        };
        assert_eq!(annotation(&b, now), "Alice, 5 min ago · fix the thing");
        let dirty = LineBlame { uncommitted: true, ..b };
        assert_eq!(annotation(&dirty, now), "uncommitted changes");
    }

    /// End-to-end against real git: commit a file as a known author, blame it, check the record.
    /// Skips silently when git isn't available.
    #[test]
    fn blame_against_a_real_repo() {
        let dir = std::env::temp_dir().join(format!("cauldron-blame-{}", std::process::id()));
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
            return;
        }
        let _ = git(&["config", "user.email", "t@t"]);
        let _ = git(&["config", "user.name", "Test Author"]);
        std::fs::write(dir.join("f.txt"), "alpha\nbeta\n").unwrap();
        let _ = git(&["add", "."]);
        let _ = git(&["commit", "-qm", "the summary line"]);
        // One uncommitted edit on line 2.
        std::fs::write(dir.join("f.txt"), "alpha\nBETA\n").unwrap();

        let lines = blame_file(&dir, &dir.join("f.txt")).unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].author, "Test Author");
        assert_eq!(lines[0].summary, "the summary line");
        assert!(!lines[0].uncommitted);
        assert!(lines[1].uncommitted, "edited line blames as uncommitted");

        // Untracked file → None → cached as a miss.
        std::fs::write(dir.join("new.txt"), "x\n").unwrap();
        assert!(blame_file(&dir, &dir.join("new.txt")).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
