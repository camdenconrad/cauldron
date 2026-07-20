//! Run/Debug configurations — RustRover-style named launch configs.
//!
//! Persisted per-workspace at `<root>/.cauldron/runconfigs.json` (gitignored via
//! `.git/info/exclude`, same pattern as deps.rs). `detect()` scans the tree for
//! sensible suggestions (cargo, pytest, uvicorn, make, built ELF binaries) and
//! `ensure_detected()` merges them into a store without duplicating.


use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};

use crate::style;

// ---------------------------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunKind {
    Cargo,
    CargoTest,
    Binary,
    Python,
    Pytest,
    Uvicorn,
    /// `dotnet run` for a .NET project (the owning .csproj / directory).
    Dotnet,
    /// `dotnet watch run` — .NET Hot Reload: rebuilds/patches the running app on save.
    DotnetWatch,
    /// `dotnet test` for a .NET test project or solution.
    DotnetTest,
    Make,
    /// Run ONE source file — `program` is its absolute path, and the command is derived from the
    /// extension (compiling first where the language needs it). What Shift+Ctrl+F10 builds.
    File,
    Custom,
}

impl RunKind {
    pub fn label(&self) -> &'static str {
        match self {
            RunKind::Cargo => "cargo run",
            RunKind::CargoTest => "cargo test",
            RunKind::Binary => "binary",
            RunKind::Python => "python",
            RunKind::Pytest => "pytest",
            RunKind::Uvicorn => "uvicorn",
            RunKind::Dotnet => "dotnet run",
            RunKind::DotnetWatch => "dotnet watch",
            RunKind::DotnetTest => "dotnet test",
            RunKind::Make => "make",
            RunKind::File => "file",
            RunKind::Custom => "custom",
        }
    }

    const ALL: [RunKind; 12] = [
        RunKind::Cargo,
        RunKind::CargoTest,
        RunKind::Binary,
        RunKind::Python,
        RunKind::Pytest,
        RunKind::Uvicorn,
        RunKind::Dotnet,
        RunKind::DotnetWatch,
        RunKind::DotnetTest,
        RunKind::Make,
        RunKind::File,
        RunKind::Custom,
    ];
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunConfig {
    pub name: String,
    pub kind: RunKind,
    /// Meaning depends on kind: Binary → path to the executable; Uvicorn → "module:app";
    /// Python → script path; File → the source file to run; Make → the target (empty = default
    /// goal); Custom → the program to exec. Ignored for Cargo/CargoTest/Pytest.
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
}

impl RunConfig {
    fn new(name: impl Into<String>, kind: RunKind, program: impl Into<String>) -> Self {
        Self { name: name.into(), kind, program: program.into(), args: Vec::new(), cwd: None, env: Vec::new() }
    }

    /// Identity used for detect-merge dedup: kind + program.
    fn dedup_key(&self) -> (String, String) {
        (format!("{:?}", self.kind), self.program.clone())
    }
}

// ---------------------------------------------------------------------------------------------
// Store + persistence
// ---------------------------------------------------------------------------------------------

pub struct RunConfigStore {
    pub configs: Vec<RunConfig>,
    pub selected: usize,
    // ---- edit-window UI state (not persisted) ----
    edit_open: bool,
    edit_env_draft: String, // "KEY=VAL" being typed
    edit_arg_draft: String,
    // ---- background detection (boot-wave item 2; house pattern: symbols.rs/psi.rs) ----
    /// Stream stamp: results from an older `kick_detect` on THIS store are dropped by
    /// `poll_detect`. Project switches build a whole new store (fresh channel), so a stale
    /// worker's send simply fails — this guards re-kicks on the same store.
    detect_gen: u64,
    detect_tx: Sender<(u64, Vec<RunConfig>)>,
    detect_rx: Receiver<(u64, Vec<RunConfig>)>,
}

