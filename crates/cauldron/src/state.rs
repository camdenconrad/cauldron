//! Session persistence: reopen a project EXACTLY where you left it — same files, same order,
//! same splits, same carets, same panels. One JSON per project root at
//! `~/.local/share/cauldron/sessions/<hash>.json`, saved on exit and on project switch.
//!
//! Also home of the LAST-PROJECT pointer (`~/.local/share/cauldron/last-project`) and the
//! project-worthiness rule: `$HOME`, `/`, and ancestors of `$HOME` are NEVER projects — a
//! dock launch (cwd = `$HOME`) must boot into the last real project, not walk the home dir.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{BottomTab, RightTab};

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Session {
    /// One inner Vec per editor group (split), in order — the open tabs' absolute paths.
    pub groups: Vec<Vec<PathBuf>>,
    /// Active tab index per group.
    pub actives: Vec<usize>,
    pub focused: usize,
    /// Primary caret byte per open file (jump target on restore).
    pub carets: Vec<(PathBuf, usize)>,
    pub pins: Vec<PathBuf>,
    pub project_open: bool,
    pub pins_open: bool,
    pub terminal_open: bool,
    pub bottom_open: bool,
    /// Which dock tab was selected. Supersedes the old `bottom_problems: bool`, which could only
    /// say Problems-or-Output and collapsed Git/Usages/Debug/Tests/Checks into "not Problems".
    /// Sessions written by that build carry the dead bool (serde ignores it) and lack this field,
    /// so they land on the default — a one-time reset of the selected tab, nothing else.
    #[serde(default)]
    pub bottom_tab: BottomTab,
    #[serde(default)]
    pub right_tab: RightTab,
    /// Breakpoints per file: 1-based line + optional condition expression. Sessions written by
    /// older builds lack the field and default to none.
    #[serde(default)]
    pub breakpoints: Vec<(PathBuf, Vec<(u32, Option<String>)>)>,
    /// Bookmarks per file: 1-based lines.
    #[serde(default)]
    pub bookmarks: Vec<(PathBuf, Vec<u32>)>,
}

/// May `path` be treated as a project root? Rejects `$HOME` itself, `/`, and every ancestor
/// of `$HOME` (`/home`, …) — opening any of those walks half the filesystem and hijacks the
/// session keyed to it. Everything else (including dirs elsewhere under `$HOME`) is fine.
pub fn project_worthy(path: &Path, home: Option<&Path>) -> bool {
    if path.as_os_str().is_empty() || path == Path::new("/") {
        return false;
    }
    match home {
        // starts_with(path) is true for home == path too, so equality + ancestors in one test.
        Some(h) => !h.starts_with(path),
        None => true,
    }
}

/// Sentinel root for NO-PROJECT mode — an absolute path that is never created, so every fs
/// probe against it (workspace walk, `Cargo.toml` checks, git) fails fast and quietly.
pub fn no_project_root() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(h) => PathBuf::from(h).join(".local/share/cauldron/no-project"),
        None => PathBuf::from("/nonexistent/cauldron-no-project"),
    }
}

fn last_project_file() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".local/share/cauldron/last-project"))
}

/// Persist `root` as the last opened project (written on session save and on open-folder).
/// Unworthy roots ($HOME opened explicitly via CLI arg, `/`) are NOT recorded — the next
/// no-arg boot must never land there. Relative roots are NOT recorded either: a pointer
/// like `.` would be re-resolved against the NEXT process's cwd ($HOME on a dock launch)
/// and open a $HOME walk (the roots `App::new` produces are canonicalized, so this is a
/// backstop). Best-effort.
pub fn save_last_project(root: &Path) {
    if !root.is_absolute() {
        return;
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if !project_worthy(root, home.as_deref()) {
        return;
    }
    let Some(file) = last_project_file() else { return };
    write_last_project(&file, root);
}

/// The last-project pointer, if it still names an existing, project-worthy directory.
pub fn load_last_project() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    read_last_project(&last_project_file()?, home.as_deref())
}

fn write_last_project(file: &Path, root: &Path) {
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(file, format!("{}\n", root.display()));
}

