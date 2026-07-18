//! Workspace discovery: clangd compile-DB resolution + rust-analyzer root selection.
//!
//! Pure filesystem walks — no process spawning, no server knowledge — so everything here is
//! unit-testable against a throwaway fixture tree. The manager feeds the results straight into
//! spawn args (`--compile-commands-dir=<dir>`) and server cwd/rootUri choices.

use std::fs;
use std::path::{Path, PathBuf};

/// Resolve the directory clangd should read `compile_commands.json` from
/// (the `--compile-commands-dir` value). clangd only searches ANCESTOR dirs of a source file on
/// its own, never `build/` siblings, so Cauldron walks the conventional spots itself:
/// explicit override → `<root>/build/` → `<root>/.cauldron/build/` → any direct subdir of
/// `<root>/build/` → `<root>/cmake-build-*/`. First directory CONTAINING the DB wins; an
/// explicit override without one falls through to the chain.
pub fn clangd_compile_db(root: &Path, explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(dir) = explicit {
        if has_db(dir) {
            return Some(dir.to_path_buf());
        }
    }
    for candidate in [root.join("build"), root.join(".cauldron").join("build")] {
        if has_db(&candidate) {
            return Some(candidate);
        }
    }
    // build/*/ — multi-config trees (e.g. build/debug, build/release).
    if let Some(found) = first_subdir_with_db(&root.join("build")) {
        return Some(found);
    }
    // cmake-build-*/ — CLion-style out-of-source dirs directly under the root.
    let mut cmake_dirs: Vec<PathBuf> = fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("cmake-build-"))
        })
        .collect();
    cmake_dirs.sort();
    cmake_dirs.into_iter().find(|d| has_db(d))
}

/// Whether clangd is usable at `root` WITHOUT a compile DB: a `.clangd` config or a
/// `compile_flags.txt` gives it flags to work with. When both this and
/// [`clangd_compile_db`] come up empty the caller emits `LspEvent::Degraded` — clangd still
/// runs, but diagnostics on anything nontrivial are noise.
pub fn clangd_fallback(root: &Path) -> bool {
    root.join(".clangd").exists() || root.join("compile_flags.txt").exists()
}

/// Pick the workspace root rust-analyzer should be spawned in for `file`: the nearest ancestor
/// containing `Cargo.toml`, but PREFERRING the outermost ancestor whose manifest declares
/// `[workspace]` — a nested crate inside a workspace repo must share the workspace-root server,
/// not spawn its own (r-a runs `cargo metadata` itself from there).
pub fn rust_analyzer_root(file: &Path) -> Option<PathBuf> {
    // Nearest → outermost, collected in one upward walk from the file's directory.
    let candidates: Vec<PathBuf> = file
        .parent()?
        .ancestors()
        .filter(|dir| dir.join("Cargo.toml").is_file())
        .map(Path::to_path_buf)
        .collect();
    let workspace = candidates.iter().rev().find(|dir| {
        // Substring check is deliberate — a full TOML parse buys nothing here.
        fs::read_to_string(dir.join("Cargo.toml")).is_ok_and(|s| s.contains("[workspace]"))
    });
    workspace.cloned().or_else(|| candidates.first().cloned())
}

fn has_db(dir: &Path) -> bool {
    dir.join("compile_commands.json").is_file()
}

