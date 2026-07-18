//! Dependency auto-install, across every ecosystem a project might use.
//!
//! Open a project and its dependencies resolve themselves: `cargo fetch`, `npm install`,
//! `dotnet restore` (NuGet), `pip install` into the project's own venv, `go mod download`,
//! `bundle`, `composer`, Maven, Gradle, vcpkg, Conan, SwiftPM, Deno, Zig, Elixir, Dart/Flutter.
//! A polyglot repo (a Rust core with a TS frontend and a C# tool) resolves ALL of them.
//!
//! Three properties make this safe to do automatically:
//!
//! - **Stamped by content, not by time.** Each ecosystem's manifests + lockfiles are hashed; a step
//!   is skipped while its hash is unchanged. Reopening a project is free, and editing `Cargo.toml`
//!   (or `package.json`, or a `.csproj`) is what makes the next open re-resolve. This subsumes the
//!   old "is the manifest newer than the lock" check, which could not see an edit that left mtimes
//!   alone (a `git checkout` of a branch with different deps).
//! - **Never on the UI thread.** One background worker runs the steps in sequence — sequential and
//!   not parallel on purpose: these tools are network- and disk-bound, and three of them at once on
//!   a cold cache is slower than one at a time, as well as unreadable in the status line.
//! - **Off switch.** `npm install` runs `postinstall` scripts, i.e. arbitrary code from the tree you
//!   just opened. That is the accepted bargain of every package manager, but it should be a bargain
//!   the user can decline: [`crate::settings::Settings::auto_deps`] turns this off, and the Run menu
//!   can still trigger it by hand.
//!
//! A tool that is not installed is not an error — the step is simply skipped, so a box without
//! `dotnet` opens a C# project exactly as it did before, just without NuGet.

use std::collections::hash_map::DefaultHasher;
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// One command in a step. A step is a small SEQUENCE because some ecosystems need a prelude — a
/// python project has to have a venv before anything can be installed into it.
#[derive(Debug, Clone, PartialEq)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
}

fn cmd(program: impl Into<String>, args: &[&str]) -> Cmd {
    Cmd { program: program.into(), args: args.iter().map(|a| a.to_string()).collect() }
}

/// One ecosystem's resolution: what to run, and the files whose content decides staleness.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    /// Stable key in the stamp file. Also what the status line says.
    pub name: &'static str,
    pub cmds: Vec<Cmd>,
    /// Manifests + lockfiles. Hashed together; a change to any of them re-runs the step.
    pub inputs: Vec<PathBuf>,
}

