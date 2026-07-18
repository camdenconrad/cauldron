//! TEST RUNNER — spawn `cargo test` / `pytest -v` / `ctest` and parse the stream into an
//! interactive pass/fail tree. Same worker shape as runner.rs: reader threads + mpsc +
//! request_repaint, no async. The parser is a pure `feed_line` state machine so the unit
//! tests below drive it with canned transcripts and never spawn anything.

#![allow(dead_code)] // integrator wires this up; until then the whole surface is "unused"

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Instant;

use crate::style::{self, colors};

// =================================================================================================
// data model
// =================================================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framework {
    CargoTest,
    Pytest,
    Ctest,
    DotnetTest,
}

impl Framework {
    /// Sniff the project root: Cargo.toml → cargo, pytest markers → pytest,
    /// CTestTestfile.cmake in a build*/ dir → ctest. Defaults to cargo.
    pub fn detect(root: &Path) -> Framework {
        if root.join("Cargo.toml").is_file() {
            return Framework::CargoTest;
        }
        if root.join("pytest.ini").is_file()
            || root.join("pyproject.toml").is_file()
            || has_py_tests(&root.join("tests"))
        {
            return Framework::Pytest;
        }
        if newest_ctest_build_dir(root).is_some() {
            return Framework::Ctest;
        }
        if has_dotnet_project(root) {
            return Framework::DotnetTest;
        }
        Framework::CargoTest
    }
}

/// A `.sln`/`.csproj`/`.fsproj`/`.vbproj` at the project root marks a .NET tree.
fn has_dotnet_project(root: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(root) else { return false };
    rd.flatten().any(|e| {
        e.path()
            .extension()
            .and_then(|x| x.to_str())
            .is_some_and(|x| matches!(x, "sln" | "csproj" | "fsproj" | "vbproj"))
    })
}

fn has_py_tests(dir: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else { return false };
    rd.flatten().any(|e| {
        e.path().extension().map(|x| x == "py").unwrap_or(false)
    })
}

/// The newest `build*` directory under `root` containing a CTestTestfile.cmake.
fn newest_ctest_build_dir(root: &Path) -> Option<PathBuf> {
    let rd = std::fs::read_dir(root).ok()?;
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name();
        let name = name.to_string_lossy();
        if !p.is_dir() || !name.starts_with("build") {
            continue;
        }
        if !p.join("CTestTestfile.cmake").is_file() {
            continue;
        }
        let mtime = e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
        if best.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
            best = Some((mtime, p));
        }
    }
    best.map(|(_, p)| p)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestStatus {
    Running,
    Pass,
    Fail,
    Ignored,
}

#[derive(Debug, Clone)]
pub struct TestCase {
    pub name: String,
    pub status: TestStatus,
    pub duration: Option<f32>,
    pub failure: String,
    pub location: Option<(PathBuf, usize)>,
}

#[derive(Debug, Clone)]
pub struct Suite {
    pub name: String,
    pub tests: Vec<TestCase>,
}

// =================================================================================================
// pure parser — feed_line drives everything; the tests below exercise it directly
// =================================================================================================

/// What a multi-line section is currently being captured into.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Capture {
    None,
    /// Inside a cargo `---- name stdout ----` failure body for the named test.
    CargoFailure(String),
    /// Saw the pytest `== FAILURES ==` banner; `current` is the test the body belongs to.
    PytestFailures { current: Option<String> },
    /// ctest --output-on-failure body following a `***Failed` line for the named test.
    CtestFailure(String),
    /// `dotnet test` detailed-logger error body following a `Failed <name>` line.
    DotnetFailure(String),
}

#[derive(Debug)]
pub struct ParseState {
    pub framework: Framework,
    pub tree: Vec<Suite>,
    capture: Capture,
}

impl ParseState {
    pub fn new(framework: Framework) -> Self {
        Self { framework, tree: Vec::new(), capture: Capture::None }
    }

    fn suite_mut(&mut self, name: &str) -> &mut Suite {
        if let Some(i) = self.tree.iter().position(|s| s.name == name) {
            return &mut self.tree[i];
        }
        self.tree.push(Suite { name: name.to_string(), tests: Vec::new() });
        self.tree.last_mut().unwrap()
    }

    fn find_test_mut(&mut self, test: &str) -> Option<&mut TestCase> {
        self.tree
            .iter_mut()
            .flat_map(|s| s.tests.iter_mut())
            .find(|t| t.name == test)
    }
}

/// Incremental parser: feed one output line (stdout or stderr) at a time.
pub fn feed_line(state: &mut ParseState, line: &str) {
    match state.framework {
        Framework::CargoTest => feed_cargo(state, line),
        Framework::Pytest => feed_pytest(state, line),
        Framework::Ctest => feed_ctest(state, line),
        Framework::DotnetTest => feed_dotnet(state, line),
    }
}

// ---- cargo test ---------------------------------------------------------------------------------