impl Default for RunConfigStore {
    fn default() -> Self {
        let (detect_tx, detect_rx) = mpsc::channel();
        Self {
            configs: Vec::new(),
            selected: 0,
            edit_open: false,
            edit_env_draft: String::new(),
            edit_arg_draft: String::new(),
            detect_gen: 0,
            detect_tx,
            detect_rx,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct StoreOnDisk {
    configs: Vec<RunConfig>,
    selected: usize,
}

fn store_path(root: &Path) -> PathBuf {
    root.join(".cauldron/runconfigs.json")
}

impl RunConfigStore {
    pub fn load(root: &Path) -> Self {
        let mut s = Self::default();
        if let Ok(text) = std::fs::read_to_string(store_path(root)) {
            if let Ok(disk) = serde_json::from_str::<StoreOnDisk>(&text) {
                s.configs = disk.configs;
                s.selected = disk.selected.min(s.configs.len().saturating_sub(1));
            }
        }
        s
    }

    pub fn save(&self, root: &Path) {
        let dir = root.join(".cauldron");
        let _ = std::fs::create_dir_all(&dir);
        exclude_from_git(root, &[".cauldron/"]);
        let disk = StoreOnDisk { configs: self.configs.clone(), selected: self.selected };
        if let Ok(json) = serde_json::to_string_pretty(&disk) {
            let _ = std::fs::write(store_path(root), json);
        }
    }

    pub fn selected(&self) -> Option<&RunConfig> {
        self.configs.get(self.selected)
    }

    /// Merge fresh `detect()` suggestions in, skipping configs already present (by kind+program).
    /// Idempotent. Returns true if anything was added.
    #[cfg(test)]
    pub fn ensure_detected(&mut self, root: &Path) -> bool {
        self.merge_detected(detect(root))
    }

    /// The idempotent detect-merge: APPEND-only. Existing configs (user edits included) are
    /// never touched, removed, or reordered, and `selected` never moves — a background merge
    /// must not clobber `runconfigs.json` edits nor flash a selection the user already made.
    /// Returns true if anything was added (caller persists).
    pub fn merge_detected(&mut self, detected: Vec<RunConfig>) -> bool {
        let existing: Vec<(String, String)> = self.configs.iter().map(|c| c.dedup_key()).collect();
        let mut added = false;
        for cfg in detected {
            if !existing.contains(&cfg.dedup_key()) {
                self.configs.push(cfg);
                added = true;
            }
        }
        if self.selected >= self.configs.len() {
            self.selected = 0;
        }
        added
    }

    /// Run `detect()` on a named worker thread (boot-wave item 2 — recon A1/A7: the ELF walk
    /// was the single largest pre-paint cost). Results land via [`Self::poll_detect`] in
    /// update(); `notify` is the repaint hook so a headless frame wakes to drain them.
    pub fn kick_detect(&mut self, root: &Path, notify: impl Fn() + Send + 'static) {
        self.detect_gen += 1;
        let generation = self.detect_gen;
        let tx = self.detect_tx.clone();
        let root = root.to_path_buf();
        std::thread::Builder::new()
            .name("cauldron-runconfig-detect".into())
            .spawn(move || {
                let found = detect(&root);
                if tx.send((generation, found)).is_ok() {
                    notify();
                }
            })
            .ok();
    }

    /// Drain finished background detections (call once per frame — cheap when idle) and merge
    /// via [`Self::merge_detected`]. Results stamped by an orphaned kick are dropped. Returns
    /// true when anything was added — the caller saves.
    pub fn poll_detect(&mut self) -> bool {
        let mut added = false;
        while let Ok((generation, found)) = self.detect_rx.try_recv() {
            if generation == self.detect_gen {
                added |= self.merge_detected(found);
            }
        }
        added
    }
}

/// Append entries to `.git/info/exclude` if not already present (deps.rs pattern).
fn exclude_from_git(root: &Path, entries: &[&str]) {
    let exclude = root.join(".git/info/exclude");
    if !exclude.parent().is_some_and(|p| p.is_dir()) {
        return; // not a git repo (or bare) — nothing to do
    }
    let mut existing = std::fs::read_to_string(&exclude).unwrap_or_default();
    if !existing.is_empty() && !existing.ends_with('\n') {
        existing.push('\n'); // don't glue our entry onto a final line missing its newline
    }
    let mut add = String::new();
    for e in entries {
        if !existing.lines().any(|l| l.trim() == *e) {
            add.push_str(e);
            add.push('\n');
        }
    }
    if !add.is_empty() {
        let _ = std::fs::write(&exclude, existing + &add);
    }
}

// ---------------------------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------------------------

/// Scan the workspace for likely run configurations.
pub fn detect(root: &Path) -> Vec<RunConfig> {
    let t = crate::boot_trace::begin();
    let walked0 = crate::boot_trace::counter("runconfig-entries-walked");
    let mut out = Vec::new();

    // Order is load-bearing: `selected` defaults to 0, so whatever lands FIRST is what an
    // untouched project runs on Shift+F10. Each block below pushes its "run the app" config ahead
    // of its secondary ones.
    if root.join("Cargo.toml").exists() {
        let (runs, lib_only) = cargo_run_configs(root);
        // A lib-only crate has nothing for `cargo run` to launch ("a bin target must be
        // available"), so leading with it would hand a brand-new `cargo new --lib` project a Run
        // button that only ever errors. There, `cargo test` IS the way you run the code.
        if lib_only {
            out.push(RunConfig::new("cargo test", RunKind::CargoTest, ""));
            out.extend(runs);
        } else {
            out.extend(runs);
            out.push(RunConfig::new("cargo test", RunKind::CargoTest, ""));
        }
    }

    // .NET: a solution or project file at the root. `dotnet run`/`dotnet test` resolve the
    // target from the cwd, so the config carries no program path. Lead with run; add test when
    // the tree looks like a test project (xunit/nunit/mstest markers or a name ending in
    // .Tests) — otherwise `dotnet test` on a plain app just prints "no test to run".
    if let Some(kind) = detect_dotnet(root) {
        out.push(RunConfig::new("dotnet run", RunKind::Dotnet, ""));
        // Hot Reload sits right behind plain run — the same launch, patched on save.
        out.push(RunConfig::new("dotnet watch (hot reload)", RunKind::DotnetWatch, ""));
        if kind == DotnetShape::HasTests {
            out.push(RunConfig::new("dotnet test", RunKind::DotnetTest, ""));
        }
    }

    let has_pytest_ini = root.join("pytest.ini").exists();
    let has_pytest_pyproject = std::fs::read_to_string(root.join("pyproject.toml"))
        .map(|t| t.contains("[tool.pytest"))
        .unwrap_or(false);
    if has_pytest_ini || has_pytest_pyproject || root.join("tests").is_dir() {
        // Only suggest pytest for python-ish trees: tests dir alone in a Rust repo is noise.
        if has_pytest_ini || has_pytest_pyproject || dir_has_py(&root.join("tests")) {
            out.push(RunConfig::new("pytest", RunKind::Pytest, ""));
        }
    }

    // FastAPI gate (boot-wave item 2): the module walk only runs for python-marked roots —
    // pyproject/requirements or root-level *.py. A pure Rust/C tree pays three stats, no walk.
    let pythonish = root.join("pyproject.toml").exists()
        || root.join("requirements.txt").exists()
        || dir_has_py(root);
    if pythonish {
        if let Some(module) = detect_fastapi_module(root) {
            out.push(RunConfig::new(format!("uvicorn {module}"), RunKind::Uvicorn, module));
        }
    }

    // A bare `make` BUILDS — it does not run anything. When the Makefile offers a `run` target,
    // lead with it: "hit Run and the thing runs" is the whole contract, and `make run` is the
    // conventional way a C/C++ tree spells compile-then-launch.
    // The target rides in `program`, NOT in `args` — `dedup_key` is (kind, program), so a
    // `make run` carrying its target in args would collide with the plain `make` config and be
    // silently dropped by merge_detected on any project that already has one. (args cannot join
    // the key: a user who adds `--fast` to a detected config must not have a duplicate re-added.)
    if root.join("Makefile").exists() {
        if make_has_target(root, "run") {
            out.push(RunConfig::new("make run", RunKind::Make, "run"));
        }
        out.push(RunConfig::new("make", RunKind::Make, ""));
    }

    // A python entry point: the file you'd type `python <x>` at. Ordered by convention, first hit
    // wins — a project with both main.py and app.py meant main.py.
    if let Some(entry) = ["main.py", "app.py", "__main__.py", "run.py"]
        .into_iter()
        .find(|f| root.join(f).is_file())
    {
        out.push(RunConfig::new(format!("python {entry}"), RunKind::Python, entry));
    }

    out.extend(detect_node(root));

    // A truly static page has no process to launch — "running" it means opening it in the browser.
    // But a bundled app (Vite/Next/…) ALSO has an index.html at the root, and opening that raw file
    // over file:// breaks every absolute module URL in it — that project is served by its dev
    // server (handled above), so the static-page rule is suppressed whenever a package.json exists.
    if root.join("index.html").is_file() && !root.join("package.json").exists() {
        out.push(RunConfig {
            args: vec!["index.html".to_string()],
            ..RunConfig::new("open index.html", RunKind::Custom, "xdg-open")
        });
    }

    for bin in find_executables(root) {
        let name = bin.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        out.push(RunConfig::new(name, RunKind::Binary, bin.to_string_lossy().into_owned()));
    }

    if crate::boot_trace::enabled() {
        let walked = crate::boot_trace::counter("runconfig-entries-walked") - walked0;
        crate::boot_trace::end(t, "runconfig-detect", &format!("entries-walked={walked}"));
    }
    out
}

/// The `cargo run` config(s) for the crate/workspace at `root`, and whether it is LIB-ONLY.
///
/// The multi-binary case is the whole reason this is not just `cargo run`: at a workspace root (or
/// any crate with several `[[bin]]`s) a bare `cargo run` fails with *"could not determine which
/// binary to run"* and lists them. So each binary gets its own `cargo run --bin <name>` config, the
/// one matching the project's own name first (that is the "main" app far more often than not), and
/// the user picks from the Run dropdown instead of hitting an error.
///
/// The binary list comes from `cargo metadata`, which is authoritative for workspaces (it reads
/// every member's real targets, including `[[bin]]` renames the filesystem cannot show). When it is
/// unavailable or fails — an as-yet-uncompilable manifest, no network for a first resolve — it
/// falls back to the cheap filesystem heuristic and a single bare `cargo run`.
fn cargo_run_configs(root: &Path) -> (Vec<RunConfig>, bool) {
    let bin = |name: &str| RunConfig::new(format!("cargo run --bin {name}"), RunKind::Cargo, name);
    match cargo_bins(root) {
        // Authoritative and multi-binary: one config each, the "main" binary first.
        Some((bins, default_run)) if bins.len() >= 2 => {
            let mut ordered = bins;
            ordered.sort();
            // Which bin leads (selected = 0)? A package's own `default-run` is the project's
            // explicit statement of intent and wins; otherwise the bin named for the checkout
            // directory (usually the app); otherwise the first alphabetically. Directory name is
            // last because a repo cloned into a differently-named dir would mislead it.
            let lead = default_run
                .filter(|d| ordered.iter().any(|b| b == d))
                .or_else(|| {
                    let self_name = root.file_name().map(|n| n.to_string_lossy().into_owned())?;
                    ordered.iter().find(|b| **b == self_name).cloned()
                });
            if let Some(lead) = lead {
                if let Some(pos) = ordered.iter().position(|b| *b == lead) {
                    ordered.swap(0, pos);
                }
            }
            (ordered.iter().map(|b| bin(b)).collect(), false)
        }
        // Authoritative: exactly one bin (bare `cargo run` is unambiguous) or zero (lib-only).
        Some((bins, _)) => (vec![RunConfig::new("cargo run", RunKind::Cargo, "")], bins.is_empty()),
        // Heuristic fallback: cannot enumerate, so a single bare `cargo run` and a guess at bin-ness.
        None => (vec![RunConfig::new("cargo run", RunKind::Cargo, "")], !cargo_has_bin(root)),
    }
}

/// Every `bin` target in the workspace and the `default-run` (if any package declares one), via
/// `cargo metadata --no-deps` (fast: reads the manifests, never touches the registry). `None` when
/// cargo is absent or the manifest will not parse; `Some((vec![], _))` is a definitive "lib-only".
fn cargo_bins(root: &Path) -> Option<(Vec<String>, Option<String>)> {
    let out = std::process::Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let mut bins = Vec::new();
    let mut default_run = None;
    for pkg in v["packages"].as_array()? {
        if let Some(d) = pkg["default_run"].as_str() {
            default_run = Some(d.to_string());
        }
        for t in pkg["targets"].as_array().into_iter().flatten() {
            let is_bin = t["kind"].as_array().is_some_and(|k| k.iter().any(|x| x == "bin"));
            if is_bin {
                if let Some(name) = t["name"].as_str() {
                    bins.push(name.to_string());
                }
            }
        }
    }
    bins.sort();
    bins.dedup();
    Some((bins, default_run))
}

/// Filesystem-only guess at whether a crate has a runnable bin — the fallback for when
/// `cargo metadata` cannot answer. Deliberately generous: a false positive just means Run errors
/// the way it does today, a false negative would demote `cargo run` on a normal binary crate.
fn cargo_has_bin(root: &Path) -> bool {
    if root.join("src/main.rs").is_file() || root.join("src/bin").is_dir() {
        return true;
    }
    match std::fs::read_to_string(root.join("Cargo.toml")) {
        Ok(t) => t.contains("[[bin]]") || t.contains("[workspace]"),
        Err(_) => false,
    }
}

/// Does the Makefile declare a rule for `<target>`? Scans rule HEADS, carefully enough not to be
/// fooled by the several things that also contain a colon:
///
/// - **Recipe lines** start with a TAB — never a rule head (a target's commands, which routinely
///   contain colons: `docker run -v $(PWD):/src`).
/// - **Assignments** — `X := v`, `X ::= v`, and crucially `X = v` whose VALUE contains a colon
///   (`DOCKER = docker run ... $(PWD):/src`). The last is the one that made a bogus `make run` the
///   default: splitting on the first colon left `DOCKER = docker run ... $(PWD)` as the "head",
///   which happens to contain the word `run`. A head with an `=` before the colon is never a rule.
/// - **Pattern rules** (`%.o:`) are not named targets.
///
/// GNU make accepts a rule head indented with SPACES (only a leading tab means recipe), so leading
/// spaces are trimmed rather than treated as disqualifying.
fn make_has_target(root: &Path, target: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(root.join("Makefile")) else { return false };
    text.lines().any(|raw| {
        if raw.starts_with('\t') {
            return false; // recipe line
        }
        let line = raw.trim_start();
        if line.is_empty() || line.starts_with('#') {
            return false;
        }
        let Some((head, after)) = line.split_once(':') else { return false };
        // `X := v` / `X ::= v` (the split colon is the assignment operator's).
        if after.starts_with('=') || after.starts_with(":=") {
            return false;
        }
        // `X = v` / `X += v` / `X ?= v` where the colon we split on lives in the VALUE.
        if head.contains('=') {
            return false;
        }
        // A rule head may list several targets: `all run: deps`. Pattern targets are not names.
        head.split_whitespace().any(|t| t == target && !t.contains('%'))
    })
}

/// npm scripts → a run config. `start` is the entry point by convention; when the package also
/// declares a `build` (a TypeScript project), Run must compile FIRST or it launches a stale — or
/// missing — `dist/`. Dependencies are installed on demand: a freshly created project has no
/// `node_modules`, and "hit Run and it works" must not require remembering `npm install`.
fn detect_node(root: &Path) -> Vec<RunConfig> {
    let Ok(text) = std::fs::read_to_string(root.join("package.json")) else { return Vec::new() };
    let Ok(pkg) = serde_json::from_str::<serde_json::Value>(&text) else { return Vec::new() };
    let has_script = |name: &str| pkg["scripts"][name].is_string();

    let needs_install = pkg["dependencies"].as_object().is_some_and(|d| !d.is_empty())
        || pkg["devDependencies"].as_object().is_some_and(|d| !d.is_empty());
    // `[ -d node_modules ] ||` and not a bare `npm install`: paying an install on every Run would
    // make the button feel broken. `&&` (not `;`) after it so a FAILED install stops with its own
    // error instead of running `npm start` against a half-populated tree. Idempotent — a deleted
    // node_modules just reinstalls.
    let install = if needs_install { "[ -d node_modules ] || npm install && " } else { "" };

    // Which script is "run"? `dev` is the run-while-coding command of every modern framework
    // (Vite, Next, Nuxt, SvelteKit, Astro) and the ones that HAVE a dev server usually have no
    // meaningful `start` until after a build — so it wins. Otherwise `start`, building first when
    // the project also declares a build (a compiled TS entry point runs a stale/absent dist/
    // without it). A package with neither is a library, not something to run.
    let (name, run) = if has_script("dev") {
        ("npm dev", "npm run dev".to_string())
    } else if has_script("start") && has_script("build") {
        ("npm build + start", "npm run build && npm start".to_string())
    } else if has_script("start") {
        ("npm start", "npm start".to_string())
    } else {
        return Vec::new();
    };
    vec![RunConfig {
        args: vec!["-c".to_string(), format!("{install}{run}")],
        ..RunConfig::new(name, RunKind::Custom, "sh")
    }]
}

/// Whether the root is a .NET project, and if so whether it looks test-shaped.
#[derive(PartialEq, Eq)]
enum DotnetShape {
    App,
    HasTests,
}

/// Is `root` a .NET project? Looks for a `.sln`/`.csproj`/`.fsproj`/`.vbproj` at the top level
/// (shallow — `dotnet run`/`test` resolve deeper targets themselves from the cwd). Classifies as
/// test-shaped when a project file name ends in `.Tests`/`.Test` or a `.csproj` references a
/// known test framework — that's the signal for whether to offer `dotnet test`.
fn detect_dotnet(root: &Path) -> Option<DotnetShape> {
    let mut found = false;
    let mut has_tests = false;
    let entries = std::fs::read_dir(root).ok()?;
    for e in entries.flatten() {
        let p = e.path();
        let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("");
        if !matches!(ext, "sln" | "csproj" | "fsproj" | "vbproj") {
            continue;
        }
        found = true;
        let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if stem.ends_with(".Tests") || stem.ends_with(".Test") || stem.ends_with("Tests") {
            has_tests = true;
        }
        // A cheap content sniff for the test SDKs — catches test projects not named *.Tests.
        if ext == "csproj" {
            if let Ok(text) = std::fs::read_to_string(&p) {
                if text.contains("Microsoft.NET.Test.Sdk")
                    || text.contains("xunit")
                    || text.contains("NUnit")
                    || text.contains("MSTest")
                {
                    has_tests = true;
                }
            }
        }
    }
    if !found {
        return None;
    }
    Some(if has_tests { DotnetShape::HasTests } else { DotnetShape::App })
}

fn dir_has_py(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .any(|e| e.path().extension().is_some_and(|x| x == "py"))
        })
        .unwrap_or(false)
}

/// Find a `*.py` containing "FastAPI(" (shallow-ish walk) and return "module:app".
fn detect_fastapi_module(root: &Path) -> Option<String> {
    let mut b = ignore::WalkBuilder::new(root);
    b.max_depth(Some(4)).filter_entry(|e| {
        let n = e.file_name().to_string_lossy();
        n != ".git" && n != ".cauldron" && n != "node_modules" && n != "target" && n != ".venv" && n != "venv"
    });
    let mut walked: u64 = 0;
    for entry in b.build().flatten() {
        walked += 1;
        let p = entry.path();
        if p.extension().is_some_and(|x| x == "py") {
            if let Ok(text) = std::fs::read_to_string(p) {
                if text.contains("FastAPI(") {
                    // module path: relative path, strip .py, / → .
                    let rel = p.strip_prefix(root).unwrap_or(p);
                    let module = rel
                        .with_extension("")
                        .components()
                        .map(|c| c.as_os_str().to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                        .join(".");
                    crate::boot_trace::count("runconfig-entries-walked", walked);
                    return Some(format!("{module}:app"));
                }
            }
        }
    }
    crate::boot_trace::count("runconfig-entries-walked", walked);
    None
}

/// Hard fuse on directory entries examined across ALL build-dir walks. The old bound-free
/// depth-7, ignores-off walk from the root hit 1.15M entries on $HOME (recon A1) — this caps
/// the worst case at ~50k even inside a pathological build dir.
const DETECT_ENTRY_FUSE: u64 = 50_000;

/// The ONLY places built ELFs are looked for (boot-wave item 2): conventional build output
/// dirs — `target/debug`, `target/release`, `target/<triple>/deps`, `build*`, `cmake-build-*`.
/// Everything else (sources, vendor trees, $HOME junk when the root is misplaced) is never
/// walked.
fn build_dirs(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let target = root.join("target");
    for name in ["debug", "release"] {
        let d = target.join(name);
        if d.is_dir() {
            out.push(d);
        }
    }
    // Cross-compilation triples: target/<triple>/deps.
    if let Ok(rd) = std::fs::read_dir(&target) {
        for e in rd.flatten() {
            let name = e.file_name();
            let n = name.to_string_lossy();
            if n == "debug" || n == "release" {
                continue; // already added above (their deps/ is inside the recursive walk)
            }
            let d = e.path().join("deps");
            if d.is_dir() {
                out.push(d);
            }
        }
    }
    // CMake/Meson/etc. conventions at the root: build*/ and cmake-build-*/.
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.flatten() {
            let name = e.file_name();
            let n = name.to_string_lossy();
            if (n.starts_with("build") || n.starts_with("cmake-build-"))
                && e.file_type().is_ok_and(|t| t.is_dir())
            {
                out.push(e.path());
            }
        }
    }
    out
}

/// Newest mtime across the build dirs INCLUDING nested subdirectories. Stat'ing only the
/// top-level dirs is not enough: on POSIX, creating `build/bin/newapp` bumps `build/bin`'s
/// mtime but NOT `build`'s, so a CMake binary landing in `build/bin/` (or a cargo example in
/// `target/debug/examples/`) would hide behind a fresh stamp FOREVER — and every run-config
/// edit re-saves the json, refreshing it. Directories only (a new file always bumps its own
/// parent dir), same chaff skip-list as the ELF walk, bounded by depth + a directory fuse;
/// `None` = nothing to stat or too big to prove freshness cheaply (caller re-walks).
fn newest_build_mtime(dirs: &[PathBuf]) -> Option<std::time::SystemTime> {
    const DIR_FUSE: usize = 5_000;
    let mut newest: Option<std::time::SystemTime> = None;
    let mut visited = 0usize;
    let mut stack: Vec<(PathBuf, usize)> = dirs.iter().map(|d| (d.clone(), 0usize)).collect();
    while let Some((dir, depth)) = stack.pop() {
        visited += 1;
        if visited > DIR_FUSE {
            return None; // conservative: an unprovable short-circuit must not skip the walk
        }
        if let Ok(m) = std::fs::metadata(&dir).and_then(|m| m.modified()) {
            newest = Some(newest.map_or(m, |n: std::time::SystemTime| n.max(m)));
        }
        // Depth matches the ELF walk's max_depth — outputs deeper than that are never found
        // by detection anyway, so their mtimes cannot matter.
        if depth >= 6 {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            if !e.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let name = e.file_name();
            let n = name.to_string_lossy();
            // Same chaff the ELF walk skips: a binary never lands there, so churn inside
            // must not lift the short-circuit (cargo bumps .fingerprint on EVERY build).
            if n == ".git"
                || n == ".cauldron"
                || n == "node_modules"
                || n == "CMakeFiles"
                || n == ".venv"
                || n == "venv"
                || n == "incremental"
                || n == ".fingerprint"
            {
                continue;
            }
            stack.push((e.path(), depth + 1));
        }
    }
    newest
}

/// Short-circuit (recon plan Commit 1): when `runconfigs.json` already holds a Binary config
/// and is NEWER than every build dir — nested subdirs included, see [`newest_build_mtime`] —
/// (a build writing outputs bumps the containing dir's mtime), the last detection is still
/// current — the ELF walk can be skipped entirely.
fn fresh_binary_config(root: &Path, dirs: &[PathBuf]) -> bool {
    let Ok(json_mtime) = std::fs::metadata(store_path(root)).and_then(|m| m.modified()) else {
        return false;
    };
    let Some(newest_build) = newest_build_mtime(dirs) else {
        return false;
    };
    // Conservative on ties: fs timestamps are jiffy-granular, so "equal" can hide a build that
    // landed just after the save — only a STRICTLY newer json short-circuits.
    if json_mtime <= newest_build {
        return false; // something (possibly) built since the last save — walk again
    }
    std::fs::read_to_string(store_path(root))
        .ok()
        .and_then(|t| serde_json::from_str::<StoreOnDisk>(&t).ok())
        .is_some_and(|d| d.configs.iter().any(|c| c.kind == RunKind::Binary))
}

/// Built ELF executables — ranking non-test binaries first and capping at 5. Bounded (boot-wave
/// item 2): walks ONLY [`build_dirs`], skips known chaff (`incremental/`, `.fingerprint/`),
/// stops at [`DETECT_ENTRY_FUSE`] entries, and short-circuits via [`fresh_binary_config`].
fn find_executables(root: &Path) -> Vec<PathBuf> {
    find_executables_bounded(root, DETECT_ENTRY_FUSE)
}

fn find_executables_bounded(root: &Path, fuse: u64) -> Vec<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let dirs = build_dirs(root);
    if dirs.is_empty() || fresh_binary_config(root, &dirs) {
        return Vec::new();
    }
    let mut out: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    let mut walked: u64 = 0;
    'dirs: for dir in &dirs {
        // Executables live in gitignored dirs (build/, target/) — disable ignore files entirely.
        let mut b = ignore::WalkBuilder::new(dir);
        b.hidden(false)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .ignore(false)
            .parents(false)
            .max_depth(Some(6))
            .filter_entry(|e| {
                let n = e.file_name().to_string_lossy();
                n != ".git"
                    && n != ".cauldron"
                    && n != "node_modules"
                    && n != "CMakeFiles"
                    && n != ".venv"
                    && n != "venv"
                    // cargo chaff: thousands of entries, never a runnable binary
                    && n != "incremental"
                    && n != ".fingerprint"
            });
        for entry in b.build().flatten() {
            walked += 1;
            if walked > fuse {
                crate::boot_trace::boot_mark!("runconfig-detect fuse tripped at {} entries", fuse);
                break 'dirs;
            }
            let p = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
                continue;
            }
            if p.extension().is_some_and(|e| matches!(e.to_str(), Some("so") | Some("o") | Some("a") | Some("sh") | Some("py"))) {
                continue;
            }
            let Ok(mut f) = std::fs::File::open(p) else { continue };
            let mut head = [0u8; 18];
            use std::io::Read as _;
            if f.read_exact(&mut head).is_err() || &head[..4] != b"\x7fELF" {
                continue;
            }
            let mtime = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            out.push((mtime, p.to_path_buf()));
        }
    }
    // Recon A1/A7 evidence counter — the walk this item bounded (was 1.15M entries on $HOME).
    crate::boot_trace::count("runconfig-entries-walked", walked);
    let is_testish = |p: &Path| {
        let s = p.to_string_lossy().to_lowercase();
        s.contains("test") || s.contains("coverage") || s.contains("ut-") || s.contains("stub")
    };
    out.sort_by(|a, b| is_testish(&a.1).cmp(&is_testish(&b.1)).then(b.0.cmp(&a.0)));
    out.truncate(5);
    out.into_iter().map(|(_, p)| p).collect()
}