/// Everything this project needs resolved, in the order it will run.
///
/// Detection is by marker file, and every step is gated on its tool actually being installed —
/// there is no point planning `bundle install` on a box with no ruby.
pub fn plan(root: &Path) -> Vec<Step> {
    let mut steps: Vec<Step> = Vec::new();
    let at = |p: &str| root.join(p);
    let exists = |p: &str| at(p).exists();

    // ---- Rust ---------------------------------------------------------------------------------
    if exists("Cargo.toml") && which("cargo") {
        steps.push(Step {
            name: "cargo",
            cmds: vec![cmd("cargo", &["fetch"])],
            inputs: vec![at("Cargo.toml"), at("Cargo.lock")],
        });
    }

    // ---- JavaScript / TypeScript --------------------------------------------------------------
    // The LOCKFILE is authoritative, not habit: `npm install` in a pnpm repo rewrites node_modules
    // into a flat layout pnpm forbids (and permits phantom deps). So a lockfile picks its tool and
    // ONLY its tool — if that tool is not installed, the step is SKIPPED, never silently downgraded
    // to npm (which is exactly the corruption). `bun.lock` (text, default since bun 1.2) and the
    // older binary `bun.lockb` both mean bun. Bare npm is used only when there is no foreign lock.
    if exists("package.json") {
        let node: Option<(&str, &str, &str)> = if exists("bun.lock") || exists("bun.lockb") {
            let lock = if exists("bun.lock") { "bun.lock" } else { "bun.lockb" };
            which("bun").then_some(("bun", "bun", lock))
        } else if exists("pnpm-lock.yaml") {
            which("pnpm").then_some(("pnpm", "pnpm", "pnpm-lock.yaml"))
        } else if exists("yarn.lock") {
            which("yarn").then_some(("yarn", "yarn", "yarn.lock"))
        } else if which("npm") {
            Some(("npm", "npm", "package-lock.json"))
        } else {
            None
        };
        if let Some((name, tool, lock)) = node {
            steps.push(Step {
                name: match name {
                    "bun" => "bun",
                    "pnpm" => "pnpm",
                    "yarn" => "yarn",
                    _ => "npm",
                },
                cmds: vec![cmd(tool, &["install"])],
                inputs: vec![at("package.json"), at(lock)],
            });
        }
    }

    // ---- Python -------------------------------------------------------------------------------
    // Into the project's OWN venv, always. A PEP-668 distro refuses to install into the system
    // interpreter at all, and even where it would work, a project's deps do not belong there.
    if let Some(step) = python_step(root) {
        steps.push(step);
    }

    // ---- .NET / NuGet -------------------------------------------------------------------------
    if let Some(step) = dotnet_step(root) {
        steps.push(step);
    }

    // ---- Go -----------------------------------------------------------------------------------
    if exists("go.mod") && which("go") {
        steps.push(Step {
            name: "go",
            cmds: vec![cmd("go", &["mod", "download"])],
            inputs: vec![at("go.mod"), at("go.sum")],
        });
    }

    // ---- Ruby ---------------------------------------------------------------------------------
    if exists("Gemfile") && which("bundle") {
        steps.push(Step {
            name: "bundler",
            cmds: vec![cmd("bundle", &["install"])],
            inputs: vec![at("Gemfile"), at("Gemfile.lock")],
        });
    }

    // ---- PHP ----------------------------------------------------------------------------------
    if exists("composer.json") && which("composer") {
        steps.push(Step {
            name: "composer",
            cmds: vec![cmd("composer", &["install", "--no-interaction"])],
            inputs: vec![at("composer.json"), at("composer.lock")],
        });
    }

    // ---- JVM ----------------------------------------------------------------------------------
    // `go-offline` is the closest Maven has to "just fetch what this needs".
    if exists("pom.xml") && which("mvn") {
        steps.push(Step {
            name: "maven",
            cmds: vec![cmd("mvn", &["-q", "-B", "dependency:go-offline"])],
            inputs: vec![at("pom.xml")],
        });
    }
    // The WRAPPER, when the repo ships one — a project pinned to Gradle 7 must not be resolved by
    // whatever Gradle happens to be on $PATH.
    for build in ["build.gradle", "build.gradle.kts"] {
        if exists(build) {
            let wrapper = at("gradlew");
            let gradle = if wrapper.is_file() {
                Some(wrapper.to_string_lossy().into_owned())
            } else if which("gradle") {
                Some("gradle".to_string())
            } else {
                None
            };
            if let Some(g) = gradle {
                steps.push(Step {
                    name: "gradle",
                    cmds: vec![Cmd {
                        program: g,
                        args: vec!["--quiet".into(), "dependencies".into()],
                    }],
                    inputs: vec![at(build), at("gradle.lockfile"), at("settings.gradle")],
                });
            }
            break;
        }
    }

    // ---- C / C++ package managers -------------------------------------------------------------
    if exists("vcpkg.json") && which("vcpkg") {
        steps.push(Step {
            name: "vcpkg",
            cmds: vec![cmd("vcpkg", &["install"])],
            inputs: vec![at("vcpkg.json"), at("vcpkg-configuration.json")],
        });
    }
    for conan in ["conanfile.txt", "conanfile.py"] {
        if exists(conan) && which("conan") {
            steps.push(Step {
                name: "conan",
                cmds: vec![cmd("conan", &["install", ".", "--build=missing"])],
                inputs: vec![at(conan)],
            });
            break;
        }
    }

    // ---- Swift / Deno / Zig / Elixir / Dart ---------------------------------------------------
    if exists("Package.swift") && which("swift") {
        steps.push(Step {
            name: "swiftpm",
            cmds: vec![cmd("swift", &["package", "resolve"])],
            inputs: vec![at("Package.swift"), at("Package.resolved")],
        });
    }
    for deno in ["deno.json", "deno.jsonc"] {
        if exists(deno) && which("deno") {
            steps.push(Step {
                name: "deno",
                cmds: vec![cmd("deno", &["install"])],
                inputs: vec![at(deno), at("deno.lock")],
            });
            break;
        }
    }
    if exists("build.zig.zon") && which("zig") {
        steps.push(Step {
            name: "zig",
            cmds: vec![cmd("zig", &["build", "--fetch"])],
            inputs: vec![at("build.zig.zon")],
        });
    }
    if exists("mix.exs") && which("mix") {
        steps.push(Step {
            name: "mix",
            cmds: vec![cmd("mix", &["deps.get"])],
            inputs: vec![at("mix.exs"), at("mix.lock")],
        });
    }
    if exists("pubspec.yaml") {
        // A Flutter app must resolve through flutter, not bare dart: the SDK pins its own package
        // versions and `dart pub get` would fight them.
        let flutter = std::fs::read_to_string(at("pubspec.yaml"))
            .map(|t| t.contains("flutter:") || t.contains("sdk: flutter"))
            .unwrap_or(false);
        // A Flutter app resolves ONLY through flutter — `dart pub get` fails outright on an `sdk:
        // flutter` dependency ("depends on flutter from sdk, which doesn't exist"), so a Flutter
        // project with no flutter on the box is skipped, not fruitlessly retried through dart.
        let tool = if flutter {
            which("flutter").then_some("flutter")
        } else if which("dart") {
            Some("dart")
        } else {
            None
        };
        if let Some(t) = tool {
            steps.push(Step {
                name: if t == "flutter" { "flutter" } else { "dart" },
                cmds: vec![cmd(t, &["pub", "get"])],
                inputs: vec![at("pubspec.yaml"), at("pubspec.lock")],
            });
        }
    }

    steps
}