fn feed_cargo(state: &mut ParseState, line: &str) {
    let t = line.trim();

    // failure-body capture ends on any new section marker
    if let Capture::CargoFailure(name) = state.capture.clone() {
        if t.starts_with("----") || t == "failures:" || t.starts_with("test result:") {
            state.capture = Capture::None;
            // fall through: the line may itself open a new capture / be a result line
        } else {
            if let Some(loc) = parse_panicked_at(t) {
                if let Some(tc) = state.find_test_mut(&name) {
                    if tc.location.is_none() {
                        tc.location = Some(loc);
                    }
                }
            }
            if let Some(tc) = state.find_test_mut(&name) {
                if !tc.failure.is_empty() {
                    tc.failure.push('\n');
                }
                tc.failure.push_str(line);
            }
            return;
        }
    }

    // suite boundaries: "Running unittests src/lib.rs (target/debug/deps/foo-1a2b)" and
    // "Doc-tests foo"
    if let Some(rest) = t.strip_prefix("Running ") {
        let name = if let (Some(op), Some(cp)) = (rest.rfind('('), rest.rfind(')')) {
            let bin = &rest[op + 1..cp];
            let stem = bin.rsplit('/').next().unwrap_or(bin);
            strip_cargo_hash(stem).to_string()
        } else {
            rest.to_string()
        };
        state.suite_mut(&name);
        return;
    }
    if let Some(rest) = t.strip_prefix("Doc-tests ") {
        state.suite_mut(&format!("doc-tests {rest}"));
        return;
    }

    // "---- name stdout ----" opens a failure body
    if t.starts_with("---- ") && t.ends_with(" ----") {
        let inner = &t[5..t.len() - 5];
        let name = inner.strip_suffix(" stdout").unwrap_or(inner).to_string();
        state.capture = Capture::CargoFailure(name);
        return;
    }

    // "test path::name ... ok/FAILED/ignored"
    if let Some(rest) = t.strip_prefix("test ") {
        if t.starts_with("test result:") {
            return;
        }
        if let Some((name, verdict)) = rest.split_once(" ... ") {
            let status = match verdict.split_whitespace().next().unwrap_or("") {
                "ok" => TestStatus::Pass,
                "FAILED" => TestStatus::Fail,
                "ignored" => TestStatus::Ignored,
                _ => return,
            };
            if state.tree.is_empty() {
                state.suite_mut("tests");
            }
            let suite = state.tree.last_mut().unwrap();
            suite.tests.push(TestCase {
                name: name.to_string(),
                status,
                duration: None,
                failure: String::new(),
                location: None,
            });
        }
    }
}

/// Strip cargo's trailing `-<hex hash>` from a test-binary stem.
fn strip_cargo_hash(stem: &str) -> &str {
    if let Some(i) = stem.rfind('-') {
        let tail = &stem[i + 1..];
        if tail.len() >= 8 && tail.chars().all(|c| c.is_ascii_hexdigit()) {
            return &stem[..i];
        }
    }
    stem
}

/// Extract `(path, line)` from both panic message shapes:
///   new:  `thread 'x' panicked at src/lib.rs:10:9:`
///   old:  `thread 'x' panicked at 'msg', src/lib.rs:10:9`
fn parse_panicked_at(line: &str) -> Option<(PathBuf, usize)> {
    let idx = line.find("panicked at ")?;
    let rest = &line[idx + "panicked at ".len()..];
    let loc = if let Some(r) = rest.strip_prefix('\'') {
        // old format: skip to `', `
        let end = r.find("', ")?;
        &r[end + 3..]
    } else {
        rest
    };
    parse_file_line(loc.trim_end_matches(':'))
}

/// Parse `path:line[:col...]` (path itself never contains ':' on unix).
fn parse_file_line(s: &str) -> Option<(PathBuf, usize)> {
    let mut parts = s.split(':');
    let path = parts.next()?;
    let line: usize = parts.next()?.trim().parse().ok()?;
    if path.is_empty() {
        return None;
    }
    Some((PathBuf::from(path), line))
}

// ---- pytest -v ----------------------------------------------------------------------------------

fn feed_pytest(state: &mut ParseState, line: &str) {
    let t = line.trim_end();
    let trimmed = t.trim();

    // FAILURES section handling
    if let Capture::PytestFailures { current } = state.capture.clone() {
        if trimmed.starts_with('=') && trimmed.contains("short test summary") {
            state.capture = Capture::None;
            return;
        }
        if trimmed.starts_with('=') && trimmed.ends_with('=') && trimmed.len() > 4 {
            // some other ==== banner ends the section
            state.capture = Capture::None;
            return;
        }
        if trimmed.starts_with('_') && trimmed.ends_with('_') {
            let name = trimmed.trim_matches('_').trim().to_string();
            if !name.is_empty() {
                state.capture = Capture::PytestFailures { current: Some(name) };
                return;
            }
        }
        if let Some(name) = current {
            // location lines look like "tests/test_x.py:12: AssertionError"
            if let Some(loc) = pytest_location(trimmed) {
                if let Some(tc) = find_pytest_test_mut(state, &name) {
                    if tc.location.is_none() {
                        tc.location = Some(loc);
                    }
                }
            }
            if let Some(tc) = find_pytest_test_mut(state, &name) {
                if !tc.failure.is_empty() {
                    tc.failure.push('\n');
                }
                tc.failure.push_str(t);
            }
        }
        return;
    }

    if trimmed.starts_with('=') && trimmed.contains("FAILURES") {
        state.capture = Capture::PytestFailures { current: None };
        return;
    }

    // "tests/test_a.py::test_b PASSED [ 12%]"
    if let Some((nodeid, rest)) = trimmed.split_once(' ') {
        if nodeid.contains("::") {
            let verdict = rest.split_whitespace().next().unwrap_or("");
            let status = match verdict {
                "PASSED" | "XPASS" => TestStatus::Pass,
                "FAILED" | "ERROR" | "XFAIL" => TestStatus::Fail,
                "SKIPPED" => TestStatus::Ignored,
                _ => return,
            };
            let (file, test) = nodeid.split_once("::").unwrap();
            let suite = state.suite_mut(file);
            suite.tests.push(TestCase {
                name: test.to_string(),
                status,
                duration: None,
                failure: String::new(),
                location: None,
            });
        }
    }
}