// ---------------------------------------------------------------------------------------------
// Command resolution
// ---------------------------------------------------------------------------------------------

/// Resolve a config into (program, args, cwd) ready for process spawn.
/// The interpreter a python config should use: the project's OWN `.venv` when it has one, else the
/// system `python3`.
///
/// This is the whole point of shipping every new project with a venv. On a PEP-668 distro (Arch,
/// and every RuneOS box) the system interpreter refuses `pip install` outright, so a project's
/// dependencies can only live in its venv — and a run that shells out to bare `python3` then fails
/// on `import` for a package the user definitely installed. Falls back to `python3` so a project
/// without a venv (every pre-existing one) behaves exactly as before.
pub fn python_bin(root: &Path) -> String {
    let venv = root.join(".venv/bin/python");
    if venv.is_file() {
        return venv.to_string_lossy().into_owned();
    }
    "python3".to_string()
}

/// Quote `s` for `sh -c`. Single-quoted, with embedded quotes closed/escaped/reopened — the only
/// form that is safe for arbitrary paths (spaces, `$`, `;`, quotes are all legal in a filename).
pub(crate) fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Where a compiled single-file run puts its binary: `~/.cache/cauldron/run/<hash>-<stem>`.
///
/// Deliberately NOT `/tmp`. `/tmp` is world-writable, so a pre-planted symlink at a guessable name
/// (`cauldron-run-main`) would be followed by `cc -o` and silently overwrite whatever it points at,
/// with the running user's rights. The cache dir is inside $HOME and owned by that user.
///
/// The hash is of the file's ABSOLUTE path, so `main.c` in two different projects — or two files
/// with the same stem — never land on the same binary and clobber each other's build.
fn run_artifact(file: &Path, stem: &str) -> PathBuf {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    file.hash(&mut h);
    let dir = match std::env::var_os("XDG_CACHE_HOME") {
        Some(c) => PathBuf::from(c),
        None => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(".cache"),
            None => std::env::temp_dir(),
        },
    };
    dir.join("cauldron/run").join(format!("{:016x}-{stem}", h.finish()))
}

/// The PACKAGE `Cargo.toml` that owns `file`: the nearest one walking up from the file, stopping at
/// the opened project root. In a workspace this is the member's manifest, not the virtual root's — a
/// member binary must be run against its own crate.
///
/// A VIRTUAL manifest (a `[workspace]` with no `[package]`) is skipped, not returned: it owns no
/// targets, so `cargo run --manifest-path <it>` would run some arbitrary member bin (or error), not
/// the file in hand. A stray `src/main.rs` directly under a virtual root therefore gets `None` and
/// is run with standalone rustc — the honest answer for a file no package claims.
fn nearest_manifest(file: &Path, root: &Path) -> Option<PathBuf> {
    let mut cur = file.parent();
    while let Some(dir) = cur {
        let m = dir.join("Cargo.toml");
        if m.is_file() && manifest_has_package(&m) {
            return Some(m);
        }
        if dir == root {
            break; // never walk above the opened project
        }
        cur = dir.parent();
    }
    None
}