/// Python: make sure the venv exists, then install whatever the project declares.
///
/// Nothing is installed for a project that declares no dependencies — a bare `pip install -e .` on
/// the New Project template would reach out to the network to build a package with no deps, which
/// is a slow way to achieve nothing.
fn python_step(root: &Path) -> Option<Step> {
    let reqs = root.join("requirements.txt");
    let pyproject = root.join("pyproject.toml");
    let declares_deps = std::fs::read_to_string(&pyproject)
        .map(|t| pyproject_has_deps(&t))
        .unwrap_or(false);
    if !reqs.is_file() && !declares_deps {
        return None;
    }
    if !which("python3") {
        return None;
    }

    let mut cmds = Vec::new();
    let venv_py = root.join(".venv/bin/python");
    if !venv_py.is_file() {
        cmds.push(cmd("python3", &["-m", "venv", ".venv"]));
    }
    // Through `-m pip`, by absolute interpreter path: a spawned process never has the venv
    // "activated", so `pip` on $PATH would be the SYSTEM pip and the install would land outside
    // the project (or be refused outright by PEP 668).
    let py = venv_py.to_string_lossy().into_owned();
    if reqs.is_file() {
        cmds.push(Cmd {
            program: py.clone(),
            args: vec!["-m".into(), "pip".into(), "install".into(), "-r".into(), "requirements.txt".into()],
        });
    } else {
        // Editable: the project's own package, importable without reinstalling on every edit.
        cmds.push(Cmd {
            program: py,
            args: vec!["-m".into(), "pip".into(), "install".into(), "-e".into(), ".".into()],
        });
    }
    Some(Step { name: "pip", cmds, inputs: vec![reqs, pyproject, root.join("poetry.lock")] })
}

/// Does this pyproject declare any runtime dependencies? Deliberately shallow: `dependencies` under
/// `[project]` (PEP 621) or a `[tool.poetry.dependencies]` table with more than the python pin.
fn pyproject_has_deps(text: &str) -> bool {
    // `dependencies = []` is an explicit "none" and must not trigger an install. A non-empty array
    // contains at least one string — and TOML strings are single- OR double-quoted, so both count
    // (`dependencies = ['requests']` is as valid as `["requests"]`).
    if let Some(rest) = text.split("dependencies").nth(1) {
        let head: String = rest.chars().take_while(|c| *c != ']').collect();
        if head.trim_start().starts_with('=') && (head.contains('"') || head.contains('\'')) {
            return true;
        }
    }
    text.contains("[tool.poetry.dependencies]")
}

/// .NET: restore NuGet packages for the solution, or for the project when there is no solution.
///
/// `dotnet restore` is the modern front-end to NuGet and is preferred; the standalone `nuget`
/// binary is the fallback for a box that has it without the SDK (and for old `packages.config`
/// trees, which `dotnet restore` does not handle).
fn dotnet_step(root: &Path) -> Option<Step> {
    let target = dotnet_target(root)?;
    let rel = target.strip_prefix(root).unwrap_or(&target).to_string_lossy().into_owned();
    if which("dotnet") {
        return Some(Step {
            name: "nuget",
            cmds: vec![Cmd {
                program: "dotnet".into(),
                args: vec!["restore".into(), rel],
            }],
            inputs: dotnet_inputs(root, &target),
        });
    }
    if which("nuget") {
        return Some(Step {
            name: "nuget",
            cmds: vec![Cmd { program: "nuget".into(), args: vec!["restore".into(), rel] }],
            inputs: dotnet_inputs(root, &target),
        });
    }
    None
}