/// A pytest traceback location line: `path.py:NN: SomeError` (must start with the path).
fn pytest_location(line: &str) -> Option<(PathBuf, usize)> {
    let first = line.split(':').next()?;
    if !first.ends_with(".py") {
        return None;
    }
    parse_file_line(line)
}

/// pytest test names in the tree may be parametrized nodeids; match on the leaf name.
fn find_pytest_test_mut<'a>(state: &'a mut ParseState, name: &str) -> Option<&'a mut TestCase> {
    state
        .tree
        .iter_mut()
        .flat_map(|s| s.tests.iter_mut())
        .find(|t| t.name == name || t.name.starts_with(&format!("{name}[")))
}

// ---- ctest --------------------------------------------------------------------------------------

fn feed_ctest(state: &mut ParseState, line: &str) {
    let t = line.trim();

    // "1/3 Test #1: name ............   Passed    0.05 sec"
    let is_test_line = t
        .split_whitespace()
        .nth(1)
        .map(|w| w == "Test")
        .unwrap_or(false)
        && t.contains('#');
    if is_test_line {
        state.capture = Capture::None;
        // "***Failed" glues onto the dot leader ("....***Failed") — split it off.
        let t = t.replace("***", " ***");
        let toks: Vec<&str> = t.split_whitespace().collect();
        // toks: [N/M, Test, #k:, name..., ......, Verdict, dur, sec]
        let dots = toks
            .iter()
            .position(|w| w.len() > 2 && w.chars().all(|c| c == '.'))
            .unwrap_or(toks.len());
        if dots <= 3 || dots + 1 >= toks.len() {
            return;
        }
        let name = toks[3..dots].join(" ");
        let verdict = toks[dots + 1];
        let status = if verdict == "Passed" { TestStatus::Pass } else { TestStatus::Fail };
        let duration = toks
            .iter()
            .position(|w| *w == "sec")
            .and_then(|i| i.checked_sub(1))
            .and_then(|i| toks[i].parse::<f32>().ok());
        let suite = state.suite_mut("ctest");
        suite.tests.push(TestCase {
            name: name.clone(),
            status,
            duration,
            failure: String::new(),
            location: None,
        });
        if status == TestStatus::Fail {
            state.capture = Capture::CtestFailure(name);
        }
        return;
    }

    if let Capture::CtestFailure(name) = state.capture.clone() {
        // Bodies from --output-on-failure may contain blank lines; only real
        // section markers end the capture (the next test line ends it above).
        if t.starts_with("Start ") || t.contains("tests passed") || t.starts_with("Total Test time")
        {
            state.capture = Capture::None;
            return;
        }
        if t.is_empty() {
            return; // skip blanks, keep capturing
        }
        if let Some(tc) = state.find_test_mut(&name) {
            if !tc.failure.is_empty() {
                tc.failure.push('\n');
            }
            tc.failure.push_str(line);
        }
    }
}

// ---- dotnet test (VSTest console logger, verbosity=detailed) ------------------------------------

fn feed_dotnet(state: &mut ParseState, line: &str) {
    let t = line.trim();

    // Per-test result lines: "Passed <fqname> [12 ms]", "Failed <fqname> [3 ms]",
    // "Skipped <fqname>". The `!`-suffixed run summary ("Passed!  - Failed: 0, …") does NOT
    // match these `<word> ` prefixes, so it falls through harmlessly.
    for (kw, status) in [
        ("Passed ", TestStatus::Pass),
        ("Failed ", TestStatus::Fail),
        ("Skipped ", TestStatus::Ignored),
    ] {
        if let Some(rest) = t.strip_prefix(kw) {
            state.capture = Capture::None;
            // The fqname may itself contain spaces (data-driven cases: "T(x: 1, y: 2)"), so the
            // name is everything up to the trailing " [<dur>]" bracket, not a whitespace split.
            let (name, duration) = match rest.rfind(" [") {
                Some(i) => {
                    let dur = rest[i + 2..]
                        .trim_end_matches(']')
                        .split_whitespace()
                        .next()
                        .and_then(|n| n.parse::<f32>().ok())
                        .map(|ms| ms / 1000.0); // logger reports ms; the panel wants seconds
                    (rest[..i].trim().to_string(), dur)
                }
                None => (rest.trim().to_string(), None),
            };
            if name.is_empty() {
                return;
            }
            // Group by the fqname's namespace+class (everything before the last '.').
            let suite_name = name.rsplit_once('.').map(|(s, _)| s).unwrap_or("dotnet").to_string();
            let suite = state.suite_mut(&suite_name);
            suite.tests.push(TestCase {
                name: name.clone(),
                status,
                duration,
                failure: String::new(),
                location: None,
            });
            if status == TestStatus::Fail {
                state.capture = Capture::DotnetFailure(name);
            }
            return;
        }
    }

    // Failure body: the "Error Message:" / "Stack Trace:" block the detailed logger prints under
    // a failed test, until the next result line (handled above) or the run summary.
    if let Capture::DotnetFailure(name) = state.capture.clone() {
        if t.starts_with("Passed!") || t.starts_with("Failed!") || t.starts_with("Test Run ") {
            state.capture = Capture::None;
            return;
        }
        // "... in /path/File.cs:line 42" → jump target.
        if let Some(loc) = parse_dotnet_location(t) {
            if let Some(tc) = state.find_test_mut(&name) {
                if tc.location.is_none() {
                    tc.location = Some(loc);
                }
            }
        }
        if t.is_empty() {
            return;
        }
        if let Some(tc) = state.find_test_mut(&name) {
            if !tc.failure.is_empty() {
                tc.failure.push('\n');
            }
            tc.failure.push_str(line);
        }
    }
}