/// Does this `Cargo.toml` declare a `[package]` (i.e. own targets), as opposed to being a virtual
/// workspace manifest? A cheap textual check — a false read just costs a fallback to rustc.
fn manifest_has_package(manifest: &Path) -> bool {
    std::fs::read_to_string(manifest)
        .map(|t| t.lines().any(|l| l.trim_start().starts_with("[package]")))
        .unwrap_or(false)
}

/// Classify a path RELATIVE to its crate manifest as a cargo run target: the cargo flag to select
/// it (`--bin` / `--example`, or `None` for the crate's default binary) and the target name.
/// `None` for anything that is not a runnable target (a library module, a test, a bench) — the
/// caller then falls back to standalone rustc.
///
/// Covers both layouts cargo accepts for bins and examples: a single file (`examples/demo.rs`) and
/// a directory with a `main.rs` (`examples/demo/main.rs`), the latter named for the DIRECTORY.
fn cargo_target(rel: &Path) -> Option<(Option<&'static str>, String)> {
    let c: Vec<String> =
        rel.components().map(|x| x.as_os_str().to_string_lossy().into_owned()).collect();
    let stem = |s: &str| {
        Path::new(s).file_stem().map(|x| x.to_string_lossy().into_owned()).unwrap_or_default()
    };
    let cs: Vec<&str> = c.iter().map(|s| s.as_str()).collect();
    match cs.as_slice() {
        ["src", "main.rs"] => Some((None, String::new())),
        ["src", "bin", f] if f.ends_with(".rs") => Some((Some("--bin"), stem(f))),
        ["src", "bin", n, "main.rs"] => Some((Some("--bin"), n.to_string())),
        ["examples", f] if f.ends_with(".rs") => Some((Some("--example"), stem(f))),
        ["examples", n, "main.rs"] => Some((Some("--example"), n.to_string())),
        _ => None,
    }
}

/// The command that runs ONE file, from its extension. Compiled languages get a
/// compile-then-run shell line so a single keystroke produces output rather than an object file;
/// interpreted ones exec directly. `None` for a file we have no idea how to run — the caller
/// reports that rather than spawning something bogus.
///
/// Returned as (program, args, cwd). cwd is the PROJECT ROOT, not the file's directory: a script
/// resolving `data/x.csv` or importing a sibling package expects to stand where the project does,
/// which is also what `python main.py` from a terminal at the root would do.
fn file_command(file: &Path, root: &Path, extra: &[String]) -> Option<(String, Vec<String>)> {
    let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    let stem = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let path = file.to_string_lossy().into_owned();
    let argv = |prog: &str, first: Vec<String>| {
        let mut a = first;
        a.extend(extra.iter().cloned());
        Some((prog.to_string(), a))
    };
    // Compile-and-run, chained on `&&`: a failed compile shows the compiler's errors and stops.
    // It can never exec a STALE binary from an earlier successful run, because the compile step is
    // what gates the exec — and `rm -f` clears the artifact first, so even a compiler that leaves a
    // partial output behind cannot be mistaken for a fresh build.
    let out = run_artifact(file, &stem);
    let out_q = sh_quote(&out.to_string_lossy());
    let dir_q = sh_quote(&out.parent().unwrap_or(Path::new("/tmp")).to_string_lossy());
    let args_q: Vec<String> = extra.iter().map(|a| sh_quote(a)).collect();
    let rest = args_q.join(" ");
    // `-g -O0`: the artifact this produces is the SAME one Debug-current-file attaches to, and
    // without debug info lldb has no line table, no locals, and no breakpoint resolution — the
    // whole debug path was unusable on a single file. -O0 keeps stepping honest (no reordered
    // lines, no elided variables); a one-file script is never being run for its throughput.
    let compile_run = |cc: &str, std_flag: &str| {
        let script = format!(
            "mkdir -p {dir_q} && rm -f {out_q} && {cc} {std_flag} -g -O0 -Wall -o {out_q} {src} \
             && exec {out_q} {rest}",
            src = sh_quote(&path),
        );
        Some(("sh".to_string(), vec!["-c".to_string(), script]))
    };

    match ext.as_str() {
        "py" => argv(&python_bin(root), vec![path]),
        "js" | "mjs" | "cjs" => argv("node", vec![path]),
        // tsx runs TypeScript straight from source (no tsconfig/build step needed for one file).
        "ts" | "tsx" => argv("npx", vec!["--yes".into(), "tsx".into(), path]),
        "sh" | "bash" => argv("bash", vec![path]),
        "go" => argv("go", vec!["run".into(), path]),
        "html" | "htm" => argv("xdg-open", vec![path]),
        "c" => compile_run("cc", "-std=c11"),
        "cpp" | "cc" | "cxx" => compile_run("c++", "-std=c++20"),
        "rs" => {
            // A lone `rustc` drops every dependency on the floor, so defer to cargo whenever the
            // file is a real cargo target — addressed by its OWNING crate's manifest, which in a
            // workspace is a member `Cargo.toml` well below the opened root (that is the whole bug
            // the reviewer caught: comparing against the workspace root never matched a member).
            // `--manifest-path` is what lets this run from the workspace-root cwd. A stray `.rs`
            // (a scratch script, a bare module) is no cargo target — rustc it standalone.
            if let Some(manifest) = nearest_manifest(file, root) {
                let mdir = manifest.parent().unwrap_or(root);
                let rel = file.strip_prefix(mdir).unwrap_or(file);
                if let Some((flag, name)) = cargo_target(rel) {
                    let mp = manifest.to_string_lossy().into_owned();
                    let mut a = vec!["run".to_string(), "--manifest-path".to_string(), mp];
                    if let Some(flag) = flag {
                        a.push(flag.to_string());
                        a.push(name);
                    }
                    return argv("cargo", a);
                }
            }
            Some((
                "sh".to_string(),
                vec![
                    "-c".to_string(),
                    format!(
                        "mkdir -p {dir_q} && rm -f {out_q} && rustc -o {out_q} {src} \
                         && exec {out_q} {rest}",
                        src = sh_quote(&path),
                    ),
                ],
            ))
        }
        _ => None,
    }
}

pub fn command_line(cfg: &RunConfig, root: &Path) -> (String, Vec<String>, PathBuf) {
    let default_cwd = |fallback: PathBuf| cfg.cwd.clone().unwrap_or(fallback);
    match cfg.kind {
        RunKind::File => {
            let file = PathBuf::from(&cfg.program);
            let cwd = default_cwd(root.to_path_buf());
            match file_command(&file, root, &cfg.args) {
                Some((prog, args)) => (prog, args, cwd),
                // Surfaced in the Output pane the same way any other failing run is — one clear
                // line, non-zero exit — rather than silently doing nothing. The message is
                // sh_quote'd: a file name is arbitrary user input, and a lone quote in it
                // ('a'b.txt) would otherwise close the echo string and run the rest as commands.
                None => {
                    let msg = format!(
                        "cauldron: no run command for {}",
                        file.file_name().unwrap_or_default().to_string_lossy()
                    );
                    ("sh".into(), vec!["-c".into(), format!("echo {} >&2; exit 1", sh_quote(&msg))], cwd)
                }
            }
        }
        // `program` is the bin to run when set — `cargo run --bin <program>`, which is how a
        // multi-binary workspace is disambiguated. Empty = bare `cargo run` (single-bin crate).
        RunKind::Cargo => {
            let mut args = vec!["run".to_string()];
            if !cfg.program.is_empty() {
                args.push("--bin".to_string());
                args.push(cfg.program.clone());
            }
            args.extend(cfg.args.iter().cloned());
            ("cargo".into(), args, default_cwd(root.to_path_buf()))
        }
        RunKind::CargoTest => {
            let mut args = vec!["test".to_string()];
            args.extend(cfg.args.iter().cloned());
            ("cargo".into(), args, default_cwd(root.to_path_buf()))
        }
        // `dotnet run` resolves the project from the cwd. Extra user args go after `--` so dotnet
        // forwards them to the app instead of parsing them as its own flags.
        RunKind::Dotnet => {
            let mut args = vec!["run".to_string()];
            if !cfg.args.is_empty() {
                args.push("--".to_string());
                args.extend(cfg.args.iter().cloned());
            }
            ("dotnet".into(), args, default_cwd(root.to_path_buf()))
        }
        RunKind::DotnetTest => {
            let mut args = vec!["test".to_string()];
            args.extend(cfg.args.iter().cloned());
            ("dotnet".into(), args, default_cwd(root.to_path_buf()))
        }
        // `dotnet watch run` = Hot Reload: the SDK watches the source tree and patches the running
        // process on save (Cauldron's Run auto-saves, so a plain Ctrl+S reloads the app).
        RunKind::DotnetWatch => {
            let mut args = vec!["watch".to_string(), "run".to_string()];
            if !cfg.args.is_empty() {
                args.push("--".to_string());
                args.extend(cfg.args.iter().cloned());
            }
            ("dotnet".into(), args, default_cwd(root.to_path_buf()))
        }
        RunKind::Binary => {
            let bin = PathBuf::from(&cfg.program);
            let parent = bin.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| root.to_path_buf());
            (cfg.program.clone(), cfg.args.clone(), default_cwd(parent))
        }
        RunKind::Python => {
            let mut args = vec![cfg.program.clone()];
            args.extend(cfg.args.iter().cloned());
            (python_bin(root), args, default_cwd(root.to_path_buf()))
        }
        RunKind::Pytest => {
            let mut args = vec!["-m".to_string(), "pytest".to_string()];
            args.extend(cfg.args.iter().cloned());
            (python_bin(root), args, default_cwd(root.to_path_buf()))
        }
        // `python -m uvicorn` rather than the bare `uvicorn` binary: the venv's script is only on
        // $PATH if the venv is ACTIVATED, which a spawned process never is — but its interpreter
        // finds the installed module every time.
        RunKind::Uvicorn => {
            let mut args = vec!["-m".to_string(), "uvicorn".to_string(), cfg.program.clone()];
            args.extend(cfg.args.iter().cloned());
            (python_bin(root), args, default_cwd(root.to_path_buf()))
        }
        // `program` is the make TARGET when set ("run"), empty for a bare `make` (the default
        // goal — a build). See `detect`: it is what keeps the two configs distinct under dedup.
        RunKind::Make => {
            let mut args: Vec<String> =
                if cfg.program.is_empty() { Vec::new() } else { vec![cfg.program.clone()] };
            args.extend(cfg.args.iter().cloned());
            ("make".into(), args, default_cwd(root.to_path_buf()))
        }
        RunKind::Custom => {
            (cfg.program.clone(), cfg.args.clone(), default_cwd(root.to_path_buf()))
        }
    }
}

/// Which debug adapter a target needs. Chosen by the app to pick the DAP backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugAdapter {
    /// Native code (lldb-dap) — Rust/C/C++ binaries.
    Native,
    /// Python (debugpy).
    Python,
    /// .NET (netcoredbg) — `program` is the built managed DLL.
    Dotnet,
}