/// The thing to restore: a solution if there is one (it pulls in every project), else the first
/// project file. Searched to a depth of two directories — the overwhelmingly common layouts are
/// `App.sln` at the root, or `src/App/App.csproj` two levels under it.
const DOTNET_MAX_DEPTH: usize = 2;
fn dotnet_target(root: &Path) -> Option<PathBuf> {
    let is = |p: &Path, ext: &str| p.extension().is_some_and(|e| e.eq_ignore_ascii_case(ext));
    let proj_ext = ["csproj", "fsproj", "vbproj"];

    let mut projects: Vec<PathBuf> = Vec::new();
    let mut dirs: Vec<PathBuf> = vec![root.to_path_buf()];
    // Breadth-first to DOTNET_MAX_DEPTH, entries sorted so the pick is deterministic rather than
    // readdir-order-dependent. Subdirs are enqueued at every level BUT the last (`depth <` guard),
    // which is what previously stopped one level short of `src/App/App.csproj`.
    for depth in 0..=DOTNET_MAX_DEPTH {
        let mut next: Vec<PathBuf> = Vec::new();
        for dir in &dirs {
            let mut entries: Vec<PathBuf> =
                std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).collect();
            entries.sort();
            for p in entries {
                if p.is_dir() {
                    let skip = p
                        .file_name()
                        .map(|n| {
                            let n = n.to_string_lossy();
                            n.starts_with('.') || n == "bin" || n == "obj" || n == "node_modules"
                        })
                        .unwrap_or(true);
                    if !skip && depth < DOTNET_MAX_DEPTH {
                        next.push(p);
                    }
                } else if is(&p, "sln") {
                    return Some(p); // a solution wins outright
                } else if proj_ext.iter().any(|e| is(&p, e)) {
                    projects.push(p);
                }
            }
        }
        dirs = next;
    }
    projects.sort();
    projects.into_iter().next()
}

fn dotnet_inputs(root: &Path, target: &Path) -> Vec<PathBuf> {
    vec![
        target.to_path_buf(),
        root.join("packages.lock.json"),
        root.join("Directory.Packages.props"),
        root.join("nuget.config"),
    ]
}

// ---------------------------------------------------------------------------------------------
// Stamping
// ---------------------------------------------------------------------------------------------

/// `<root>/.cauldron/deps-stamp.json` — ecosystem name → hash of its inputs at the last SUCCESSFUL
/// resolve. Lives in the project (not a global dir) so deleting the project takes its stamp with it,
/// and so a copy of the tree does not inherit a stamp describing someone else's `node_modules`.
fn stamp_path(root: &Path) -> PathBuf {
    root.join(".cauldron/deps-stamp.json")
}