/// Parse a .NET stack-trace frame's source ref: `… in <file>:line <n>` → (file, line).
fn parse_dotnet_location(t: &str) -> Option<(PathBuf, usize)> {
    let idx = t.rfind(" in ")?;
    let tail = &t[idx + 4..];
    let (file, rest) = tail.rsplit_once(":line ")?;
    let line = rest.split_whitespace().next()?.parse::<usize>().ok()?;
    Some((PathBuf::from(file.trim()), line))
}

// ---- gutter test detection ----------------------------------------------------------------------

/// Find test declarations in a buffer: `(0-based line of the fn/def, test name)`.
/// Language by extension: Rust `#[test]`-family attributes (incl. tokio/rstest) with the fn on a
/// following line; Python `def test_*`; C# xUnit/NUnit/MSTest attributes with the method on a
/// following line. Pure text scan — no tree-sitter needed for declaration lines.
pub fn find_test_decls(text: &str, ext: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    match ext {
        "rs" => {
            let mut armed = false;
            for (i, raw) in lines.iter().enumerate() {
                let t = raw.trim_start();
                if t.starts_with("#[test]")
                    || t.starts_with("#[tokio::test")
                    || t.starts_with("#[rstest")
                    || t.starts_with("#[test_case")
                {
                    // Single-line form: `#[test] fn name() {}` declares on the SAME line.
                    if let Some(close) = t.find(']') {
                        if let Some(name) = fn_name_on(t[close + 1..].trim_start(), "fn ") {
                            out.push((i, name));
                            continue;
                        }
                    }
                    armed = true;
                    continue;
                }
                if armed {
                    // Attributes / doc lines may sit between the marker and the fn.
                    if t.starts_with("#[") || t.starts_with("//") || t.is_empty() {
                        continue;
                    }
                    if let Some(name) = fn_name_on(t, "fn ") {
                        out.push((i, name));
                    }
                    armed = false;
                }
            }
        }
        "py" => {
            for (i, raw) in lines.iter().enumerate() {
                let t = raw.trim_start();
                for kw in ["def test_", "async def test_"] {
                    if let Some(rest) = t.strip_prefix(kw) {
                        let name_tail: String = rest
                            .chars()
                            .take_while(|c| c.is_alphanumeric() || *c == '_')
                            .collect();
                        out.push((i, format!("test_{name_tail}")));
                        break;
                    }
                }
            }
        }
        "cs" => {
            let mut armed = false;
            for (i, raw) in lines.iter().enumerate() {
                let t = raw.trim_start();
                if t.starts_with("[Fact")
                    || t.starts_with("[Theory")
                    || t.starts_with("[Test]")
                    || t.starts_with("[Test(")
                    || t.starts_with("[TestMethod")
                {
                    // Single-line form: `[Fact] public void X() {}`.
                    if let Some(close) = t.find(']') {
                        let rest = t[close + 1..].trim_start();
                        if let Some(open) = rest.find('(') {
                            if let Some(name) = rest[..open].split_whitespace().last() {
                                if !name.is_empty()
                                    && name.chars().all(|c| c.is_alphanumeric() || c == '_')
                                {
                                    out.push((i, name.to_string()));
                                    continue;
                                }
                            }
                        }
                    }
                    armed = true;
                    continue;
                }
                if armed {
                    if t.starts_with('[') || t.is_empty() || t.starts_with("//") {
                        continue;
                    }
                    // "public async Task Name(" / "public void Name(" — name = token before '('.
                    if let Some(open) = t.find('(') {
                        let head = &t[..open];
                        if let Some(name) = head.split_whitespace().last() {
                            if name.chars().all(|c| c.is_alphanumeric() || c == '_')
                                && !name.is_empty()
                            {
                                out.push((i, name.to_string()));
                            }
                        }
                    }
                    armed = false;
                }
            }
        }
        _ => {}
    }
    out
}

/// `fn NAME(` after any qualifiers (`pub`, `async`, `unsafe`, `const`).
fn fn_name_on(t: &str, kw: &str) -> Option<String> {
    let idx = t.find(kw)?;
    // Only accept when everything before `fn` is qualifiers — avoids matching `let fnx = …`.
    let before = &t[..idx];
    if !before
        .split_whitespace()
        .all(|w| matches!(w, "pub" | "async" | "unsafe" | "const" | "extern" | "\"C\""))
    {
        return None;
    }
    let rest = &t[idx + kw.len()..];
    let name: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
    (!name.is_empty()).then_some(name)
}

// =================================================================================================
// TestRunner — child management + streaming, ui
// =================================================================================================

/// Events are tagged with the run generation that produced them so that a
/// killed run's still-draining reader/waiter threads can't pollute the next run.
enum TestEvent {
    Line(u64, String),
    Done(u64),
}