/// Debuggable target for a config: (program, args, cwd, adapter).
/// Cargo maps to `target/debug/<rootname>`; Dotnet to its built DLL under `bin/Debug`;
/// CargoTest/DotnetTest/Make/Custom aren't debuggable here.
pub fn debug_target(
    cfg: &RunConfig,
    root: &Path,
) -> Option<(PathBuf, Vec<String>, PathBuf, DebugAdapter)> {
    match cfg.kind {
        RunKind::Cargo => {
            // The specific bin when the config names one, else the package-named default.
            let name = if cfg.program.is_empty() {
                root.file_name()?.to_string_lossy().into_owned()
            } else {
                cfg.program.clone()
            };
            let bin = root.join("target/debug").join(name);
            Some((
                bin,
                cfg.args.clone(),
                cfg.cwd.clone().unwrap_or_else(|| root.to_path_buf()),
                DebugAdapter::Native,
            ))
        }
        RunKind::Binary => {
            let bin = PathBuf::from(&cfg.program);
            let cwd = cfg
                .cwd
                .clone()
                .unwrap_or_else(|| bin.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| root.to_path_buf()));
            Some((bin, cfg.args.clone(), cwd, DebugAdapter::Native))
        }
        // The built managed DLL under bin/Debug/<tfm>/. Requires a prior build — if nothing is
        // there yet, Debug reports it can't (Run still builds+runs). netcoredbg loads the DLL.
        RunKind::Dotnet => {
            let dll = find_dotnet_dll(root)?;
            Some((
                dll,
                cfg.args.clone(),
                cfg.cwd.clone().unwrap_or_else(|| root.to_path_buf()),
                DebugAdapter::Dotnet,
            ))
        }
        RunKind::Python => Some((
            PathBuf::from(&cfg.program),
            cfg.args.clone(),
            cfg.cwd.clone().unwrap_or_else(|| root.to_path_buf()),
            DebugAdapter::Python,
        )),
        // pytest/uvicorn debug as PYTHON MODULES (their `program` is empty / the app spec, not a
        // script path) — the old code passed an empty `program` to debugpy and every launch
        // failed. The `-m:<module>` sentinel tells the debugpy launch to use `"module"` instead
        // of `"program"`; uvicorn's app module (cfg.program, e.g. "main:app") leads its args,
        // exactly like the Run command.
        RunKind::Pytest => Some((
            PathBuf::from("-m:pytest"),
            cfg.args.clone(),
            cfg.cwd.clone().unwrap_or_else(|| root.to_path_buf()),
            DebugAdapter::Python,
        )),
        RunKind::Uvicorn => {
            let mut args = Vec::new();
            if !cfg.program.is_empty() {
                args.push(cfg.program.clone());
            }
            args.extend(cfg.args.iter().cloned());
            Some((
                PathBuf::from("-m:uvicorn"),
                args,
                cfg.cwd.clone().unwrap_or_else(|| root.to_path_buf()),
                DebugAdapter::Python,
            ))
        }
        // A python file debugs like any other python script. The compiled languages would need the
        // temp binary built with -g first, which `debug_target` has no way to do — so they are not
        // debuggable from a bare "run this file" (Run still works; Debug says it cannot).
        RunKind::File => {
            let file = PathBuf::from(&cfg.program);
            let is_py = file.extension().is_some_and(|e| e == "py");
            is_py.then(|| {
                (
                    file,
                    cfg.args.clone(),
                    cfg.cwd.clone().unwrap_or_else(|| root.to_path_buf()),
                    DebugAdapter::Python,
                )
            })
        }
        // dotnet watch owns the process lifecycle itself — attaching a debugger to it isn't the
        // model; debug the plain Dotnet config instead.
        RunKind::CargoTest
        | RunKind::DotnetTest
        | RunKind::DotnetWatch
        | RunKind::Make
        | RunKind::Custom => None,
    }
}

/// The most-recently-built managed DLL under `bin/Debug/<tfm>/`. Skips the ref-assemblies dir
/// and picks the newest so the last build wins in a multi-target project.
fn find_dotnet_dll(root: &Path) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    let mut b = ignore::WalkBuilder::new(root.join("bin").join("Debug"));
    b.standard_filters(false).max_depth(Some(2));
    for entry in b.build().flatten() {
        let p = entry.path();
        if p.extension().and_then(|x| x.to_str()) != Some("dll") {
            continue;
        }
        // ref/ holds reference assemblies, not runnable output.
        if p.components().any(|c| c.as_os_str() == "ref") {
            continue;
        }
        let mtime = entry.metadata().ok().and_then(|m| m.modified().ok())?;
        if best.as_ref().is_none_or(|(t, _)| mtime > *t) {
            best = Some((mtime, p.to_path_buf()));
        }
    }
    best.map(|(_, p)| p)
}

// ---------------------------------------------------------------------------------------------
// UI
// ---------------------------------------------------------------------------------------------

impl RunConfigStore {
    /// Compact selector dropdown + "Edit…" entry. Returns true when the selection changed.
    /// The edit window (add/remove/rename/args/env) lives inside the store and is drawn here too.
    pub fn selector_ui(&mut self, ui: &mut egui::Ui, root: &Path) -> bool {
        let mut changed = false;
        let current = self
            .selected()
            .map(|c| c.name.clone())
            .unwrap_or_else(|| "no config".to_string());

        ui.scope(|ui| {
            let v = ui.visuals_mut();
            v.widgets.inactive.weak_bg_fill = style::colors::BG_RAISED();
            v.widgets.hovered.weak_bg_fill = style::colors::BG_ACTIVE();
            v.widgets.open.weak_bg_fill = style::colors::BG_ACTIVE();
            v.override_text_color = Some(style::colors::TEXT());
            egui::ComboBox::from_id_salt("runconfig-selector")
                .width(180.0)
                .selected_text(egui::RichText::new(current).size(12.0))
                .show_ui(ui, |ui| {
                    ui.set_min_width(180.0);
                    for i in 0..self.configs.len() {
                        let name = self.configs[i].name.clone();
                        if ui.selectable_label(i == self.selected, name).clicked_by(egui::PointerButton::Primary)
                            && i != self.selected {
                                self.selected = i;
                                changed = true;
                                self.save(root);
                            }
                    }
                    if !self.configs.is_empty() {
                        style::hairline(ui);
                    }
                    if ui
                        .selectable_label(false, egui::RichText::new("Edit…").color(style::colors::TEXT_MUTED()))
                        .clicked_by(egui::PointerButton::Primary)
                    {
                        self.edit_open = true;
                    }
                });
        });

        if self.edit_open {
            self.edit_window(ui.ctx(), root);
        }
        changed
    }

    fn edit_window(&mut self, ctx: &egui::Context, root: &Path) {
        let mut open = self.edit_open;
        let mut dirty = false;
        egui::Window::new("Run Configurations")
            .open(&mut open)
            .default_width(420.0)
            .collapsible(false)
            .frame(
                egui::Frame::window(&ctx.style())
                    .fill(style::colors::BG_OVERLAY())
                    .stroke(egui::Stroke::new(1.0, style::colors::BORDER())),
            )
            .show(ctx, |ui| {
                style::panel_header_inline(ui, "configurations");
                // list + add/remove
                let mut remove: Option<usize> = None;
                for i in 0..self.configs.len() {
                    ui.horizontal(|ui| {
                        if ui
                            .selectable_label(i == self.selected, &self.configs[i].name)
                            .clicked_by(egui::PointerButton::Primary)
                        {
                            self.selected = i;
                            dirty = true;
                        }
                        if style::tool_button(ui, "✕", false).clicked_by(egui::PointerButton::Primary) {
                            remove = Some(i);
                        }
                    });
                }
                if let Some(i) = remove {
                    self.configs.remove(i);
                    if self.selected >= self.configs.len() {
                        self.selected = self.configs.len().saturating_sub(1);
                    }
                    dirty = true;
                }
                if style::tool_button(ui, "+ add", false).clicked_by(egui::PointerButton::Primary) {
                    self.configs.push(RunConfig::new("new config", RunKind::Custom, ""));
                    self.selected = self.configs.len() - 1;
                    dirty = true;
                }
                style::hairline(ui);

                // editor for the selected config
                let sel = self.selected;
                if let Some(cfg) = self.configs.get_mut(sel) {
                    egui::Grid::new("runconfig-edit-grid").num_columns(2).show(ui, |ui| {
                        ui.label(egui::RichText::new("name").color(style::colors::TEXT_MUTED()));
                        dirty |= ui.text_edit_singleline(&mut cfg.name).changed();
                        ui.end_row();

                        ui.label(egui::RichText::new("kind").color(style::colors::TEXT_MUTED()));
                        egui::ComboBox::from_id_salt("runconfig-kind")
                            .selected_text(cfg.kind.label())
                            .show_ui(ui, |ui| {
                                for k in RunKind::ALL {
                                    let lbl = k.label();
                                    if ui.selectable_label(cfg.kind == k, lbl).clicked_by(egui::PointerButton::Primary) {
                                        cfg.kind = k;
                                        dirty = true;
                                    }
                                }
                            });
                        ui.end_row();

                        ui.label(egui::RichText::new("program").color(style::colors::TEXT_MUTED()));
                        dirty |= ui.text_edit_singleline(&mut cfg.program).changed();
                        ui.end_row();

                        ui.label(egui::RichText::new("cwd").color(style::colors::TEXT_MUTED()));
                        let mut cwd = cfg.cwd.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
                        if ui.text_edit_singleline(&mut cwd).changed() {
                            cfg.cwd = if cwd.trim().is_empty() { None } else { Some(PathBuf::from(cwd)) };
                            dirty = true;
                        }
                        ui.end_row();
                    });

                    // args
                    ui.label(egui::RichText::new("ARGS").size(10.0).color(style::colors::TEXT_FAINT()));
                    let mut rm_arg: Option<usize> = None;
                    for (i, a) in cfg.args.iter().enumerate() {
                        ui.horizontal(|ui| {
                            ui.label(a);
                            if style::tool_button(ui, "✕", false).clicked_by(egui::PointerButton::Primary) {
                                rm_arg = Some(i);
                            }
                        });
                    }
                    if let Some(i) = rm_arg {
                        cfg.args.remove(i);
                        dirty = true;
                    }
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.edit_arg_draft)
                                .hint_text("new arg")
                                .desired_width(160.0),
                        );
                        if style::tool_button(ui, "+", false).clicked_by(egui::PointerButton::Primary) && !self.edit_arg_draft.trim().is_empty() {
                            cfg.args.push(self.edit_arg_draft.trim().to_string());
                            self.edit_arg_draft.clear();
                            dirty = true;
                        }
                    });

