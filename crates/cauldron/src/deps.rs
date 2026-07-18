//! C dependency auto-resolution: give clangd real compile information instead of letting it
//! guess (a fresh cFS clone shows 20+ phantom errors purely from unresolved includes).
//!
//! Two tiers, both background:
//! - **Tier 1 (instant):** generate `compile_flags.txt` at the project root — `-I` for every
//!   directory in the tree that contains headers. Fixes include resolution immediately; clangd
//!   picks the file up by convention. The file (and `.cauldron/`) are added to
//!   `.git/info/exclude` so the user's repo stays clean.
//! - **Tier 2 (fidelity):** produce a real `compile_commands.json` — cFS via
//!   `make SIMULATION=native prep` (sub-build DBs merged into `build/compile_commands.json`),
//!   generic CMake via `cmake -B .cauldron/build -DCMAKE_EXPORT_COMPILE_COMMANDS=ON`. When it
//!   lands, the app hot-restarts clangd (kill → the manager's respawn path rediscovers the DB).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Directories never worth `-I` (build junk, VCS, unit-test stubs that shadow real headers).
const SKIP_DIRS: &[&str] = &[".git", ".cauldron", "build", "ut-stubs", "ut_assert", "unit-test", "coveragetest"];

/// Kick the resolver for a C workspace. `files` = the canonical workspace file universe
/// (`Workspace::all_files` — Tier 1 derives header dirs from it instead of re-walking).
/// Reports progress into `status`; sets `restart_clangd` when a better compile DB appeared
/// and clangd should be bounced.
pub fn resolve_c_deps(
    root: PathBuf,
    files: Vec<PathBuf>,
    status: Arc<Mutex<Option<String>>>,
    restart_clangd: Arc<AtomicBool>,
    ctx: egui::Context,
) {
    std::thread::Builder::new()
        .name("cauldron-c-deps".into())
        .spawn(move || {
            let say = |s: &str| {
                *status.lock().unwrap_or_else(|p| p.into_inner()) = Some(s.to_string());
                ctx.request_repaint();
            };

            // Already have a real DB? Nothing to do.
            if cauldron_lsp::discovery::clangd_compile_db(&root, None).is_some() {
                return;
            }

            // ---- Tier 1: compile_flags.txt (instant include resolution) ------------------
            let flags_path = root.join("compile_flags.txt");
            if !flags_path.exists() {
                let dirs = header_dirs(&root, &files);
                if !dirs.is_empty() {
                    let mut body = String::from("-std=c99\n");
                    for d in &dirs {
                        body.push_str(&format!("-I{}\n", d.display()));
                    }
                    if std::fs::write(&flags_path, body).is_ok() {
                        exclude_from_git(&root, &["compile_flags.txt", ".cauldron/"]);
                        say(&format!("deps: include paths resolved ({} dirs) — reloading clangd", dirs.len()));
                        restart_clangd.store(true, Ordering::SeqCst);
                        ctx.request_repaint();
                    }
                }
            }

            // ---- Tier 2: a real compile_commands.json --------------------------------------
            let made_db = if is_cfs(&root) {
                say("deps: building cFS compile DB (make prep — takes a minute)…");
                cfs_prep(&root)
            } else if root.join("CMakeLists.txt").exists() && which("cmake") {
                say("deps: configuring CMake for compile DB…");
                cmake_configure(&root)
            } else {
                false
            };
            if made_db {
                say("deps ✓ compile DB ready — reloading clangd");
                restart_clangd.store(true, Ordering::SeqCst);
            } else if flags_path.exists() {
                say("deps ✓ include-path mode (no build DB available)");
            }
            ctx.request_repaint();
        })
        .ok();
}

/// Every directory containing at least one header in the workspace file universe, partition
/// dirs skipped. Pure over the caller-supplied list — no private re-walk.
fn header_dirs(root: &Path, files: &[PathBuf]) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for p in files {
        if p.extension().and_then(|e| e.to_str()) != Some("h") {
            continue;
        }
        let rel = p.strip_prefix(root).unwrap_or(p);
        let skipped = rel.iter().any(|comp| {
            let name = comp.to_string_lossy();
            SKIP_DIRS.iter().any(|s| name.starts_with(s))
        });
        if skipped {
            continue;
        }
        if let Some(dir) = p.parent() {
            if seen.insert(dir.to_path_buf()) {
                dirs.push(dir.to_path_buf());
            }
        }
    }
    dirs.sort();
    dirs.truncate(400); // sanity cap for pathological trees
    dirs
}

/// The cFS shape: a top Makefile wrapping cmake + the cfe submodule dir.
fn is_cfs(root: &Path) -> bool {
    root.join("cfe").is_dir() && root.join("Makefile").exists()
}