pub struct TestRunner {
    rx: Receiver<TestEvent>,
    tx: Sender<TestEvent>,
    child: Option<Arc<Mutex<Child>>>,
    state: ParseState,
    pub running: bool,
    framework: Framework,
    root: PathBuf,
    /// Monotonic run id; stale events from killed runs are dropped in pump().
    generation: u64,
    started: Option<Instant>,
    /// Wall-clock of the last finished run.
    elapsed: f32,
}

impl Default for TestRunner {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            rx,
            tx,
            child: None,
            state: ParseState::new(Framework::CargoTest),
            running: false,
            framework: Framework::CargoTest,
            root: PathBuf::new(),
            generation: 0,
            started: None,
            elapsed: 0.0,
        }
    }
}

impl TestRunner {
    /// The parsed suite tree.
    pub fn tree(&self) -> &[Suite] {
        &self.state.tree
    }

    /// Detect the framework for `root` and run the whole test suite.
    pub fn start(&mut self, root: &Path, ctx: &egui::Context) {
        let fw = Framework::detect(root);
        match fw {
            Framework::CargoTest => self.spawn(
                fw,
                root,
                "cargo",
                &["test", "--workspace", "--no-fail-fast"],
                ctx,
            ),
            // `<venv-python> -m pytest`, never bare `pytest` from $PATH: on PEP-668 distros
            // pytest and the project's imports live venv-only (same rule as runconfig's Run).
            Framework::Pytest => {
                let py = crate::runconfig::python_bin(root);
                self.spawn(fw, root, &py, &["-m", "pytest", "-v", "--tb=short"], ctx)
            }
            Framework::Ctest => {
                let dir = newest_ctest_build_dir(root).unwrap_or_else(|| root.to_path_buf());
                self.spawn(fw, &dir, "ctest", &["--output-on-failure"], ctx);
            }
            // The detailed console logger prints one `Passed/Failed/Skipped <fqname> [n ms]`
            // line per test — the only default channel with per-test granularity.
            Framework::DotnetTest => self.spawn(
                fw,
                root,
                "dotnet",
                &["test", "--logger", "console;verbosity=detailed", "--nologo"],
                ctx,
            ),
        }
        self.root = root.to_path_buf();
    }

    /// Re-run exactly one test.
    pub fn rerun_single(&mut self, root: &Path, suite: &str, test: &str, ctx: &egui::Context) {
        let fw = Framework::detect(root);
        match fw {
            Framework::CargoTest => {
                self.spawn(fw, root, "cargo", &["test", test, "--", "--exact"], ctx)
            }
            Framework::Pytest => {
                let nodeid = format!("{suite}::{test}");
                let py = crate::runconfig::python_bin(root);
                self.spawn(fw, root, &py, &["-m", "pytest", "-v", "--tb=short", &nodeid], ctx);
            }
            Framework::Ctest => {
                let dir = newest_ctest_build_dir(root).unwrap_or_else(|| root.to_path_buf());
                let pat = format!("^{test}$");
                self.spawn(fw, &dir, "ctest", &["--output-on-failure", "-R", &pat], ctx);
            }
            Framework::DotnetTest => {
                // FullyQualifiedName~ is a substring match on the fqname — enough to isolate the
                // one method the panel row names.
                let filter = format!("FullyQualifiedName~{test}");
                self.spawn(
                    fw,
                    root,
                    "dotnet",
                    &["test", "--logger", "console;verbosity=detailed", "--nologo", "--filter", &filter],
                    ctx,
                );
            }
        }
        self.root = root.to_path_buf();
    }

    /// Run ONE named test found in `rel_file` (the gutter ▶). Name matching is deliberately
    /// fuzzy where the framework allows it — a bare fn name matches through module paths.
    pub fn run_named(&mut self, root: &Path, rel_file: &Path, name: &str, ctx: &egui::Context) {
        // Runner chosen by the FILE's language, not the workspace root's framework — a mixed
        // repo (pytest scripts inside a cargo workspace) must run the right tool per file.
        let ext = rel_file.extension().and_then(|e| e.to_str()).unwrap_or("");
        let fw = match ext {
            "rs" => Framework::CargoTest,
            "py" => Framework::Pytest,
            "cs" | "fs" => Framework::DotnetTest,
            _ => Framework::detect(root),
        };
        match fw {
            // Substring match (no --exact) reaches mod-nested tests; --workspace reaches tests
            // in member crates of a root-package workspace.
            Framework::CargoTest => {
                self.spawn(fw, root, "cargo", &["test", "--workspace", name], ctx)
            }
            Framework::Pytest => {
                // `file -k name` matches class methods too — a plain file::name nodeid misses
                // unittest/pytest class-style tests.
                let rel = rel_file.display().to_string();
                let py = crate::runconfig::python_bin(root);
                self.spawn(
                    fw,
                    root,
                    &py,
                    &["-m", "pytest", "-v", "--tb=short", &rel, "-k", name],
                    ctx,
                );
            }
            Framework::Ctest => {
                let dir = newest_ctest_build_dir(root).unwrap_or_else(|| root.to_path_buf());
                self.spawn(fw, &dir, "ctest", &["--output-on-failure", "-R", name], ctx);
            }
            Framework::DotnetTest => {
                let filter = format!("FullyQualifiedName~{name}");
                self.spawn(
                    fw,
                    root,
                    "dotnet",
                    &["test", "--logger", "console;verbosity=detailed", "--nologo", "--filter", &filter],
                    ctx,
                );
            }
        }
        self.root = root.to_path_buf();
    }