fn load_stamp(root: &Path) -> BTreeMap<String, String> {
    std::fs::read_to_string(stamp_path(root))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Serializes the stamp's read-merge-write. Two workers for one root are already rare (the
/// generation guard makes the superseded one abandon before it saves), but a project can also be
/// open in two cauldron INSTANCES — so the write is atomic (tmp+rename, never a torn file) and
/// MERGES: the on-disk entries this worker did not touch are preserved rather than clobbered, so
/// the loser of a race costs at most a re-resolve, never another ecosystem's stamp.
static STAMP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn save_stamp(root: &Path, stamp: &BTreeMap<String, String>) {
    let _guard = STAMP_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let path = stamp_path(root);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Re-read under the lock and layer this worker's entries on top, so a concurrent worker's
    // just-written entries for OTHER ecosystems survive.
    let mut merged = load_stamp(root);
    for (k, v) in stamp {
        merged.insert(k.clone(), v.clone());
    }
    if let Ok(json) = serde_json::to_string_pretty(&merged) {
        let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
    crate::deps::exclude_from_git(root, &[".cauldron/"]);
}

/// Content hash of a step's inputs. A MISSING file hashes distinctly from an empty one, so deleting
/// `Cargo.lock` (or a first `npm install` creating `package-lock.json`) is itself a change.
pub fn input_hash(inputs: &[PathBuf]) -> String {
    let mut h = DefaultHasher::new();
    for p in inputs {
        p.file_name().unwrap_or_default().hash(&mut h);
        match std::fs::read(p) {
            Ok(bytes) => {
                1u8.hash(&mut h);
                bytes.hash(&mut h);
            }
            Err(_) => 0u8.hash(&mut h),
        }
    }
    format!("{:016x}", h.finish())
}

/// Which steps are actually stale (what a run would DO). Also what the tests assert against.
pub fn stale_steps(root: &Path, force: bool) -> Vec<Step> {
    let stamp = load_stamp(root);
    plan(root)
        .into_iter()
        .filter(|s| force || stamp.get(s.name) != Some(&input_hash(&s.inputs)))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The worker
// ---------------------------------------------------------------------------------------------

/// Resolve every stale ecosystem in the background.
///
/// A per-root lock so at most ONE install runs in a given tree at a time. The generation guard
/// only silences a superseded worker's STATUS writes — it cannot interrupt an in-flight `npm
/// install` subprocess — so without this a manual re-install (or a re-open) racing the auto one
/// would run two package managers in the same directory and corrupt node_modules / the lockfile.
/// A worker for the SAME root waits behind the running one, then re-evaluates staleness; workers
/// for DIFFERENT roots use different locks and still run concurrently.
fn root_lock(root: &Path) -> Arc<Mutex<()>> {
    static LOCKS: Mutex<BTreeMap<PathBuf, Arc<Mutex<()>>>> = Mutex::new(BTreeMap::new());
    let mut map = LOCKS.lock().unwrap_or_else(|p| p.into_inner());
    map.entry(root.to_path_buf()).or_insert_with(|| Arc::new(Mutex::new(()))).clone()
}

/// `generation` guards the status line against a project switch: a resolve for the project you just
/// LEFT must not keep writing "deps: npm install…" over the new project's status. The worker
/// compares the generation it captured against the live one before every write, and abandons the
/// run entirely once they differ — the packages it already installed are still installed, they just
/// stop being announced.
pub fn install(
    root: PathBuf,
    force: bool,
    generation: Arc<AtomicU64>,
    status: Arc<Mutex<Option<String>>>,
    notify: impl Fn() + Send + 'static,
) {
    let mine = generation.load(Ordering::SeqCst);
    std::thread::Builder::new()
        .name("cauldron-deps-install".into())
        .spawn(move || {
            // Serialize with any other worker in THIS tree. Held for the whole run; a same-root
            // worker blocks here until we finish (it then re-reads the stamp and usually no-ops).
            let lock = root_lock(&root);
            let _run_guard = lock.lock().unwrap_or_else(|p| p.into_inner());
            let current = || generation.load(Ordering::SeqCst) == mine;
            let say = |s: Option<String>| {
                if current() {
                    *status.lock().unwrap_or_else(|p| p.into_inner()) = s;
                    notify();
                }
            };

            let steps = stale_steps(&root, force);
            if steps.is_empty() {
                crate::boot_trace::mark("deps: nothing stale");
                return;
            }

            let mut stamp = load_stamp(&root);
            let mut failed: Vec<&str> = Vec::new();
            let total = steps.len();
            for (i, step) in steps.iter().enumerate() {
                if !current() {
                    return; // the project changed under us
                }
                say(Some(match total {
                    1 => format!("deps: {}…", step.name),
                    _ => format!("deps: {} ({}/{})…", step.name, i + 1, total),
                }));

                let mut ok = true;
                for c in &step.cmds {
                    match std::process::Command::new(&c.program)
                        .args(&c.args)
                        .current_dir(&root)
                        .output()
                    {
                        Ok(o) if o.status.success() => {}
                        Ok(o) => {
                            // The tail is what matters: package managers print a wall of progress
                            // and then the actual reason on the last few lines.
                            let err = String::from_utf8_lossy(&o.stderr);
                            let tail: Vec<&str> = err.lines().rev().take(5).collect();
                            log::warn!(
                                "deps: {} failed ({} {}): {}",
                                step.name,
                                c.program,
                                c.args.join(" "),
                                tail.into_iter().rev().collect::<Vec<_>>().join(" | ")
                            );
                            ok = false;
                            break;
                        }
                        Err(e) => {
                            log::warn!("deps: {} could not start {}: {e}", step.name, c.program);
                            ok = false;
                            break;
                        }
                    }
                }
                if ok {
                    // Stamped only on SUCCESS: a failed resolve must be retried on the next open,
                    // not marked done and silently skipped forever.
                    stamp.insert(step.name.to_string(), input_hash(&step.inputs));
                } else {
                    failed.push(step.name);
                }
            }

            if !current() {
                return;
            }
            save_stamp(&root, &stamp);
            say(Some(if failed.is_empty() {
                "deps ✓".to_string()
            } else {
                format!("deps: {} failed (see log)", failed.join(", "))
            }));
        })
        .ok();
}

/// Is `bin` on $PATH? `--version` is the one flag essentially every one of these tools accepts.
/// A tool that exists but errors on it still counts as present (`vcpkg version` differs) — what is
/// being tested is "could we spawn it at all".
fn which(bin: &str) -> bool {
    match std::process::Command::new(bin).arg("--version").output() {
        Ok(_) => true,
        Err(e) => e.kind() != std::io::ErrorKind::NotFound,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cauldron-deps-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn names(steps: &[Step]) -> Vec<&str> {
        steps.iter().map(|s| s.name).collect()
    }

    /// The lockfile — not habit — decides the package manager. Running `npm install` in a pnpm repo
    /// rewrites node_modules into a layout pnpm did not ask for.
    #[test]
    fn node_package_manager_comes_from_the_lockfile() {
        let root = tmp("node");
        std::fs::write(root.join("package.json"), "{}").unwrap();
        // npm is the fallback when no other lock is present.
        assert_eq!(names(&plan(&root)), vec!["npm"]);

        for (lock, want) in [("yarn.lock", "yarn"), ("pnpm-lock.yaml", "pnpm"), ("bun.lockb", "bun")]
        {
            std::fs::write(root.join(lock), "").unwrap();
            // Only assert the ones actually installed on this box; a missing tool falls back.
            if which(want) {
                assert_eq!(names(&plan(&root)), vec![want], "{lock} must select {want}");
            }
            std::fs::remove_file(root.join(lock)).unwrap();
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A polyglot repo resolves EVERY ecosystem it contains, not just the first one recognized.
    #[test]
    fn polyglot_repo_plans_every_ecosystem() {
        let root = tmp("poly");
        std::fs::write(root.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        std::fs::write(root.join("package.json"), "{}").unwrap();
        std::fs::write(root.join("go.mod"), "module x\n").unwrap();
        std::fs::write(root.join("requirements.txt"), "requests\n").unwrap();
        let planned = plan(&root);
        let got = names(&planned);
        for want in ["cargo", "npm", "pip", "go"] {
            if which(match want {
                "cargo" => "cargo",
                "npm" => "npm",
                "pip" => "python3",
                _ => "go",
            }) {
                assert!(got.contains(&want), "{want} must be planned: {got:?}");
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// NuGet: a solution wins over a project, and the project is found one level down.
    #[test]
    fn dotnet_target_prefers_the_solution() {
        let root = tmp("dotnet");
        std::fs::create_dir_all(root.join("src/App")).unwrap();
        std::fs::write(root.join("src/App/App.csproj"), "<Project/>").unwrap();
        assert_eq!(dotnet_target(&root), Some(root.join("src/App/App.csproj")), "nested project");

        std::fs::write(root.join("App.sln"), "").unwrap();
        assert_eq!(dotnet_target(&root), Some(root.join("App.sln")), "the solution wins");

        // bin/obj are build output — a .csproj copied in there is not the project.
        let other = tmp("dotnet2");
        std::fs::create_dir_all(other.join("obj")).unwrap();
        std::fs::write(other.join("obj/Ghost.csproj"), "<Project/>").unwrap();
        assert_eq!(dotnet_target(&other), None, "obj/ must be skipped");

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&other);
    }

    /// Python installs into the project's OWN venv, creating it first when absent — and does
    /// nothing at all for a project that declares no dependencies.
    #[test]
    fn python_step_targets_the_venv_and_skips_when_there_is_nothing_to_do() {
        if !which("python3") {
            return;
        }
        let root = tmp("py");

        // A pyproject with an explicitly EMPTY dependency list is not a reason to install.
        std::fs::write(root.join("pyproject.toml"), "[project]\nname=\"x\"\ndependencies = []\n")
            .unwrap();
        assert!(python_step(&root).is_none(), "no deps declared -> no install");

        // Real deps: create the venv, then install into it.
        std::fs::write(root.join("requirements.txt"), "requests\n").unwrap();
        let step = python_step(&root).expect("a step");
        assert_eq!(step.cmds[0], cmd("python3", &["-m", "venv", ".venv"]), "venv first");
        assert_eq!(step.cmds[1].program, root.join(".venv/bin/python").to_string_lossy());
        assert_eq!(step.cmds[1].args, vec!["-m", "pip", "install", "-r", "requirements.txt"]);

        // Once the venv exists, it is not recreated.
        std::fs::create_dir_all(root.join(".venv/bin")).unwrap();
        std::fs::write(root.join(".venv/bin/python"), "#!/bin/sh\n").unwrap();
        let step = python_step(&root).unwrap();
        assert_eq!(step.cmds.len(), 1, "existing venv is reused: {:?}", step.cmds);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn pyproject_dependency_sniffing() {
        assert!(!pyproject_has_deps("[project]\nname=\"x\"\ndependencies = []\n"));
        assert!(pyproject_has_deps("[project]\ndependencies = [\"requests\"]\n"));
        // TOML literal (single-quoted) strings are valid and declare real deps.
        assert!(pyproject_has_deps("[project]\ndependencies = ['requests', 'flask']\n"));
        assert!(pyproject_has_deps("[tool.poetry.dependencies]\nrequests = \"*\"\n"));
        assert!(!pyproject_has_deps("[project]\nname = \"x\"\n"));
    }

    /// A lockfile picks its tool and ONLY its tool. If the tool is missing, the node step is
    /// SKIPPED — never downgraded to `npm install`, which would rewrite the tree into a layout the
    /// real package manager forbids. Bare npm is used only when there is no foreign lock.
    #[test]
    fn a_foreign_lockfile_without_its_tool_is_skipped_not_downgraded_to_npm() {
        let root = tmp("nodeskip");
        std::fs::write(root.join("package.json"), "{}").unwrap();

        // A pnpm repo on a box without pnpm: NO node step (not npm).
        std::fs::write(root.join("pnpm-lock.yaml"), "").unwrap();
        if !which("pnpm") {
            assert!(
                !names(&plan(&root)).contains(&"npm"),
                "a pnpm lock must never be resolved by npm"
            );
            assert!(names(&plan(&root)).iter().all(|n| *n != "npm"));
        }
        std::fs::remove_file(root.join("pnpm-lock.yaml")).unwrap();

        // bun's TEXT lockfile (default since bun 1.2) is recognized as bun, not npm.
        std::fs::write(root.join("bun.lock"), "").unwrap();
        if which("bun") {
            assert_eq!(names(&plan(&root)), vec!["bun"], "bun.lock means bun");
        } else {
            assert!(!names(&plan(&root)).contains(&"npm"), "no bun -> skip, not npm");
        }
        std::fs::remove_file(root.join("bun.lock")).unwrap();

        // No foreign lock at all: bare npm is the right default.
        if which("npm") {
            assert_eq!(names(&plan(&root)), vec!["npm"]);
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A Flutter app resolves only through flutter; with no flutter installed the step is skipped,
    /// never routed to `dart pub get` (which fails on an `sdk: flutter` dependency).
    #[test]
    fn flutter_without_flutter_is_skipped_not_routed_to_dart() {
        let root = tmp("flutter");
        std::fs::write(
            root.join("pubspec.yaml"),
            "name: app\ndependencies:\n  flutter:\n    sdk: flutter\n",
        )
        .unwrap();
        let planned = plan(&root);
        if !which("flutter") {
            assert!(
                !names(&planned).contains(&"dart"),
                "a Flutter app must never be resolved by plain dart: {:?}",
                names(&planned)
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The stamp is what makes auto-install free to leave on: unchanged manifests => no work.
    /// A CONTENT change re-runs the step even if mtimes would say otherwise (a `git checkout` of a
    /// branch with different deps restores old content with a NEW mtime — and vice versa).
    #[test]
    fn stamp_skips_unchanged_and_reruns_on_content_change() {
        let root = tmp("stamp");
        std::fs::write(root.join("go.mod"), "module x\n").unwrap();
        if !which("go") {
            return;
        }
        assert_eq!(names(&stale_steps(&root, false)), vec!["go"], "never resolved -> stale");

        // Pretend it succeeded.
        let mut stamp = BTreeMap::new();
        stamp.insert("go".to_string(), input_hash(&plan(&root)[0].inputs));
        save_stamp(&root, &stamp);
        assert!(stale_steps(&root, false).is_empty(), "unchanged -> skipped");

        // force ignores the stamp (the manual "Install dependencies" action).
        assert_eq!(names(&stale_steps(&root, true)), vec!["go"]);

        // Editing the manifest makes it stale again.
        std::fs::write(root.join("go.mod"), "module x\nrequire y v1.0.0\n").unwrap();
        assert_eq!(names(&stale_steps(&root, false)), vec!["go"], "content changed -> stale");

        // So does a lockfile APPEARING (an input that was missing is not the same as unchanged).
        std::fs::write(root.join("go.mod"), "module x\n").unwrap();
        assert!(stale_steps(&root, false).is_empty(), "back to the stamped content");
        std::fs::write(root.join("go.sum"), "y v1.0.0 h1:...\n").unwrap();
        assert_eq!(names(&stale_steps(&root, false)), vec!["go"], "new lockfile -> stale");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A failed step must NOT be stamped, or a transient network failure would mark the project
    /// resolved forever and it would never be retried.
    #[test]
    fn a_failing_step_is_never_stamped() {
        let root = tmp("fail");
        // A go.mod that `go mod download` will reject.
        std::fs::write(root.join("go.mod"), "this is not a go.mod\n").unwrap();
        if !which("go") {
            return;
        }
        let gen = Arc::new(AtomicU64::new(0));
        let status = Arc::new(Mutex::new(None));
        let ctx = egui::Context::default();
        install(root.clone(), false, Arc::clone(&gen), Arc::clone(&status), || {});

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            let msg = status.lock().unwrap_or_else(|p| p.into_inner()).clone();
            if msg.as_deref().is_some_and(|m| m.contains("failed")) {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "timed out; status = {msg:?}");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(load_stamp(&root).get("go").is_none(), "a failure must stay stale");
        assert_eq!(names(&stale_steps(&root, false)), vec!["go"], "and be retried next time");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Concurrent stamp saves for the SAME root (two cauldron instances, or a manual re-install
    /// racing the auto one) must not lose each other's ecosystem entries — the read-merge-rename
    /// under the lock is what guarantees it.
    #[test]
    fn concurrent_stamp_saves_lose_no_entries() {
        let root = tmp("stamp-race");
        std::fs::create_dir_all(root.join(".cauldron")).unwrap();
        let handles: Vec<_> = ["cargo", "npm", "pip", "go", "nuget", "maven", "deno", "zig"]
            .into_iter()
            .map(|eco| {
                let root = root.clone();
                std::thread::spawn(move || {
                    let mut m = BTreeMap::new();
                    m.insert(eco.to_string(), format!("hash-{eco}"));
                    save_stamp(&root, &m);
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let final_stamp = load_stamp(&root);
        for eco in ["cargo", "npm", "pip", "go", "nuget", "maven", "deno", "zig"] {
            assert_eq!(
                final_stamp.get(eco).map(String::as_str),
                Some(format!("hash-{eco}").as_str()),
                "{eco}'s entry was lost in the race; stamp = {final_stamp:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// End to end, for real: a project with a venv-less python tree and an empty requirements file
    /// gets a working venv and a successful install — no network needed.
    #[test]
    fn install_creates_the_venv_for_real() {
        if !which("python3") {
            return;
        }
        let root = tmp("e2e");
        std::fs::write(root.join("requirements.txt"), "").unwrap();
        let gen = Arc::new(AtomicU64::new(0));
        let status = Arc::new(Mutex::new(None));
        install(root.clone(), false, Arc::clone(&gen), Arc::clone(&status), || {});

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            if status.lock().unwrap_or_else(|p| p.into_inner()).as_deref() == Some("deps ✓") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out; status = {:?}",
                status.lock().unwrap_or_else(|p| p.into_inner())
            );
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(root.join(".venv/bin/python").is_file(), "the venv was actually created");
        assert!(load_stamp(&root).contains_key("pip"), "success is stamped");
        assert!(stale_steps(&root, false).is_empty(), "and a second open does nothing");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A project switch mid-install must not let the old project's worker keep writing the status
    /// line of the new one.
    #[test]
    fn a_stale_generation_stops_reporting() {
        let root = tmp("gen");
        std::fs::write(root.join("requirements.txt"), "").unwrap();
        if !which("python3") {
            return;
        }
        let gen = Arc::new(AtomicU64::new(7));
        let status = Arc::new(Mutex::new(None));
        install(root.clone(), false, Arc::clone(&gen), Arc::clone(&status), || {});
        // The user opens another project immediately.
        gen.store(8, Ordering::SeqCst);

        std::thread::sleep(std::time::Duration::from_millis(400));
        let msg = status.lock().unwrap_or_else(|p| p.into_inner()).clone();
        assert!(
            msg.is_none() || !msg.as_deref().unwrap_or("").contains("deps ✓"),
            "a superseded worker must not announce completion: {msg:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