/// `make SIMULATION=native prep`, then merge every generated sub-build DB into
/// `build/compile_commands.json` (clangd discovery's first stop).
fn cfs_prep(root: &Path) -> bool {
    // CMAKE_POLICY_VERSION_MINIMUM: several cFS apps declare ancient cmake_minimum_required
    // values that CMake 4 hard-rejects; the floor makes them configure again.
    // Exit status deliberately ignored: multi-config trees (RTEMS cross-targets without a
    // toolchain) fail overall while the native config still generated its DB — merge whatever
    // exists.
    let _ = std::process::Command::new("make")
        .args(["SIMULATION=native", "prep"])
        .env("CMAKE_POLICY_VERSION_MINIMUM", "3.5")
        .current_dir(root)
        .output();
    // Merge every build*/ sub-DB → build/compile_commands.json (clangd discovery's first stop).
    // Ignore files are disabled: build dirs are always gitignored.
    let mut merged: Vec<serde_json::Value> = Vec::new();
    for top in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let name = top.file_name();
        if !name.to_string_lossy().starts_with("build") || !top.path().is_dir() {
            continue;
        }
        let mut b = ignore::WalkBuilder::new(top.path());
        b.hidden(false).git_ignore(false).git_global(false).git_exclude(false).ignore(false).parents(false);
        for entry in b.build().flatten() {
            let p = entry.path();
            if p.file_name().and_then(|n| n.to_str()) == Some("compile_commands.json") {
                if let Ok(text) = std::fs::read_to_string(p) {
                    if let Ok(serde_json::Value::Array(items)) = serde_json::from_str(&text) {
                        merged.extend(items);
                    }
                }
            }
        }
    }
    if merged.is_empty() {
        return false;
    }
    let _ = std::fs::create_dir_all(root.join("build"));
    std::fs::write(
        root.join("build/compile_commands.json"),
        serde_json::to_string(&serde_json::Value::Array(merged)).unwrap_or_default(),
    )
    .is_ok()
}

/// Generic CMake: configure-only into `.cauldron/build` with the export flag.
fn cmake_configure(root: &Path) -> bool {
    let build = root.join(".cauldron/build");
    let _ = std::fs::create_dir_all(&build);
    let ok = std::process::Command::new("cmake")
        .env("CMAKE_POLICY_VERSION_MINIMUM", "3.5")
        .arg("-S")
        .arg(root)
        .arg("-B")
        .arg(&build)
        .arg("-DCMAKE_EXPORT_COMPILE_COMMANDS=ON")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    ok && build.join("compile_commands.json").exists()
}

fn which(bin: &str) -> bool {
    std::process::Command::new(bin)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Keep generated files out of the user's `git status` without touching .gitignore.
pub fn exclude_from_git(root: &Path, entries: &[&str]) {
    let exclude = root.join(".git/info/exclude");
    let existing = std::fs::read_to_string(&exclude).unwrap_or_default();
    let mut add = String::new();
    for e in entries {
        if !existing.lines().any(|l| l.trim() == *e) {
            add.push_str(e);
            add.push('\n');
        }
    }
    if !add.is_empty() {
        // Don't glue the first new entry onto a last line that lacks a trailing newline —
        // `.git/info/exclude` written by hand (or by other tools) often ends without one, and
        // `existing + add` would corrupt that last pattern (e.g. "*.o" + "compile_flags.txt").
        let mut out = existing;
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&add);
        let _ = std::fs::write(&exclude, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// exclude_from_git must not glue a new entry onto a last line lacking a trailing newline,
    /// must skip duplicates, and must create the file when absent.
    #[test]
    fn exclude_preserves_existing_and_dedups() {
        let dir = std::env::temp_dir().join(format!("cauldron-excl-nl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let git_info = dir.join(".git/info");
        std::fs::create_dir_all(&git_info).unwrap();
        let exclude = git_info.join("exclude");

        // Existing file with NO trailing newline.
        std::fs::write(&exclude, "*.o\n*.log").unwrap();
        exclude_from_git(&dir, &["compile_flags.txt", ".cauldron/"]);
        let got = std::fs::read_to_string(&exclude).unwrap();
        assert!(got.contains("\n*.log\n"), "last line kept intact");
        assert!(got.contains("\ncompile_flags.txt\n"), "new entry on its own line");
        assert!(got.contains(".cauldron/"));

        // Re-run: no duplicates.
        exclude_from_git(&dir, &["compile_flags.txt"]);
        let got2 = std::fs::read_to_string(&exclude).unwrap();
        assert_eq!(got2.matches("compile_flags.txt").count(), 1, "no duplicate append");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn header_dirs_finds_and_skips() {
        // Pure over the caller-supplied universe — no filesystem needed.
        let root = Path::new("/proj");
        let files = vec![
            root.join("inc/api.h"),
            root.join("src/impl.c"),
            root.join("ut-stubs/fake.h"),
            root.join("build/gen/out.h"),
        ];
        let dirs = header_dirs(root, &files);
        assert!(dirs.iter().any(|d| d.ends_with("inc")));
        assert!(!dirs.iter().any(|d| d.ends_with("ut-stubs")), "stub headers excluded");
        assert!(!dirs.iter().any(|d| d.ends_with("gen")), "build-dir headers excluded");
        assert!(!dirs.iter().any(|d| d.ends_with("src")), "no headers there");
        assert_eq!(dirs.len(), 1);
    }

    #[test]
    fn git_exclude_appends_once() {
        let dir = std::env::temp_dir().join(format!("cauldron-excl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git/info")).unwrap();
        exclude_from_git(&dir, &["compile_flags.txt"]);
        exclude_from_git(&dir, &["compile_flags.txt"]);
        let text = std::fs::read_to_string(dir.join(".git/info/exclude")).unwrap();
        assert_eq!(text.matches("compile_flags.txt").count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