    fn spawn(&mut self, fw: Framework, cwd: &Path, program: &str, args: &[&str], ctx: &egui::Context) {
        self.stop();
        self.generation += 1;
        let generation = self.generation;
        self.framework = fw;
        self.state = ParseState::new(fw);
        self.started = Some(Instant::now());
        self.elapsed = 0.0;

        let mut command = Command::new(program);
        crate::runner::own_process_group(&mut command);
        let spawned = command
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                self.started = None;
                self.state.suite_mut("error").tests.push(TestCase {
                    name: format!("failed to start {program}: {e}"),
                    status: TestStatus::Fail,
                    duration: None,
                    failure: String::new(),
                    location: None,
                });
                return;
            }
        };
        self.running = true;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        for pipe in [
            stdout.map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
            stderr.map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
        ]
        .into_iter()
        .flatten()
        {
            let tx = self.tx.clone();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let r = BufReader::new(pipe);
                for line in r.lines().map_while(Result::ok) {
                    if tx.send(TestEvent::Line(generation, line)).is_err() {
                        return;
                    }
                    ctx.request_repaint();
                }
            });
        }

        let child = Arc::new(Mutex::new(child));
        self.child = Some(Arc::clone(&child));
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            loop {
                match child.lock().unwrap().try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) => {}
                    Err(_) => break,
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            let _ = tx.send(TestEvent::Done(generation));
            ctx.request_repaint();
        });
    }

    /// Kill the running test process. The killed run's events become stale
    /// (generation-tagged) and are dropped by pump().
    pub fn stop(&mut self) {
        if let Some(child) = self.child.take() {
            crate::runner::kill_process_group(&mut child.lock().unwrap());
        }
        self.running = false;
        if let Some(t0) = self.started.take() {
            self.elapsed = t0.elapsed().as_secs_f32();
        }
    }

    /// Drain events into the parser; call once per frame (ui() does it for you).
    fn pump(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                TestEvent::Line(g, _) | TestEvent::Done(g) if g != self.generation => {}
                TestEvent::Line(_, l) => feed_line(&mut self.state, &l),
                TestEvent::Done(_) => {
                    self.running = false;
                    self.child = None;
                    if let Some(t0) = self.started.take() {
                        self.elapsed = t0.elapsed().as_secs_f32();
                    }
                }
            }
        }
    }

    /// The tool-window body. Returns a location the user clicked (jump target).
    pub fn ui(&mut self, ui: &mut egui::Ui) -> Option<(PathBuf, usize)> {
        self.pump();
        let mut jump: Option<(PathBuf, usize)> = None;
        let mut rerun: Option<(String, String)> = None;

        // ---- summary strip -----------------------------------------------------------------
        let (mut passed, mut failed, mut ignored) = (0usize, 0usize, 0usize);
        for s in &self.state.tree {
            for t in &s.tests {
                match t.status {
                    TestStatus::Pass => passed += 1,
                    TestStatus::Fail => failed += 1,
                    TestStatus::Ignored => ignored += 1,
                    TestStatus::Running => {}
                }
            }
        }
        ui.horizontal(|ui| {
            style::panel_header_inline(ui, "tests");
            if self.running {
                ui.spinner();
                if ui.button("■ stop").clicked_by(egui::PointerButton::Primary) {
                    self.stop();
                }
            }
            style::status_chip(ui, &format!("{passed} ✓"), colors::MOSS());
            if failed > 0 {
                style::status_chip(ui, &format!("{failed} ✗"), colors::ERROR());
            }
            if ignored > 0 {
                style::status_chip(ui, &format!("{ignored} ○"), colors::TEXT_FAINT());
            }
            let secs = if self.running {
                self.started.map(|t| t.elapsed().as_secs_f32()).unwrap_or(0.0)
            } else {
                self.elapsed
            };
            if secs > 0.0 {
                ui.colored_label(colors::TEXT_FAINT(), format!("{secs:.1}s"));
            }
        });
        style::hairline(ui);

        // ---- tree ----------------------------------------------------------------------------
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for (si, suite) in self.state.tree.iter().enumerate() {
                egui::CollapsingHeader::new(
                    egui::RichText::new(&suite.name).color(colors::TEXT_MUTED()),
                )
                .id_salt(("testrun-suite", si))
                .default_open(true)
                .show(ui, |ui| {
                    for t in &suite.tests {
                        let (glyph, gcol) = match t.status {
                            TestStatus::Pass => ("✓", colors::MOSS()),
                            TestStatus::Fail => ("✗", colors::ERROR()),
                            TestStatus::Ignored => ("○", colors::TEXT_FAINT()),
                            TestStatus::Running => ("…", colors::TEXT_FAINT()),
                        };
                        ui.horizontal(|ui| {
                            ui.colored_label(gcol, glyph);
                            let ncol = if t.status == TestStatus::Fail {
                                colors::TEXT()
                            } else {
                                colors::TEXT_MUTED()
                            };
                            ui.colored_label(ncol, &t.name);
                            if ui
                                .small_button("↻")
                                .on_hover_text("re-run this test")
                                .clicked_by(egui::PointerButton::Primary)
                            {
                                rerun = Some((suite.name.clone(), t.name.clone()));
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if let Some(d) = t.duration {
                                        ui.colored_label(
                                            colors::TEXT_FAINT(),
                                            format!("{d:.2}s"),
                                        );
                                    }
                                },
                            );
                        });
                        if t.status == TestStatus::Fail {
                            ui.indent(("testrun-fail", si, &t.name), |ui| {
                                if let Some((path, line)) = &t.location {
                                    let label = format!("{}:{}", path.display(), line);
                                    if ui
                                        .link(
                                            egui::RichText::new(label)
                                                .color(colors::ACCENT_HI())
                                                .monospace(),
                                        )
                                        .clicked_by(egui::PointerButton::Primary)
                                    {
                                        jump = Some((path.clone(), *line));
                                    }
                                }
                                if !t.failure.is_empty() {
                                    ui.label(
                                        egui::RichText::new(&t.failure)
                                            .monospace()
                                            .color(colors::TEXT_MUTED()),
                                    );
                                }
                            });
                        }
                    }
                });
            }
        });

        if let Some((suite, test)) = rerun {
            let root = self.root.clone();
            let ctx = ui.ctx().clone();
            self.rerun_single(&root, &suite, &test, &ctx);
        }
        jump
    }
}