/// READ-time defense, mirroring `openfolder::parse_recents`: a poisoned pointer (a relative
/// `.`/`src` written by a pre-canonicalize build — `is_dir()` would resolve it against the
/// CURRENT cwd, i.e. $HOME on a dock launch — or a literal `/home/user` written under a
/// different $HOME / by hand) must never boot into a home-directory walk. The write-time
/// guard in [`save_last_project`] cannot protect against pre-existing or foreign files.
fn read_last_project(file: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let text = std::fs::read_to_string(file).ok()?;
    let p = PathBuf::from(text.trim());
    (p.is_absolute() && p.is_dir() && project_worthy(&p, home)).then_some(p)
}

fn session_file(root: &Path) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut h = DefaultHasher::new();
    canon.hash(&mut h);
    Some(
        PathBuf::from(home)
            .join(".local/share/cauldron/sessions")
            .join(format!("{:016x}.json", h.finish())),
    )
}

/// Persist `session` for `root`. Best-effort — a failed save never bothers the user.
pub fn save(root: &Path, session: &Session) {
    let Some(file) = session_file(root) else { return };
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string_pretty(session) {
        let _ = std::fs::write(file, json);
    }
}

/// Load the saved session for `root` (missing/corrupt → None). Dead file paths are dropped so a
/// renamed tree restores as much as still exists.
pub fn load(root: &Path) -> Option<Session> {
    let file = session_file(root)?;
    let text = std::fs::read_to_string(file).ok()?;
    let mut s: Session = serde_json::from_str(&text).ok()?;
    for g in &mut s.groups {
        g.retain(|p| p.is_file());
    }
    s.groups.retain(|g| !g.is_empty());
    s.pins.retain(|p| p.is_file());
    s.carets.retain(|(p, _)| p.is_file());
    s.breakpoints.retain(|(p, _)| p.is_file());
    s.bookmarks.retain(|(p, _)| p.is_file());
    while s.actives.len() < s.groups.len() {
        s.actives.push(0);
    }
    s.actives.truncate(s.groups.len().max(1));
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_worthy_rejects_home_root_and_home_ancestors() {
        let home = Path::new("/home/user");
        // The rule: reject only $HOME, /, and ancestors of $HOME.
        assert!(!project_worthy(Path::new("/"), Some(home)));
        assert!(!project_worthy(Path::new("/home"), Some(home)));
        assert!(!project_worthy(Path::new("/home/user"), Some(home)));
        assert!(!project_worthy(Path::new("/home/user/"), Some(home)), "trailing slash same");
        assert!(!project_worthy(Path::new(""), Some(home)));
        // Real projects — under home or elsewhere — pass.
        assert!(project_worthy(Path::new("/home/user/RustroverProjects/cauldron"), Some(home)));
        assert!(project_worthy(Path::new("/opt/src/cfs"), Some(home)));
        assert!(project_worthy(Path::new("/home/other"), Some(home)), "sibling of home is fine");
        // No HOME at all: only / and empty are rejected.
        assert!(!project_worthy(Path::new("/"), None));
        assert!(project_worthy(Path::new("/srv/proj"), None));
    }

    #[test]
    fn last_project_pointer_round_trip_and_dead_target() {
        let dir = std::env::temp_dir().join(format!("cauldron-lastproj-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("nested/last-project"); // parent dir is created on write
        let proj = dir.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let home = Some(Path::new("/home/user"));

        assert_eq!(read_last_project(&file, home), None, "missing pointer file");
        write_last_project(&file, &proj);
        assert_eq!(read_last_project(&file, home), Some(proj.clone()));
        // Pointer target vanished (project deleted/renamed) → treated as absent.
        std::fs::remove_dir_all(&proj).unwrap();
        assert_eq!(read_last_project(&file, home), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A poisoned pointer must never boot a $HOME (or cwd-dependent) walk: relative paths
    /// (`.` written by a pre-canonicalize build would resolve against the NEW process cwd —
    /// $HOME on a dock launch) and unworthy absolutes ($HOME itself, its ancestors) are
    /// rejected at READ time, mirroring the recents picker's parse-time defense.
    #[test]
    fn last_project_pointer_rejects_relative_and_unworthy_paths() {
        let dir =
            std::env::temp_dir().join(format!("cauldron-lastproj-poison-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("last-project");
        let home = Some(Path::new("/home/user"));

        // Relative pointers: "." IS a dir relative to any cwd — must still be rejected.
        for poisoned in [".", "src", "./proj"] {
            std::fs::write(&file, format!("{poisoned}\n")).unwrap();
            assert_eq!(read_last_project(&file, home), None, "relative pointer {poisoned:?}");
        }
        // Unworthy absolutes: $HOME and its ancestors (hand-edited / written under another
        // $HOME) — exactly the walk the boot guard exists to kill.
        std::fs::write(&file, "/home/user\n").unwrap();
        assert_eq!(read_last_project(&file, home), None, "$HOME pointer");
        std::fs::write(&file, "/\n").unwrap();
        assert_eq!(read_last_project(&file, home), None, "/ pointer");
        // The same path is fine when it is NOT the current $HOME's territory.
        std::fs::write(&file, format!("{}\n", dir.display())).unwrap();
        assert_eq!(read_last_project(&file, home), Some(dir.clone()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn round_trip_and_dead_path_filtering() {
        let dir = std::env::temp_dir().join(format!("cauldron-sess-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let alive = dir.join("alive.rs");
        std::fs::write(&alive, "x").unwrap();
        let dead = dir.join("gone.rs");

        // Point HOME at the temp dir so the test never touches the real sessions dir.
        let _home = crate::testenv::HomeGuard::set(&dir);

        let sess = Session {
            groups: vec![vec![alive.clone(), dead.clone()], vec![dead.clone()]],
            actives: vec![1, 0],
            focused: 1,
            carets: vec![(alive.clone(), 42), (dead.clone(), 7)],
            pins: vec![alive.clone(), dead.clone()],
            project_open: true,
            pins_open: true,
            terminal_open: true,
            bottom_open: true,
            bottom_tab: BottomTab::Git,
            right_tab: RightTab::Structure,
            breakpoints: vec![
                (alive.clone(), vec![(3, None), (9, Some("i == 7".into()))]),
                (dead.clone(), vec![(1, None)]),
            ],
            bookmarks: vec![(alive.clone(), vec![5]), (dead.clone(), vec![2])],
        };
        save(&dir, &sess);
        let back = load(&dir).expect("session loads");
        assert_eq!(back.groups, vec![vec![alive.clone()]], "dead paths + empty groups dropped");
        assert_eq!(back.actives.len(), 1);
        assert_eq!(back.carets, vec![(alive.clone(), 42)]);
        assert_eq!(back.pins, vec![alive.clone()]);
        assert_eq!(
            back.breakpoints,
            vec![(alive.clone(), vec![(3, None), (9, Some("i == 7".into()))])],
            "breakpoints round-trip with conditions; dead paths pruned",
        );
        assert_eq!(back.bookmarks, vec![(alive, vec![5])], "bookmarks round-trip + prune");
        assert!(back.terminal_open);
        // The selected tabs survive as themselves — Git is one of the five the old
        // `bottom_problems: bool` could not represent at all.
        assert_eq!(back.bottom_tab, BottomTab::Git);
        assert_eq!(back.right_tab, RightTab::Structure);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A session file written by the PREVIOUS build carries `bottom_problems` (which this struct
    /// no longer has) and lacks `bottom_tab` / `right_tab`. It must still load — landing on the
    /// defaults — rather than failing to parse and silently discarding the user's whole session,
    /// tabs and all. Deliberately a hardcoded old-format string: round-tripping a struct would
    /// test nothing, since the struct can no longer produce the old shape.
    #[test]
    fn old_session_file_without_tab_fields_still_loads() {
        let dir = std::env::temp_dir().join(format!("cauldron-sess-mig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let alive = dir.join("alive.rs");
        std::fs::write(&alive, "x").unwrap();

        let _home = crate::testenv::HomeGuard::set(&dir);

        // Exactly the shape the shipped build writes today.
        let old_json = format!(
            r#"{{
              "groups": [["{p}"]],
              "actives": [0],
              "focused": 0,
              "carets": [["{p}", 12]],
              "pins": [],
              "project_open": true,
              "pins_open": false,
              "terminal_open": true,
              "bottom_open": true,
              "bottom_problems": true
            }}"#,
            p = alive.display()
        );
        let file = session_file(&dir).unwrap();
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, old_json).unwrap();

        let back = load(&dir).expect("a pre-existing session file must still load");
        assert_eq!(back.groups, vec![vec![alive.clone()]], "the real session survives");
        assert_eq!(back.carets, vec![(alive, 12)]);
        assert!(back.terminal_open, "old fields still read");
        assert_eq!(back.bottom_tab, BottomTab::default(), "missing field → default");
        assert_eq!(back.right_tab, RightTab::default(), "missing field → default");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