                    // env
                    ui.label(egui::RichText::new("ENV").size(10.0).color(style::colors::TEXT_FAINT()));
                    let mut rm_env: Option<usize> = None;
                    for (i, (k, v)) in cfg.env.iter().enumerate() {
                        ui.horizontal(|ui| {
                            ui.label(format!("{k}={v}"));
                            if style::tool_button(ui, "✕", false).clicked_by(egui::PointerButton::Primary) {
                                rm_env = Some(i);
                            }
                        });
                    }
                    if let Some(i) = rm_env {
                        cfg.env.remove(i);
                        dirty = true;
                    }
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.edit_env_draft)
                                .hint_text("KEY=VALUE")
                                .desired_width(160.0),
                        );
                        if style::tool_button(ui, "+", false).clicked_by(egui::PointerButton::Primary) {
                            if let Some((k, v)) = self.edit_env_draft.split_once('=') {
                                if !k.trim().is_empty() {
                                    cfg.env.push((k.trim().to_string(), v.trim().to_string()));
                                    self.edit_env_draft.clear();
                                    dirty = true;
                                }
                            }
                        }
                    });
                }
            });
        self.edit_open = open;
        if dirty {
            self.save(root);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cauldron-runconfig-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    use crate::newproject::{self, Template};

    /// THE contract for a freshly created project: open it, hit Run, and the thing runs — with no
    /// visit to the Run Configurations window. `selected` defaults to 0, so this asserts the FIRST
    /// config `detect()` produces for each template is the one that launches the entry point.
    #[test]
    fn every_template_auto_detects_a_run_config() {
        let base = tmpdir("templates");
        // (template, expected first config name)
        let expect: &[(Template, &str)] = &[
            (Template::CargoBin, "cargo run"),
            // A lib crate has no bin: `cargo run` would only ever error, so testing IS running it.
            (Template::CargoLib, "cargo test"),
            (Template::RustWgpu, "cargo run"),
            (Template::RuneApp, "cargo run"),
            (Template::RuneCompute, "cargo run"),
            // `make` alone builds; the templates ship a `run:` target and detect must prefer it.
            (Template::C, "make run"),
            (Template::CFlight, "make run"),
            (Template::Cpp, "make run"),
            (Template::Python, "python main.py"),
            (Template::Node, "npm start"),
            (Template::JavaScript, "npm start"),
            // TypeScript must COMPILE before it launches, or it runs a missing dist/.
            (Template::TypeScript, "npm build + start"),
            (Template::Html, "open index.html"),
        ];
        for (t, want) in expect {
            let dir = base.join(format!("{t:?}"));
            newproject::create_project(&dir, *t).unwrap();
            let got = detect(&dir);
            let first = got.first().unwrap_or_else(|| panic!("{t:?}: detect found NOTHING"));
            assert_eq!(&first.name, want, "{t:?} must lead with {want}, got {got:?}");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// …and the detected command must actually RUN. Builds each template's first config into a real
    /// command line, executes it, and asserts the program's own output comes back — the difference
    /// between "a config exists" and "hitting Run works". Only the templates that terminate on
    /// their own and need no network are executed (no GUI/wgpu, no npm install, no browser).
    #[test]
    fn detected_configs_actually_run_the_entry_point() {
        let base = tmpdir("run-e2e");
        for (t, tool) in [
            (Template::C, "make"),
            (Template::Cpp, "make"),
            (Template::CargoBin, "cargo"),
            (Template::Python, "python3"),
            (Template::Node, "node"),
        ] {
            if std::process::Command::new(tool).arg("--version").output().is_err() {
                eprintln!("skipped {t:?}: no {tool}");
                continue;
            }
            let dir = base.join(format!("{t:?}"));
            newproject::create_project(&dir, t).unwrap();
            let cfg = detect(&dir).into_iter().next().expect("a config");
            let (prog, args, cwd) = command_line(&cfg, &dir);
            let out = std::process::Command::new(&prog)
                .args(&args)
                .current_dir(&cwd)
                .output()
                .unwrap_or_else(|e| panic!("{t:?}: spawn {prog} failed: {e}"));
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(
                out.status.success(),
                "{t:?}: `{prog} {args:?}` failed\nstdout: {stdout}\nstderr: {stderr}"
            );
            // Every one of these entry points greets (cargo's own template capitalizes it).
            assert!(
                stdout.to_lowercase().contains("hello"),
                "{t:?}: the entry point must actually print\nstdout: {stdout}\nstderr: {stderr}"
            );
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The python config must use the project's OWN interpreter — the venv every new project
    /// ships. Bare `python3` cannot import anything the user pip-installed (PEP 668 refuses the
    /// system interpreter), so a run that reaches for it fails on the first `import`.
    #[test]
    fn python_runs_through_the_project_venv() {
        let root = tmpdir("venv-run");
        let cfg = RunConfig::new("python main.py", RunKind::Python, "main.py");

        // No venv (every pre-existing project): unchanged behavior, the system interpreter.
        assert_eq!(command_line(&cfg, &root).0, "python3");

        // With one, the venv's interpreter — by absolute path, because a spawned process never
        // has the venv "activated" and $PATH would still find the system python.
        std::fs::create_dir_all(root.join(".venv/bin")).unwrap();
        std::fs::write(root.join(".venv/bin/python"), "#!/bin/sh\n").unwrap();
        let (prog, args, _) = command_line(&cfg, &root);
        assert_eq!(prog, root.join(".venv/bin/python").to_string_lossy());
        assert_eq!(args, vec!["main.py"]);

        // Pytest and uvicorn ride the same interpreter (`-m`), for the same reason.
        assert_eq!(command_line(&RunConfig::new("t", RunKind::Pytest, ""), &root).0, prog);
        let uv = command_line(&RunConfig::new("u", RunKind::Uvicorn, "app:app"), &root);
        assert_eq!(uv.0, prog);
        assert_eq!(uv.1, vec!["-m", "uvicorn", "app:app"], "the venv's uvicorn, not $PATH's");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `make run` and plain `make` must SURVIVE each other through the merge. dedup_key is
    /// (kind, program) — so the target has to live in `program`; carrying it in `args` would make
    /// the two configs identical under dedup and one would be silently dropped on any project that
    /// already had the other.
    #[test]
    fn make_run_and_make_are_distinct_configs() {
        let root = tmpdir("make-dedup");
        std::fs::write(root.join("Makefile"), "all: build\n\nrun: all\n\t./app\n").unwrap();
        let found = detect(&root);
        let names: Vec<&str> = found.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["make run", "make"], "run target leads");
        assert_eq!(command_line(&found[0], &root).1, vec!["run"]);
        assert!(command_line(&found[1], &root).1.is_empty(), "bare make = default goal");

        // The exact regression: a project that already knows `make` still gains `make run`.
        let mut store = RunConfigStore::default();
        store.configs.push(RunConfig::new("make", RunKind::Make, ""));
        assert!(store.merge_detected(found), "make run must not be deduped away");
        assert_eq!(store.configs.len(), 2);
        assert!(store.configs.iter().any(|c| c.name == "make run"));

        // A Makefile with no `run:` target must not sprout one.
        std::fs::write(root.join("Makefile"), "all:\n\tcc x.c\nrunner:\n\t./x\n").unwrap();
        let names: Vec<String> = detect(&root).into_iter().map(|c| c.name).collect();
        assert!(!names.contains(&"make run".to_string()), "no run target: {names:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The Makefile target scan must not be fooled by the several colon-bearing lines that are NOT
    /// rule heads — the false positive that made a broken `make run` the default Run config.
    #[test]
    fn make_target_scan_rejects_assignments_and_accepts_indentation() {
        let root = tmpdir("makescan");
        let has = |body: &str| {
            std::fs::write(root.join("Makefile"), body).unwrap();
            make_has_target(&root, "run")
        };

        // Real rule heads, in the forms GNU make accepts.
        assert!(has("run:\n\t./app\n"));
        assert!(has("all run: deps\n\t./app\n"), "multi-target head");
        assert!(has("  run:\n\t./app\n"), "space-indented head is a rule");
        assert!(has("run:: deps\n\t./app\n"), "double-colon rule");

        // NOT rule heads.
        assert!(!has("all:\n\t./x\n"), "no run target at all");
        assert!(
            !has("DOCKER = docker run --rm -v $(PWD):/src img\nall:\n\t$(DOCKER)\n"),
            "an assignment whose VALUE contains `run` and a colon is not a target"
        );
        assert!(!has("run := ./app\nall:\n\t$(run)\n"), "run := ... is an assignment");
        assert!(!has("run ?= ./app\nall:\n\t$(run)\n"), "run ?= ... is an assignment");
        assert!(!has("\trun: nested\n"), "a tab-indented line is a recipe, not a rule");
        assert!(!has("# run: not a real target\nall:\n\t./x\n"), "a comment is not a rule");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Run-current-file derives its command from the EXTENSION, compiling first where the language
    /// needs it, and refuses (visibly) when it has no idea.
    #[test]
    fn run_current_file_dispatches_on_extension() {
        let root = tmpdir("runfile");
        let file_cfg = |p: &Path| RunConfig::new("f", RunKind::File, p.to_string_lossy());

        // Interpreted: exec'd directly.
        let (prog, args, cwd) = command_line(&file_cfg(&root.join("s.py")), &root);
        assert_eq!(prog, "python3");
        assert_eq!(args, vec![root.join("s.py").to_string_lossy().into_owned()]);
        assert_eq!(cwd, root, "cwd is the PROJECT ROOT, so relative imports/paths resolve");
        assert_eq!(command_line(&file_cfg(&root.join("s.js")), &root).0, "node");
        assert_eq!(command_line(&file_cfg(&root.join("s.sh")), &root).0, "bash");

        // Compiled: one shell line that builds THEN runs, so a keystroke yields output.
        let (prog, args, _) = command_line(&file_cfg(&root.join("s.c")), &root);
        assert_eq!(prog, "sh");
        assert!(args[1].contains(" cc "), "{args:?}");
        assert!(args[1].contains("&& exec "), "compile must gate the run: {args:?}");
        assert!(command_line(&file_cfg(&root.join("s.cpp")), &root).1[1].contains(" c++ "));
        // The artifact never lands in world-writable /tmp, where a planted symlink at a guessable
        // name would be followed by `cc -o` and overwrite whatever it points at.
        assert!(!args[1].contains("-o '/tmp/"), "artifact must not live in /tmp: {args:?}");
        // Same stem, different projects -> different artifacts, so one never clobbers the other.
        let a = command_line(&file_cfg(Path::new("/p1/main.c")), &root).1[1].clone();
        let b = command_line(&file_cfg(Path::new("/p2/main.c")), &root).1[1].clone();
        assert_ne!(a, b, "artifact path must be keyed to the file's full path");

        // Rust inside a cargo project defers to cargo (via --manifest-path, so it runs from a
        // workspace-root cwd), which knows the dependencies; a stray .rs has no cargo target that
        // names it, so it is rustc'd standalone.
        std::fs::write(root.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        let manifest = root.join("Cargo.toml").to_string_lossy().into_owned();
        let main_rs = command_line(&file_cfg(&root.join("src/main.rs")), &root);
        assert_eq!(main_rs.0, "cargo");
        assert_eq!(main_rs.1, vec!["run", "--manifest-path", &manifest]);
        let ex = command_line(&file_cfg(&root.join("examples/demo.rs")), &root);
        assert_eq!(ex.1, vec!["run", "--manifest-path", &manifest, "--example", "demo"]);
        // Directory-style example examples/multi/main.rs -> --example multi (named for the dir).
        let exdir = command_line(&file_cfg(&root.join("examples/multi/main.rs")), &root);
        assert_eq!(exdir.1, vec!["run", "--manifest-path", &manifest, "--example", "multi"]);
        // src/bin/tool.rs -> --bin tool.
        let binf = command_line(&file_cfg(&root.join("src/bin/tool.rs")), &root);
        assert_eq!(binf.1, vec!["run", "--manifest-path", &manifest, "--bin", "tool"]);
        let stray = command_line(&file_cfg(&root.join("scratch/thing.rs")), &root);
        assert_eq!(stray.0, "sh");
        assert!(stray.1[1].contains(" rustc -o "), "{:?}", stray.1);

        // A WORKSPACE member: the file's OWN crate manifest wins, not the virtual root's — the
        // exact case (Cauldron itself) where the old root-anchored check fell through to a rustc
        // that could not resolve the crate's dependencies.
        let ws = tmpdir("runfile-ws");
        std::fs::write(ws.join("Cargo.toml"), "[workspace]\nmembers=[\"crates/app\"]\n").unwrap();
        std::fs::create_dir_all(ws.join("crates/app/src")).unwrap();
        std::fs::write(ws.join("crates/app/Cargo.toml"), "[package]\nname=\"app\"\n").unwrap();
        let mm = command_line(&file_cfg(&ws.join("crates/app/src/main.rs")), &ws);
        assert_eq!(mm.0, "cargo");
        assert_eq!(
            mm.1,
            vec!["run", "--manifest-path", &ws.join("crates/app/Cargo.toml").to_string_lossy().into_owned()],
            "a member binary runs against its OWN manifest"
        );

        // A stray src/main.rs directly under the VIRTUAL workspace root belongs to no package —
        // `cargo run --manifest-path <virtual>` would run some arbitrary member, so it must fall
        // through to standalone rustc instead.
        std::fs::create_dir_all(ws.join("src")).unwrap();
        let stray_ws = command_line(&file_cfg(&ws.join("src/main.rs")), &ws);
        assert_eq!(stray_ws.0, "sh", "a virtual-root src/main.rs is not a cargo target");
        assert!(stray_ws.1[1].contains(" rustc -o "), "{:?}", stray_ws.1);
        let _ = std::fs::remove_dir_all(&ws);

        // A file we cannot run says so and exits non-zero, rather than spawning something bogus.
        let unknown = command_line(&file_cfg(&root.join("notes.txt")), &root);
        assert_eq!(unknown.0, "sh");
        assert!(unknown.1[1].contains("no run command for notes.txt"), "{:?}", unknown.1);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A run whose COMPILE fails must fail — never silently exec the binary a previous, successful
    /// run left at the same artifact path. That is the worst failure a Run button has: you "fix" a
    /// bug, hit Run, and watch the old code's output.
    #[test]
    fn failed_compile_never_runs_the_previous_binary() {
        if std::process::Command::new("cc").arg("--version").output().is_err() {
            eprintln!("skipped: no cc");
            return;
        }
        let root = tmpdir("stale");
        let src = root.join("prog.c");
        let cfg = RunConfig::new("f", RunKind::File, src.to_string_lossy());
        let run = || {
            let (p, a, cwd) = command_line(&cfg, &root);
            std::process::Command::new(&p).args(&a).current_dir(&cwd).output().unwrap()
        };

        // 1. A good build runs and prints.
        std::fs::write(&src, "#include <stdio.h>\nint main(void){printf(\"v1\\n\");return 0;}\n")
            .unwrap();
        let first = run();
        assert!(first.status.success());
        assert!(String::from_utf8_lossy(&first.stdout).contains("v1"));

        // 2. Now the source no longer compiles. The run must FAIL, and must not print v1 — the
        //    artifact from step 1 is still sitting there, and `&&` + `rm -f` are what keep it out.
        std::fs::write(&src, "#include <stdio.h>\nint main(void){ this is not c }\n").unwrap();
        let second = run();
        assert!(!second.status.success(), "a broken compile must fail the run");
        let out = String::from_utf8_lossy(&second.stdout);
        assert!(!out.contains("v1"), "STALE binary was executed after a failed compile: {out}");
        assert!(
            String::from_utf8_lossy(&second.stderr).contains("error"),
            "the compiler's diagnostics must reach the Output pane"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Paths are shell-quoted: a space or a quote in the project path must not split the command
    /// or let anything be injected into the `sh -c` line.
    #[test]
    fn file_commands_quote_hostile_paths() {
        let root = PathBuf::from("/tmp/we ird'; touch /tmp/pwned; #");
        let cfg = RunConfig::new("f", RunKind::File, root.join("a b.c").to_string_lossy());
        let (_, args, _) = command_line(&cfg, &root);
        let script = &args[1];
        assert!(script.contains(r"'/tmp/we ird'\''; touch /tmp/pwned; #/a b.c'"), "{script}");
        // The injected command never becomes a command: it stays inside the quoted argument.
        assert!(!script.contains("; touch /tmp/pwned; #/a b.c'\n"), "{script}");
    }

    #[test]
    fn roundtrip_persistence() {
        let root = tmpdir("roundtrip");
        let mut s = RunConfigStore::default();
        s.configs.push(RunConfig {
            name: "my bin".into(),
            kind: RunKind::Binary,
            program: "/tmp/x/bin".into(),
            args: vec!["--fast".into()],
            cwd: Some(PathBuf::from("/tmp/x")),
            env: vec![("RUST_LOG".into(), "debug".into())],
        });
        s.configs.push(RunConfig::new("t", RunKind::CargoTest, ""));
        s.selected = 1;
        s.save(&root);
        assert!(root.join(".cauldron/runconfigs.json").exists());
        let l = RunConfigStore::load(&root);
        assert_eq!(l.configs, s.configs);
        assert_eq!(l.selected, 1);
        assert_eq!(l.selected().unwrap().kind, RunKind::CargoTest);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn load_missing_is_empty() {
        let root = tmpdir("missing");
        let s = RunConfigStore::load(&root);
        assert!(s.configs.is_empty());
        assert!(s.selected().is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn save_appends_git_exclude() {
        let root = tmpdir("gitexcl");
        std::fs::create_dir_all(root.join(".git/info")).unwrap();
        let s = RunConfigStore::default();
        s.save(&root);
        s.save(&root); // idempotent
        let excl = std::fs::read_to_string(root.join(".git/info/exclude")).unwrap();
        assert_eq!(excl.lines().filter(|l| l.trim() == ".cauldron/").count(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn git_exclude_handles_missing_trailing_newline() {
        let root = tmpdir("gitexcl-nonl");
        std::fs::create_dir_all(root.join(".git/info")).unwrap();
        std::fs::write(root.join(".git/info/exclude"), "foo").unwrap(); // no trailing \n
        RunConfigStore::default().save(&root);
        let excl = std::fs::read_to_string(root.join(".git/info/exclude")).unwrap();
        assert!(excl.lines().any(|l| l == "foo"));
        assert!(excl.lines().any(|l| l == ".cauldron/"));
        assert!(!excl.contains("foo.cauldron/"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn find_executables_skips_venv() {
        use std::os::unix::fs::PermissionsExt;
        let root = tmpdir("venv");
        let elf = |p: &Path| {
            let mut bytes = b"\x7fELF".to_vec();
            bytes.extend_from_slice(&[0u8; 20]);
            std::fs::write(p, bytes).unwrap();
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        std::fs::create_dir_all(root.join(".venv/bin")).unwrap();
        elf(&root.join(".venv/bin/python3"));
        std::fs::create_dir_all(root.join("build")).unwrap();
        elf(&root.join("build/app"));
        let found = find_executables(&root);
        assert_eq!(found, vec![root.join("build/app")]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_fixture_tree() {
        let root = tmpdir("detect");
        std::fs::write(root.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        std::fs::write(root.join("pytest.ini"), "[pytest]\n").unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(root.join("Makefile"), "all:\n\ttrue\n").unwrap();
        std::fs::write(root.join("app.py"), "from fastapi import FastAPI\napp = FastAPI()\n").unwrap();
        let cfgs = detect(&root);
        let kinds: Vec<&RunKind> = cfgs.iter().map(|c| &c.kind).collect();
        assert!(kinds.contains(&&RunKind::Cargo));
        assert!(kinds.contains(&&RunKind::CargoTest));
        assert!(kinds.contains(&&RunKind::Pytest));
        assert!(kinds.contains(&&RunKind::Make));
        let uv = cfgs.iter().find(|c| c.kind == RunKind::Uvicorn).expect("uvicorn detected");
        assert_eq!(uv.program, "app:app");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn command_line_per_kind() {
        let root = PathBuf::from("/ws/proj");
        let c = |kind, program: &str| RunConfig::new("x", kind, program);

        let (p, a, cwd) = command_line(&c(RunKind::Cargo, ""), &root);
        assert_eq!((p.as_str(), a, cwd), ("cargo", vec!["run".to_string()], root.clone()));

        let (p, a, _) = command_line(&c(RunKind::CargoTest, ""), &root);
        assert_eq!((p.as_str(), a[0].as_str()), ("cargo", "test"));

        let (p, _, cwd) = command_line(&c(RunKind::Binary, "/build/out/app"), &root);
        assert_eq!(p, "/build/out/app");
        assert_eq!(cwd, PathBuf::from("/build/out")); // binary parent dir

        let (p, a, _) = command_line(&c(RunKind::Python, "main.py"), &root);
        assert_eq!((p.as_str(), a[0].as_str()), ("python3", "main.py"));

        let (p, a, _) = command_line(&c(RunKind::Pytest, ""), &root);
        assert_eq!((p.as_str(), a[0].as_str(), a[1].as_str()), ("python3", "-m", "pytest"));

        // `python -m uvicorn`, not the bare `uvicorn` binary: a spawned process never has the
        // venv activated, so $PATH would find the SYSTEM uvicorn (or none at all) while the
        // interpreter finds the module the project actually installed.
        let (p, a, _) = command_line(&c(RunKind::Uvicorn, "app:app"), &root);
        assert_eq!((p.as_str(), a.as_slice()), ("python3", &["-m".to_string(), "uvicorn".to_string(), "app:app".to_string()][..]));

        let (p, _, _) = command_line(&c(RunKind::Make, ""), &root);
        assert_eq!(p, "make");

        let mut custom = c(RunKind::Custom, "/usr/bin/thing");
        custom.args = vec!["-v".into()];
        custom.cwd = Some(PathBuf::from("/elsewhere"));
        let (p, a, cwd) = command_line(&custom, &root);
        assert_eq!((p.as_str(), a[0].as_str(), cwd), ("/usr/bin/thing", "-v", PathBuf::from("/elsewhere")));
    }

    #[test]
    fn debug_target_per_kind() {
        let root = PathBuf::from("/ws/proj");
        let c = |kind, program: &str| RunConfig::new("x", kind, program);

        let (bin, _, cwd, ad) = debug_target(&c(RunKind::Cargo, ""), &root).unwrap();
        assert_eq!(bin, PathBuf::from("/ws/proj/target/debug/proj"));
        assert_eq!(cwd, root);
        assert_eq!(ad, DebugAdapter::Native);

        let (bin, _, cwd, ad) = debug_target(&c(RunKind::Binary, "/b/out/app"), &root).unwrap();
        assert_eq!(bin, PathBuf::from("/b/out/app"));
        assert_eq!(cwd, PathBuf::from("/b/out"));
        assert_eq!(ad, DebugAdapter::Native);

        let (_, _, _, ad) = debug_target(&c(RunKind::Python, "main.py"), &root).unwrap();
        assert_eq!(ad, DebugAdapter::Python);

        assert!(debug_target(&c(RunKind::CargoTest, ""), &root).is_none());
        assert!(debug_target(&c(RunKind::DotnetTest, ""), &root).is_none());
        assert!(debug_target(&c(RunKind::Make, ""), &root).is_none());
        assert!(debug_target(&c(RunKind::Custom, "x"), &root).is_none());

        // pytest/uvicorn debug as python modules (was an empty program → failed launch).
        let (bin, args, _, ad) = debug_target(&c(RunKind::Pytest, ""), &root).unwrap();
        assert_eq!(bin, PathBuf::from("-m:pytest"));
        assert!(args.is_empty());
        assert_eq!(ad, DebugAdapter::Python);
        let (bin, args, _, _) = debug_target(&c(RunKind::Uvicorn, "main:app"), &root).unwrap();
        assert_eq!(bin, PathBuf::from("-m:uvicorn"));
        assert_eq!(args, vec!["main:app".to_string()], "uvicorn app module leads args");
    }

    #[test]
    fn ensure_detected_idempotent() {
        let root = tmpdir("ensure");
        std::fs::write(root.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        let mut s = RunConfigStore::default();
        assert!(s.ensure_detected(&root));
        let n = s.configs.len();
        assert!(n >= 2);
        assert!(!s.ensure_detected(&root)); // second call adds nothing
        assert_eq!(s.configs.len(), n);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Write a minimal ELF executable at `p`.
    fn elf(p: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let mut bytes = b"\x7fELF".to_vec();
        bytes.extend_from_slice(&[0u8; 20]);
        std::fs::write(p, bytes).unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn merge_detected_never_clobbers_edits_or_selection() {
        let mut s = RunConfigStore::default();
        let mut edited = RunConfig::new("my renamed bin", RunKind::Binary, "/t/debug/app");
        edited.args = vec!["--fast".into()]; // user-added arg
        s.configs.push(RunConfig::new("cargo run", RunKind::Cargo, ""));
        s.configs.push(edited.clone());
        s.selected = 1; // the user picked their edited config
        // A background detect lands: a duplicate of the edited config (same kind+program,
        // detector-default name/args) plus a genuinely new one.
        let added = s.merge_detected(vec![
            RunConfig::new("app", RunKind::Binary, "/t/debug/app"),
            RunConfig::new("other", RunKind::Binary, "/t/debug/other"),
        ]);
        assert!(added);
        assert_eq!(s.configs.len(), 3); // dup skipped, new appended at the END
        assert_eq!(s.configs[0].kind, RunKind::Cargo); // order untouched
        assert_eq!(s.configs[1], edited); // user edits (name + args) intact
        assert_eq!(s.configs[2].name, "other");
        assert_eq!(s.selected, 1); // selection never moves
        // Idempotent re-merge adds nothing.
        assert!(!s.merge_detected(vec![RunConfig::new("app", RunKind::Binary, "/t/debug/app")]));
        assert_eq!(s.configs.len(), 3);
    }

    #[test]
    fn background_detect_round_trip() {
        let root = tmpdir("bgdetect");
        std::fs::write(root.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        let mut s = RunConfigStore::default();
        s.kick_detect(&root, || {});
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut added = false;
        while !added && std::time::Instant::now() < deadline {
            added = s.poll_detect();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(added, "worker result never arrived");
        let kinds: Vec<&RunKind> = s.configs.iter().map(|c| &c.kind).collect();
        assert!(kinds.contains(&&RunKind::Cargo));
        assert!(kinds.contains(&&RunKind::CargoTest));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn poll_detect_drops_orphaned_generation() {
        let mut s = RunConfigStore::default();
        // A result stamped gen 1 is in flight...
        s.detect_tx.send((1, vec![RunConfig::new("stale", RunKind::Binary, "/old/bin")])).unwrap();
        // ...but a re-kick has since moved the store to gen 2.
        s.detect_gen = 2;
        assert!(!s.poll_detect());
        assert!(s.configs.is_empty());
        // A current-generation result still lands.
        s.detect_tx.send((2, vec![RunConfig::new("fresh", RunKind::Binary, "/new/bin")])).unwrap();
        assert!(s.poll_detect());
        assert_eq!(s.configs.len(), 1);
        assert_eq!(s.configs[0].name, "fresh");
    }

    #[test]
    fn find_executables_only_walks_build_dirs() {
        let root = tmpdir("bounded");
        // ELFs OUTSIDE conventional build dirs are never found (the walk is bounded now).
        std::fs::create_dir_all(root.join("src")).unwrap();
        elf(&root.join("src/stray"));
        elf(&root.join("loose"));
        // ELFs in each conventional location ARE found.
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        elf(&root.join("target/debug/app"));
        std::fs::create_dir_all(root.join("target/x86_64-unknown-linux-gnu/deps")).unwrap();
        elf(&root.join("target/x86_64-unknown-linux-gnu/deps/cross"));
        std::fs::create_dir_all(root.join("cmake-build-debug")).unwrap();
        elf(&root.join("cmake-build-debug/capp"));
        let found = find_executables(&root);
        assert!(found.contains(&root.join("target/debug/app")));
        assert!(found.contains(&root.join("target/x86_64-unknown-linux-gnu/deps/cross")));
        assert!(found.contains(&root.join("cmake-build-debug/capp")));
        assert!(!found.iter().any(|p| p.ends_with("stray") || p.ends_with("loose")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn find_executables_skips_cargo_chaff_dirs() {
        let root = tmpdir("chaff");
        std::fs::create_dir_all(root.join("target/debug/incremental/x")).unwrap();
        std::fs::create_dir_all(root.join("target/debug/.fingerprint/y")).unwrap();
        elf(&root.join("target/debug/incremental/x/junk"));
        elf(&root.join("target/debug/.fingerprint/y/junk2"));
        elf(&root.join("target/debug/real"));
        let found = find_executables(&root);
        assert_eq!(found, vec![root.join("target/debug/real")]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn find_executables_entry_fuse_stops_walk() {
        let root = tmpdir("fuse");
        std::fs::create_dir_all(root.join("build")).unwrap();
        for i in 0..20 {
            elf(&root.join(format!("build/bin{i:02}")));
        }
        // Unbounded finds plenty (capped at 5 by ranking)…
        assert_eq!(find_executables_bounded(&root, u64::MAX).len(), 5);
        // …but a tiny fuse stops the walk early: at most 4 entries examined (the build dir
        // itself + 4 files, one over the fuse), so at most 4 results.
        let fused = find_executables_bounded(&root, 5);
        assert!(fused.len() <= 4, "fuse did not bound the walk: {fused:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn fresh_binary_config_short_circuits_walk() {
        let root = tmpdir("shortcircuit");
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        elf(&root.join("target/debug/app"));
        // No saved config yet → the walk runs.
        assert_eq!(find_executables(&root), vec![root.join("target/debug/app")]);
        // Save a store holding that Binary config: runconfigs.json is now NEWER than the
        // build dir, so detection short-circuits (no ELF opens at all). Sleep past the fs
        // timestamp granularity so "newer" is strict (real saves trail builds by seconds).
        std::thread::sleep(std::time::Duration::from_millis(30));
        let mut s = RunConfigStore::default();
        s.configs.push(RunConfig::new("app", RunKind::Binary, root.join("target/debug/app").to_string_lossy().into_owned()));
        s.save(&root);
        assert!(fresh_binary_config(&root, &build_dirs(&root)));
        assert_eq!(find_executables(&root), Vec::<PathBuf>::new());
        // A new build output bumps the dir mtime → the short-circuit lifts.
        elf(&root.join("target/debug/app2"));
        assert!(!fresh_binary_config(&root, &build_dirs(&root)));
        let found = find_executables(&root);
        assert!(found.contains(&root.join("target/debug/app2")));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// POSIX only bumps the IMMEDIATE parent dir's mtime on file creation: a binary landing
    /// in a NESTED build subdir (CMake's `build/bin/`, cargo's `target/debug/examples/`)
    /// must still lift the short-circuit, or it never appears as a run-config suggestion —
    /// and every run-config edit re-saves the json, re-freshening the stale stamp.
    #[test]
    fn fresh_binary_config_sees_nested_build_outputs() {
        let root = tmpdir("nested-bins");
        std::fs::create_dir_all(root.join("build/bin")).unwrap();
        elf(&root.join("build/bin/app"));
        assert_eq!(find_executables(&root), vec![root.join("build/bin/app")]);
        std::thread::sleep(std::time::Duration::from_millis(30));
        let mut s = RunConfigStore::default();
        s.configs.push(RunConfig::new(
            "app",
            RunKind::Binary,
            root.join("build/bin/app").to_string_lossy().into_owned(),
        ));
        s.save(&root);
        assert!(fresh_binary_config(&root, &build_dirs(&root)), "json newer than all dirs");
        std::thread::sleep(std::time::Duration::from_millis(30));
        // New binary in the NESTED dir: bumps build/bin's mtime, NOT build's — the old
        // top-level-only stat stayed "fresh" forever here.
        elf(&root.join("build/bin/app2"));
        assert!(
            !fresh_binary_config(&root, &build_dirs(&root)),
            "nested output must lift the short-circuit"
        );
        let found = find_executables(&root);
        assert!(found.contains(&root.join("build/bin/app2")), "{found:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn fastapi_walk_gated_on_root_markers() {
        // FastAPI code buried in a subdir WITHOUT any python root marker: gate says no walk.
        let root = tmpdir("fastapi-gate");
        std::fs::create_dir_all(root.join("svc")).unwrap();
        std::fs::write(root.join("svc/app.py"), "from fastapi import FastAPI\napp = FastAPI()\n").unwrap();
        let cfgs = detect(&root);
        assert!(!cfgs.iter().any(|c| c.kind == RunKind::Uvicorn));
        // Add a root marker (requirements.txt) → the walk runs and finds it.
        std::fs::write(root.join("requirements.txt"), "fastapi\n").unwrap();
        let cfgs = detect(&root);
        let uv = cfgs.iter().find(|c| c.kind == RunKind::Uvicorn).expect("uvicorn detected");
        assert_eq!(uv.program, "svc.app:app");
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod single_file_debug_tests {
    use super::*;

    fn script_for(name: &str) -> String {
        let cfg = RunConfig {
            name: "t".into(),
            kind: RunKind::File,
            program: format!("/tmp/{name}"),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
        };
        let (prog, args, _) = command_line(&cfg, Path::new("/tmp"));
        assert_eq!(prog, "sh");
        args.get(1).cloned().unwrap_or_default()
    }

    /// The artifact a single-file Run produces is the same one Debug attaches to, so it must
    /// carry debug info. Without -g there is no line table and lldb cannot bind a breakpoint.
    #[test]
    fn single_file_c_compiles_with_debug_info() {
        let s = script_for("x.c");
        assert!(s.contains(" -g "), "C compile line lacks -g: {s}");
        assert!(s.contains(" -O0 "), "C compile line lacks -O0: {s}");
    }

    #[test]
    fn single_file_cpp_compiles_with_debug_info() {
        let s = script_for("x.cpp");
        assert!(s.contains(" -g "), "C++ compile line lacks -g: {s}");
        assert!(s.contains(" -O0 "), "C++ compile line lacks -O0: {s}");
    }

    /// The artifact path must be STABLE for the same source: a debugger is pointed at it after
    /// the build, and a randomized name would leave nothing to attach to.
    #[test]
    fn run_artifact_is_deterministic() {
        let a = run_artifact(Path::new("/tmp/x.c"), "x");
        let b = run_artifact(Path::new("/tmp/x.c"), "x");
        assert_eq!(a, b);
        assert_ne!(a, run_artifact(Path::new("/other/x.c"), "x"), "distinct sources, distinct artifacts");
    }
}