/// First direct subdirectory of `dir` (sorted, for determinism) containing the compile DB.
fn first_subdir_with_db(dir: &Path) -> Option<PathBuf> {
    let mut subdirs: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    subdirs.sort();
    subdirs.into_iter().find(|d| has_db(d))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Throwaway fixture tree under the system temp dir, removed on drop. Named with the pid +
    /// a per-test tag so parallel test runs never collide.
    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("cauldron-lsp-discovery-{}-{tag}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).expect("create fixture root");
            Self { root }
        }

        fn file(&self, rel: &str, contents: &str) -> PathBuf {
            let path = self.root.join(rel);
            fs::create_dir_all(path.parent().expect("fixture files have parents"))
                .expect("create fixture dirs");
            fs::write(&path, contents).expect("write fixture file");
            path
        }

        fn dir(&self, rel: &str) -> PathBuf {
            let path = self.root.join(rel);
            fs::create_dir_all(&path).expect("create fixture dir");
            path
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.root.join(rel)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn compile_db_in_build() {
        let fx = Fixture::new("db-build");
        fx.file("build/compile_commands.json", "[]");
        assert_eq!(clangd_compile_db(&fx.root, None), Some(fx.path("build")));
    }

    #[test]
    fn compile_db_in_cauldron_build() {
        let fx = Fixture::new("db-cauldron");
        fx.file(".cauldron/build/compile_commands.json", "[]");
        assert_eq!(clangd_compile_db(&fx.root, None), Some(fx.path(".cauldron/build")));
    }

    #[test]
    fn compile_db_in_build_subdir() {
        let fx = Fixture::new("db-build-subdir");
        fx.dir("build/artifacts"); // decoy without a DB, sorts first
        fx.file("build/debug/compile_commands.json", "[]");
        assert_eq!(clangd_compile_db(&fx.root, None), Some(fx.path("build/debug")));
    }

    #[test]
    fn compile_db_in_cmake_build_dir() {
        let fx = Fixture::new("db-cmake");
        fx.file("cmake-build-debug/compile_commands.json", "[]");
        assert_eq!(clangd_compile_db(&fx.root, None), Some(fx.path("cmake-build-debug")));
    }

    #[test]
    fn compile_db_explicit_override_wins() {
        let fx = Fixture::new("db-explicit");
        fx.file("elsewhere/compile_commands.json", "[]");
        fx.file("build/compile_commands.json", "[]");
        assert_eq!(
            clangd_compile_db(&fx.root, Some(&fx.path("elsewhere"))),
            Some(fx.path("elsewhere"))
        );
    }

    #[test]
    fn compile_db_explicit_without_db_falls_through() {
        let fx = Fixture::new("db-explicit-empty");
        fx.file("build/compile_commands.json", "[]");
        let empty = fx.dir("empty");
        assert_eq!(clangd_compile_db(&fx.root, Some(&empty)), Some(fx.path("build")));
    }

    #[test]
    fn compile_db_none_found() {
        let fx = Fixture::new("db-none");
        fx.file("src/main.c", "int main(void) { return 0; }\n");
        assert_eq!(clangd_compile_db(&fx.root, None), None);
    }

    #[test]
    fn fallback_dot_clangd() {
        let fx = Fixture::new("fb-clangd");
        assert!(!clangd_fallback(&fx.root));
        fx.file(".clangd", "CompileFlags:\n  Add: [-std=c11]\n");
        assert!(clangd_fallback(&fx.root));
    }

    #[test]
    fn fallback_compile_flags_txt() {
        let fx = Fixture::new("fb-flags");
        assert!(!clangd_fallback(&fx.root));
        fx.file("compile_flags.txt", "-Wall\n");
        assert!(clangd_fallback(&fx.root));
    }

    #[test]
    fn rust_root_nested_crate_picks_workspace_root() {
        let fx = Fixture::new("ra-workspace");
        fx.file("Cargo.toml", "[workspace]\nmembers = [\"crates/inner\"]\n");
        fx.file("crates/inner/Cargo.toml", "[package]\nname = \"inner\"\n");
        let file = fx.file("crates/inner/src/lib.rs", "");
        assert_eq!(rust_analyzer_root(&file), Some(fx.root.clone()));
    }

    #[test]
    fn rust_root_standalone_crate_picks_itself() {
        let fx = Fixture::new("ra-standalone");
        fx.file("Cargo.toml", "[package]\nname = \"solo\"\n");
        let file = fx.file("src/main.rs", "fn main() {}\n");
        assert_eq!(rust_analyzer_root(&file), Some(fx.root.clone()));
    }
}