// =================================================================================================
// tests — pure parser over canned transcripts
// =================================================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(fw: Framework, transcript: &str) -> ParseState {
        let mut st = ParseState::new(fw);
        for line in transcript.lines() {
            feed_line(&mut st, line);
        }
        st
    }

    #[test]
    fn cargo_multi_suite_with_failure() {
        let transcript = r#"
   Compiling foo v0.1.0
     Running unittests src/lib.rs (target/debug/deps/foo-1a2b3c4d5e6f7890)

running 3 tests
test parser::parses_ok ... ok
test parser::rejects_bad ... FAILED
test slow_one ... ignored

failures:

---- parser::rejects_bad stdout ----
thread 'parser::rejects_bad' panicked at src/parser.rs:42:9:
assertion `left == right` failed
  left: 1
 right: 2
note: run with `RUST_BACKTRACE=1` for a backtrace

failures:
    parser::rejects_bad

test result: FAILED. 1 passed; 1 failed; 1 ignored; 0 measured; 0 filtered out

     Running tests/integration.rs (target/debug/deps/integration-aabbccddeeff0011)

running 1 test
test end_to_end ... ok

test result: ok. 1 passed; 0 failed; 0 ignored
"#;
        let st = feed_all(Framework::CargoTest, transcript);
        assert_eq!(st.tree.len(), 2);
        assert_eq!(st.tree[0].name, "foo");
        assert_eq!(st.tree[1].name, "integration");
        assert_eq!(st.tree[0].tests.len(), 3);
        assert_eq!(st.tree[0].tests[0].name, "parser::parses_ok");
        assert_eq!(st.tree[0].tests[0].status, TestStatus::Pass);
        let fail = &st.tree[0].tests[1];
        assert_eq!(fail.status, TestStatus::Fail);
        assert_eq!(
            fail.location,
            Some((PathBuf::from("src/parser.rs"), 42))
        );
        assert!(fail.failure.contains("assertion `left == right` failed"));
        assert!(fail.failure.contains("panicked at"));
        assert_eq!(st.tree[0].tests[2].status, TestStatus::Ignored);
        assert_eq!(st.tree[1].tests[0].status, TestStatus::Pass);
    }

    #[test]
    fn cargo_old_panic_format() {
        assert_eq!(
            parse_panicked_at("thread 'main' panicked at 'boom', src/lib.rs:7:5"),
            Some((PathBuf::from("src/lib.rs"), 7))
        );
        assert_eq!(
            parse_panicked_at("thread 'x' panicked at src/a.rs:10:9:"),
            Some((PathBuf::from("src/a.rs"), 10))
        );
        assert_eq!(parse_panicked_at("no panic here"), None);
    }

    #[test]
    fn pytest_verbose_with_failure() {
        let transcript = r#"
============================= test session starts ==============================
collected 3 items

tests/test_math.py::test_add PASSED                                      [ 33%]
tests/test_math.py::test_div FAILED                                      [ 66%]
tests/test_io.py::test_read SKIPPED                                      [100%]

=================================== FAILURES ===================================
_________________________________ test_div _____________________________________
tests/test_math.py:12: in test_div
    assert div(1, 0) == 0
tests/test_math.py:12: AssertionError
=========================== short test summary info ============================
FAILED tests/test_math.py::test_div - AssertionError
"#;
        let st = feed_all(Framework::Pytest, transcript);
        assert_eq!(st.tree.len(), 2);
        assert_eq!(st.tree[0].name, "tests/test_math.py");
        assert_eq!(st.tree[0].tests[0].status, TestStatus::Pass);
        let fail = &st.tree[0].tests[1];
        assert_eq!(fail.name, "test_div");
        assert_eq!(fail.status, TestStatus::Fail);
        assert_eq!(
            fail.location,
            Some((PathBuf::from("tests/test_math.py"), 12))
        );
        assert!(fail.failure.contains("AssertionError"));
        assert_eq!(st.tree[1].tests[0].status, TestStatus::Ignored);
    }

    #[test]
    fn ctest_pass_and_fail() {
        let transcript = r#"
Test project /home/u/proj/build
    Start 1: unit_math
1/2 Test #1: unit_math ........................   Passed    0.05 sec
    Start 2: unit_io
2/2 Test #2: unit_io ..........................***Failed    0.12 sec
Assertion failed: (fd >= 0), function open_file, file io.c, line 33.

50% tests passed, 1 tests failed out of 2
"#;
        let st = feed_all(Framework::Ctest, transcript);
        assert_eq!(st.tree.len(), 1);
        assert_eq!(st.tree[0].name, "ctest");
        let tests = &st.tree[0].tests;
        assert_eq!(tests.len(), 2);
        assert_eq!(tests[0].name, "unit_math");
        assert_eq!(tests[0].status, TestStatus::Pass);
        assert_eq!(tests[0].duration, Some(0.05));
        assert_eq!(tests[1].name, "unit_io");
        assert_eq!(tests[1].status, TestStatus::Fail);
        assert_eq!(tests[1].duration, Some(0.12));
        assert!(tests[1].failure.contains("Assertion failed"));
    }

    #[test]
    fn ctest_failure_body_survives_blank_lines() {
        let transcript = r#"
1/2 Test #1: unit_io ..........................***Failed    0.12 sec
first paragraph of output

second paragraph after a blank line
    Start 2: unit_math
2/2 Test #2: unit_math ........................   Passed    0.01 sec

50% tests passed, 1 tests failed out of 2
"#;
        let st = feed_all(Framework::Ctest, transcript);
        let fail = &st.tree[0].tests[0];
        assert!(fail.failure.contains("first paragraph"));
        assert!(fail.failure.contains("second paragraph"));
        assert!(!fail.failure.contains("Start 2"));
        assert_eq!(st.tree[0].tests[1].status, TestStatus::Pass);
    }

    #[test]
    fn dotnet_pass_fail_skip_and_location() {
        // VSTest console logger at verbosity=detailed.
        let transcript = r#"
  Passed Calc.Tests.MathTests.Adds [12 ms]
  Skipped Calc.Tests.MathTests.NotReady
  Failed Calc.Tests.MathTests.Divides [4 ms]
  Error Message:
   Assert.Equal() Failure: 0 != 1
  Stack Trace:
     at Calc.Tests.MathTests.Divides() in /src/Calc.Tests/MathTests.cs:line 20

Passed!  - Failed: 1, Passed: 1, Skipped: 1, Total: 3
"#;
        let st = feed_all(Framework::DotnetTest, transcript);
        // Grouped by namespace+class.
        assert_eq!(st.tree.len(), 1);
        assert_eq!(st.tree[0].name, "Calc.Tests.MathTests");
        let tests = &st.tree[0].tests;
        assert_eq!(tests.len(), 3);
        assert_eq!(tests[0].name, "Calc.Tests.MathTests.Adds");
        assert_eq!(tests[0].status, TestStatus::Pass);
        assert_eq!(tests[0].duration, Some(0.012)); // ms → seconds
        assert_eq!(tests[1].status, TestStatus::Ignored);
        let fail = &tests[2];
        assert_eq!(fail.status, TestStatus::Fail);
        assert!(fail.failure.contains("Assert.Equal() Failure"));
        assert_eq!(
            fail.location,
            Some((PathBuf::from("/src/Calc.Tests/MathTests.cs"), 20))
        );
    }

    #[test]
    fn finds_rust_test_decls() {
        let src = r#"
mod tests {
    #[test]
    fn plain_case() {}

    #[test]
    /// docs between attribute and fn
    pub async fn documented_case() {}

    #[tokio::test]
    async fn tokio_case() {}

    fn not_a_test() {}
    let fnx = 3; // must not match
}
"#;
        let decls = find_test_decls(src, "rs");
        let names: Vec<&str> = decls.iter().map(|(_, n)| n.as_str()).collect();
        assert_eq!(names, vec!["plain_case", "documented_case", "tokio_case"]);
        assert_eq!(decls[0].0, 3, "0-based fn line");
    }

    #[test]
    fn finds_single_line_test_decls() {
        let rs = "#[test] fn one_liner() {}\n#[test]\nfn two_liner() {}\n";
        let names: Vec<String> = find_test_decls(rs, "rs").into_iter().map(|(_, n)| n).collect();
        assert_eq!(names, vec!["one_liner", "two_liner"]);

        let cs = "[Fact] public void OneLiner() {}\n[Fact]\npublic void TwoLiner() {}\n";
        let names: Vec<String> = find_test_decls(cs, "cs").into_iter().map(|(_, n)| n).collect();
        assert_eq!(names, vec!["OneLiner", "TwoLiner"]);
    }

    #[test]
    fn finds_python_and_csharp_test_decls() {
        let py = "def test_alpha():
    pass

async def test_beta():
    pass

def helper():
    pass
";
        let names: Vec<String> = find_test_decls(py, "py").into_iter().map(|(_, n)| n).collect();
        assert_eq!(names, vec!["test_alpha", "test_beta"]);

        let cs = "public class T {
    [Fact]
    public void Adds() {}
    [Theory]
    [InlineData(1)]
    public async Task Parses(int x) {}
    public void NotATest() {}
}
";
        let names: Vec<String> = find_test_decls(cs, "cs").into_iter().map(|(_, n)| n).collect();
        assert_eq!(names, vec!["Adds", "Parses"]);

        assert!(find_test_decls("anything", "txt").is_empty());
    }

    #[test]
    fn detect_prefers_cargo() {
        let dir = std::env::temp_dir().join(format!("testrun-detect-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("Cargo.toml"), "[package]").unwrap();
        assert_eq!(Framework::detect(&dir), Framework::CargoTest);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
