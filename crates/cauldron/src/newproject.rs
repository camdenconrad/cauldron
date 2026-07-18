//! Project templates for the New Project dialog (Ctrl+Shift+N).
//!
//! Every template lands a *runnable* skeleton: source, a build/run entry point, `.gitignore`, a git
//! repo — and a project-local `.venv`. The venv is not just for the Python templates: on a PEP-668
//! distro (Arch, and every RuneOS box) a bare `pip install` into the system interpreter is refused
//! outright, so ANY project that later grows a build script, a codegen step or a test helper wants
//! an interpreter it is allowed to write to. Making it standard costs one `python3 -m venv` and
//! removes a whole class of "externally-managed-environment" papercuts. It is best-effort: a box
//! without `python3` still gets its project (see [`create_project`]).
//!
//! The two Rune templates are path-linked to the sibling RuneOS checkouts. Those paths are resolved
//! to ABSOLUTE ones at creation time ([`find_sibling`]) — a relative `../livewall-studio` would only
//! work for a project created in exactly the right directory.

use std::path::{Path, PathBuf};

#[derive(PartialEq, Clone, Copy, Debug)]
pub enum Template {
    Empty,
    CargoBin,
    CargoLib,
    RustWgpu,
    RuneApp,
    RuneCompute,
    C,
    CFlight,
    Cpp,
    Python,
    Node,
    TypeScript,
    JavaScript,
    Html,
}

/// The dialog's grid, in display order: `(template, label, one-line hint)`.
pub const TEMPLATES: &[(Template, &str, &str)] = &[
    (Template::CargoBin, "Rust (bin)", "cargo new — binary crate"),
    (Template::CargoLib, "Rust (lib)", "cargo new --lib"),
    (Template::RustWgpu, "Rust (wgpu)", "winit window + wgpu clear pass"),
    (Template::RuneApp, "Rune (wgpu app)", "eframe/wgpu pinned to the Rune line"),
    (Template::RuneCompute, "Rune (compute)", "rune-runtime auto-placing compute"),
    (Template::C, "C", "src/main.c + Makefile (c11, -Wall)"),
    (Template::CFlight, "C (flight-style)", "strict NASA-ish flags + compile_flags"),
    (Template::Cpp, "C++", "src/main.cpp + Makefile (c++20)"),
    (Template::Python, "Python", "package layout + pyproject + .venv"),
    (Template::Node, "Node.js", "package.json + ESM entry point"),
    (Template::TypeScript, "TypeScript", "tsc + src/index.ts"),
    (Template::JavaScript, "JavaScript", "plain ESM, no build step"),
    (Template::Html, "HTML page", "index.html + style.css + main.js"),
    (Template::Empty, "Empty", "just git + .gitignore + .venv"),
];

/// Create the project skeleton at `target`.
///
/// The venv and `git init` are deliberately NON-FATAL: a missing `python3` or `git` must not strand
/// a project whose source tree was already written to disk (the caller opens `target` the moment
/// this returns `Ok`, and a half-created dir the user can't see is worse than a missing venv).
/// Anything that would leave the tree unusable — a failed `cargo new`, an unwritable path — is an
/// error and aborts before the project is opened.
pub fn create_project(target: &Path, template: Template) -> Result<(), String> {
    use Template::*;

    // `cargo new` insists on creating the directory itself, so it runs FIRST and the shared
    // scaffolding (gitignore/venv) lands on top of what it made.
    match template {
        CargoBin => cargo_new(target, &[])?,
        CargoLib => cargo_new(target, &["--lib"])?,
        _ => std::fs::create_dir_all(target).map_err(|e| format!("{}: {e}", target.display()))?,
    }

    for (rel, body) in files_for(target, template) {
        write_file(target, &rel, &body)?;
    }

    // .gitignore: the template's own entries plus the venv, which every project now has.
    let mut ignore = gitignore_for(template).to_string();
    ignore.push_str(".venv/\n__pycache__/\n");
    append_gitignore(target, &ignore)?;

    // --- best-effort tail: neither failure invalidates the tree ---------------------------------
    if !target.join(".git").exists() {
        let _ = run("git", &["init", "-q"], Some(target));
    }
    make_venv(target);
    Ok(())
}

/// `python3 -m venv .venv` — the PEP-668 escape hatch every project ships with. Silent on failure
/// (no python3 on the box, no `ensurepip`): the project is still perfectly usable without it.
fn make_venv(target: &Path) {
    if target.join(".venv").exists() {
        return;
    }
    if let Err(e) = run("python3", &["-m", "venv", ".venv"], Some(target)) {
        log::warn!("no .venv for {}: {e}", target.display());
    }
}

fn cargo_new(target: &Path, extra: &[&str]) -> Result<(), String> {
    let mut args: Vec<&str> = vec!["new"];
    args.extend_from_slice(extra);
    let s = target.to_string_lossy();
    args.push(&s);
    run("cargo", &args, None)
}

fn run(cmd: &str, args: &[&str], cwd: Option<&Path>) -> Result<(), String> {
    let mut c = std::process::Command::new(cmd);
    c.args(args);
    if let Some(d) = cwd {
        c.current_dir(d);
    }
    match c.output() {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
            Err(if err.is_empty() { format!("{cmd} failed") } else { err })
        }
        Err(e) => Err(format!("{cmd}: {e}")),
    }
}

fn write_file(target: &Path, rel: &str, body: &str) -> Result<(), String> {
    let path = target.join(rel);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(&path, body).map_err(|e| format!("{}: {e}", path.display()))
}

/// Append to `.gitignore`, creating it if absent (`cargo new` already wrote one with `/target`).
fn append_gitignore(target: &Path, add: &str) -> Result<(), String> {
    let path = target.join(".gitignore");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut out = existing.clone();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    for line in add.lines().filter(|l| !l.trim().is_empty()) {
        if !existing.lines().any(|e| e.trim() == line.trim()) {
            out.push_str(line);
            out.push('\n');
        }
    }
    std::fs::write(&path, out).map_err(|e| format!("{}: {e}", path.display()))
}

fn gitignore_for(t: Template) -> &'static str {
    use Template::*;
    match t {
        CargoBin | CargoLib | RustWgpu | RuneApp | RuneCompute => "/target\n",
        C | CFlight | Cpp => "build/\n*.o\n",
        Node | TypeScript | JavaScript => "node_modules/\ndist/\n",
        Python => "*.egg-info/\n.pytest_cache/\n",
        Html | Empty => "",
    }
}

/// The project name a template stamps into manifests — the target's last path component, sanitized
/// to the conservative intersection of what cargo, npm and python all accept as an identifier.
fn project_name(target: &Path) -> String {
    let raw = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let cleaned: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    let cleaned = cleaned.trim_matches(['-', '_']).to_string();
    if cleaned.is_empty() || cleaned.starts_with(|c: char| c.is_ascii_digit()) {
        format!("app-{cleaned}").trim_end_matches('-').to_string()
    } else {
        cleaned
    }
}

/// Render `p` as a TOML basic string BODY (no surrounding quotes), escaping the two characters
/// that would otherwise close or corrupt it. Both are legal in a Linux path, and the paths here are
/// user-controlled (the project's parent directory), so an unescaped one emits a `Cargo.toml` cargo
/// cannot parse at all. Backslash first — escaping it after the quote would double-escape.
fn toml_path(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"")
}

/// Locate a sibling RuneOS checkout (`livewall-studio`, `rune-runtime`) so the Rune templates can
/// path-depend on it with an ABSOLUTE path. Looks beside the new project first, then in the usual
/// `~/RustroverProjects` home. `None` → the template degrades to a commented-out dep rather than
/// emitting a path that does not resolve.
fn find_sibling(target: &Path, name: &str) -> Option<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(parent) = target.parent() {
        roots.push(parent.to_path_buf());
    }
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(home).join("RustroverProjects"));
    }
    roots.into_iter().map(|r| r.join(name)).find(|p| p.is_dir())
}

/// Every file a template writes, as `(relative path, contents)`.
fn files_for(target: &Path, t: Template) -> Vec<(String, String)> {
    use Template::*;
    let name = project_name(target);
    let f = |p: &str, b: String| (p.to_string(), b);

    match t {
        // cargo new already wrote a working crate; nothing to add.
        CargoBin | CargoLib | Empty => Vec::new(),

        RustWgpu => vec![
            f(
                "Cargo.toml",
                format!(
                    "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
                     [dependencies]\nwgpu = \"22.1\"\nwinit = \"0.29\"\npollster = \"0.3\"\n\
                     env_logger = \"0.11\"\n"
                ),
            ),
            f("src/main.rs", rust_wgpu_main()),
        ],

        RuneApp => {
            let patch = match find_sibling(target, "livewall-studio") {
                Some(lw) => format!(
                    "\n# The Rune fork of egui-winit (wlr-data-control clipboard). Same pin as cauldron.\n\
                     [patch.crates-io]\negui-winit = {{ path = \"{}\" }}\n",
                    toml_path(&lw.join("vendor/egui-winit"))
                ),
                None => "\n# NOTE: no livewall-studio checkout found — add the egui-winit patch\n\
                         # ([patch.crates-io]) if clipboard misbehaves under Rune.\n"
                    .to_string(),
            };
            vec![
                f(
                    "Cargo.toml",
                    format!(
                        "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
                         # Pinned to the Rune ecosystem line (wgpu 22.1.0). Do NOT bump these\n\
                         # independently of livewall-studio.\n[dependencies]\negui = \"0.29.1\"\n\
                         eframe = {{ version = \"0.29.1\", default-features = false, \
                         features = [\"wgpu\", \"default_fonts\", \"wayland\", \"x11\"] }}\n{patch}"
                    ),
                ),
                f("src/main.rs", rune_app_main(&name)),
            ]
        }

        RuneCompute => {
            let dep = match find_sibling(target, "rune-runtime") {
                Some(rt) => {
                    format!("rune-runtime = {{ path = \"{}\" }}\n", toml_path(&rt))
                }
                None => "# rune-runtime = { path = \"../rune-runtime\" }  # checkout not found\n"
                    .to_string(),
            };
            vec![
                f(
                    "Cargo.toml",
                    format!(
                        "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
                         [dependencies]\n{dep}\n[profile.release]\nlto = \"thin\"\n"
                    ),
                ),
                f("src/main.rs", rune_compute_main()),
            ]
        }

        C => vec![
            f("src/main.c", C_MAIN.to_string()),
            f("Makefile", makefile("gcc", "CFLAGS", "-std=c11 -Wall -Wextra -O2", "c")),
            f("compile_flags.txt", "-std=c11\n-Wall\n-Wextra\n".to_string()),
        ],

        CFlight => vec![
            f("src/main.c", C_FLIGHT_MAIN.to_string()),
            f(
                "Makefile",
                makefile("gcc", "CFLAGS", "-std=c11 -Wall -Wextra -Werror -O2", "c"),
            ),
            f("compile_flags.txt", "-std=c11\n-Wall\n-Wextra\n".to_string()),
        ],

        Cpp => vec![
            f("src/main.cpp", CPP_MAIN.to_string()),
            f("Makefile", makefile("g++", "CXXFLAGS", "-std=c++20 -Wall -Wextra -O2", "cpp")),
            f("compile_flags.txt", "-std=c++20\n-Wall\n-Wextra\n".to_string()),
        ],

        Python => {
            let module = name.replace('-', "_");
            vec![
                f(
                    "pyproject.toml",
                    format!(
                        "[project]\nname = \"{name}\"\nversion = \"0.1.0\"\n\
                         requires-python = \">=3.9\"\ndependencies = []\n\n\
                         [build-system]\nrequires = [\"setuptools>=61\"]\n\
                         build-backend = \"setuptools.build_meta\"\n"
                    ),
                ),
                f(&format!("src/{module}/__init__.py"), String::new()),
                f(&format!("src/{module}/__main__.py"), PY_MAIN.to_string()),
                f("main.py", format!("from src.{module}.__main__ import main\n\nmain()\n")),
                f("README.md", py_readme(&name)),
            ]
        }

        Node => vec![
            f(
                "package.json",
                format!(
                    "{{\n  \"name\": \"{name}\",\n  \"version\": \"0.1.0\",\n  \"type\": \"module\",\n  \
                     \"main\": \"src/index.js\",\n  \"scripts\": {{\n    \"start\": \"node src/index.js\"\n  }}\n}}\n"
                ),
            ),
            f("src/index.js", NODE_MAIN.to_string()),
        ],

        TypeScript => vec![
            f(
                "package.json",
                format!(
                    "{{\n  \"name\": \"{name}\",\n  \"version\": \"0.1.0\",\n  \"type\": \"module\",\n  \
                     \"scripts\": {{\n    \"build\": \"tsc\",\n    \"start\": \"node dist/index.js\"\n  }},\n  \
                     \"devDependencies\": {{\n    \"typescript\": \"^5.4.0\",\n    \"@types/node\": \"^20.0.0\"\n  }}\n}}\n"
                ),
            ),
            f("tsconfig.json", TSCONFIG.to_string()),
            f("src/index.ts", TS_MAIN.to_string()),
        ],

        JavaScript => vec![
            f(
                "package.json",
                format!(
                    "{{\n  \"name\": \"{name}\",\n  \"version\": \"0.1.0\",\n  \"type\": \"module\",\n  \
                     \"main\": \"index.js\",\n  \"scripts\": {{\n    \"start\": \"node index.js\"\n  }}\n}}\n"
                ),
            ),
            f("index.js", NODE_MAIN.to_string()),
        ],

        Html => vec![
            f("index.html", html_index(&name)),
            f("style.css", HTML_CSS.to_string()),
            f("main.js", HTML_JS.to_string()),
        ],
    }
}

/// A build/clean Makefile whose only variable parts are the compiler, its flags var and the source
/// extension — identical shape for C and C++ so `make` behaves the same in both trees.
fn makefile(cc: &str, flags_var: &str, flags: &str, ext: &str) -> String {
    let cc_var = if ext == "cpp" { "CXX" } else { "CC" };
    format!(
        "{cc_var} ?= {cc}\n{flags_var} ?= {flags}\n\nall: build/app\n\n\
         build/app: src/main.{ext}\n\tmkdir -p build\n\t$({cc_var}) $({flags_var}) -o $@ $^\n\n\
         run: build/app\n\t./build/app\n\nclean:\n\trm -rf build\n\n.PHONY: all run clean\n"
    )
}

const C_MAIN: &str = "#include <stdio.h>\n\nint main(void)\n{\n    printf(\"hello\\n\");\n    return 0;\n}\n";

const C_FLIGHT_MAIN: &str =
    "#include <stdio.h>\n\nint main(void)\n{\n    printf(\"mission ready\\n\");\n    return 0;\n}\n";

const CPP_MAIN: &str =
    "#include <iostream>\n\nint main()\n{\n    std::cout << \"hello\\n\";\n    return 0;\n}\n";

const PY_MAIN: &str = "def main() -> None:\n    print(\"hello\")\n\n\nif __name__ == \"__main__\":\n    main()\n";

const NODE_MAIN: &str = "console.log(\"hello\");\n";

const TS_MAIN: &str = "function main(): void {\n  console.log(\"hello\");\n}\n\nmain();\n";

const TSCONFIG: &str = "{\n  \"compilerOptions\": {\n    \"target\": \"ES2022\",\n    \"module\": \"ES2022\",\n    \"moduleResolution\": \"bundler\",\n    \"outDir\": \"dist\",\n    \"rootDir\": \"src\",\n    \"strict\": true,\n    \"esModuleInterop\": true,\n    \"skipLibCheck\": true\n  },\n  \"include\": [\"src\"]\n}\n";

const HTML_CSS: &str = ":root {\n  color-scheme: dark;\n}\n\nbody {\n  margin: 0;\n  min-height: 100vh;\n  display: grid;\n  place-items: center;\n  font-family: system-ui, sans-serif;\n  background: #16130f;\n  color: #e8e2d8;\n}\n";

const HTML_JS: &str = "document.querySelector(\"h1\")?.addEventListener(\"click\", () => {\n  console.log(\"hello\");\n});\n";

fn html_index(name: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\">\n  <head>\n    <meta charset=\"utf-8\" />\n    \
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\" />\n    \
         <title>{name}</title>\n    <link rel=\"stylesheet\" href=\"style.css\" />\n  </head>\n  \
         <body>\n    <h1>{name}</h1>\n    <script type=\"module\" src=\"main.js\"></script>\n  </body>\n</html>\n"
    )
}

fn py_readme(name: &str) -> String {
    format!(
        "# {name}\n\nThe project ships its own interpreter — the system one is externally managed\n\
         (PEP 668), so installing into it is refused.\n\n```sh\nsource .venv/bin/activate\n\
         pip install -e .\npython main.py\n```\n"
    )
}

fn rust_wgpu_main() -> String {
    r#"//! A winit window with a wgpu surface, cleared to a flat colour each frame.

use std::sync::Arc;

use winit::event::{Event, WindowEvent};
use winit::event_loop::EventLoop;
use winit::window::WindowBuilder;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    let event_loop = EventLoop::new()?;
    let window = Arc::new(WindowBuilder::new().with_title("wgpu").build(&event_loop)?);

    let instance = wgpu::Instance::default();
    let surface = instance.create_surface(window.clone())?;
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        compatible_surface: Some(&surface),
        ..Default::default()
    }))
    .ok_or("no suitable GPU adapter")?;
    let (device, queue) =
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default(), None))?;

    let size = window.inner_size();
    let mut config = surface
        .get_default_config(&adapter, size.width.max(1), size.height.max(1))
        .ok_or("surface is not supported by this adapter")?;
    surface.configure(&device, &config);

    event_loop.run(move |event, target| {
        let Event::WindowEvent { event, .. } = event else { return };
        match event {
            WindowEvent::CloseRequested => target.exit(),
            WindowEvent::Resized(new) => {
                config.width = new.width.max(1);
                config.height = new.height.max(1);
                surface.configure(&device, &config);
            }
            WindowEvent::RedrawRequested => {
                let frame = match surface.get_current_texture() {
                    Ok(f) => f,
                    Err(_) => {
                        surface.configure(&device, &config);
                        return;
                    }
                };
                let view = frame.texture.create_view(&Default::default());
                let mut enc = device.create_command_encoder(&Default::default());
                enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("clear"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color {
                                r: 0.05,
                                g: 0.04,
                                b: 0.03,
                                a: 1.0,
                            }),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    ..Default::default()
                });
                queue.submit([enc.finish()]);
                frame.present();
                window.request_redraw();
            }
            _ => {}
        }
    })?;
    Ok(())
}
"#
    .to_string()
}

fn rune_app_main(name: &str) -> String {
    format!(
        r#"//! An eframe/wgpu window, pinned to the Rune ecosystem line.

fn main() -> eframe::Result<()> {{
    let native = eframe::NativeOptions {{
        viewport: egui::ViewportBuilder::default().with_inner_size([1200.0, 800.0]),
        ..Default::default()
    }};
    eframe::run_native(
        "{name}",
        native,
        Box::new(|_cc| Ok(Box::new(App::default()))),
    )
}}

#[derive(Default)]
struct App {{
    clicks: u32,
}}

impl eframe::App for App {{
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {{
        egui::CentralPanel::default().show(ctx, |ui| {{
            ui.heading("{name}");
            if ui.button(format!("clicked {{}}x", self.clicks)).clicked() {{
                self.clicks += 1;
            }}
        }});
    }}
}}
"#
    )
}

fn rune_compute_main() -> String {
    r#"//! rune-runtime places this work for you: serial for small inputs, the CPU worker pool in the
//! middle, the GPU once it pays for itself. The width of the CPU pool comes from rune-sched's live
//! telemetry, so bulk compute never piles onto a contended box.

use rune_runtime::{Place, Runtime};

fn main() {
    let rt = Runtime::new();
    match rt.planner().idle_cpus() {
        Some(idle) => println!("rune-sched telemetry: LIVE ({idle} CPUs idle)"),
        None => println!("rune-sched telemetry: absent — static full-width fallback"),
    }

    // y = a*x + y, in place. The runtime picks WHERE from the size and the live occupancy.
    for exp in [10u32, 18, 24] {
        let n = 1usize << exp;
        let x = vec![1.0f32; n];
        let mut y = vec![2.0f32; n];
        let place: Place = rt.saxpy(2.0, &x, &mut y);
        println!("n = {n:>9}  ->  {place:?}   (y[0] = {})", y[0]);
    }
}
"#
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cauldron-newproj-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// The name stamped into Cargo.toml / package.json / pyproject must be a legal identifier for
    /// all three, whatever the user typed as the directory name.
    #[test]
    fn project_name_is_sanitized() {
        assert_eq!(project_name(Path::new("/x/my-app")), "my-app");
        assert_eq!(project_name(Path::new("/x/my app!")), "my-app");
        assert_eq!(project_name(Path::new("/x/.hidden")), "hidden");
        // A leading digit is legal for a directory but not for a crate/module — it gets a prefix.
        assert_eq!(project_name(Path::new("/x/3d")), "app-3d");
    }

    /// Every non-cargo template writes a tree that actually contains its entry point, plus the
    /// shared tail: .gitignore (with the venv ignored) and a git repo. Asserted for ALL of them so
    /// a newly added template cannot silently ship an empty directory.
    #[test]
    fn every_template_writes_its_entry_point() {
        let base = tmp("all");
        let expect: &[(Template, &str)] = &[
            (Template::RustWgpu, "src/main.rs"),
            (Template::RuneApp, "src/main.rs"),
            (Template::RuneCompute, "src/main.rs"),
            (Template::C, "src/main.c"),
            (Template::CFlight, "src/main.c"),
            (Template::Cpp, "src/main.cpp"),
            (Template::Python, "pyproject.toml"),
            (Template::Node, "src/index.js"),
            (Template::TypeScript, "src/index.ts"),
            (Template::JavaScript, "index.js"),
            (Template::Html, "index.html"),
        ];
        for (t, entry) in expect {
            let dir = base.join(format!("{t:?}"));
            create_project(&dir, *t).unwrap_or_else(|e| panic!("{t:?}: {e}"));
            assert!(dir.join(entry).is_file(), "{t:?} must write {entry}");
            let ignore = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
            assert!(ignore.contains(".venv/"), "{t:?} must ignore its venv");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The Makefile templates must name the source file they actually wrote — a C++ Makefile
    /// pointing at `src/main.c` builds nothing.
    #[test]
    fn makefiles_match_their_sources() {
        let base = tmp("make");
        for (t, src) in [(Template::C, "src/main.c"), (Template::Cpp, "src/main.cpp")] {
            let dir = base.join(format!("{t:?}"));
            create_project(&dir, t).unwrap();
            let mk = std::fs::read_to_string(dir.join("Makefile")).unwrap();
            assert!(mk.contains(src), "{t:?} Makefile must build {src}: {mk}");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The gitignore merge is idempotent and additive: cargo's own `/target` survives, the venv
    /// line is added once, and re-running never duplicates a line.
    #[test]
    fn gitignore_merges_without_duplicates() {
        let dir = tmp("ignore");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".gitignore"), "/target\n").unwrap();
        append_gitignore(&dir, "/target\n.venv/\n").unwrap();
        append_gitignore(&dir, ".venv/\n").unwrap();
        let body = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert_eq!(body.matches("/target").count(), 1, "{body}");
        assert_eq!(body.matches(".venv/").count(), 1, "{body}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The C and C++ skeletons must actually BUILD — the whole point of a template is that `make`
    /// works the second the project opens. Compiles for real and runs the binary. Skipped (not
    /// failed) on a box without the compiler, so the suite still passes in a bare container.
    #[test]
    fn c_and_cpp_templates_compile_and_run() {
        if run("make", &["--version"], None).is_err() {
            eprintln!("skipped: no make on this box");
            return;
        }
        let base = tmp("build");
        for (t, cc) in [(Template::C, "gcc"), (Template::Cpp, "g++"), (Template::CFlight, "gcc")] {
            if run(cc, &["--version"], None).is_err() {
                eprintln!("skipped {t:?}: no {cc}");
                continue;
            }
            let dir = base.join(format!("{t:?}"));
            create_project(&dir, t).unwrap();
            run("make", &[], Some(&dir)).unwrap_or_else(|e| panic!("{t:?} must build: {e}"));
            assert!(dir.join("build/app").is_file(), "{t:?} produced no binary");
            let out = std::process::Command::new(dir.join("build/app")).output().unwrap();
            assert!(out.status.success(), "{t:?} binary must run");
            assert!(!out.stdout.is_empty(), "{t:?} binary must print something");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The `.venv` every project ships must be a REAL, usable interpreter — an empty `.venv/`
    /// directory would satisfy "it exists" while still leaving pip refusing to install (the
    /// PEP-668 papercut this feature exists to remove).
    #[test]
    fn projects_ship_a_working_venv() {
        if run("python3", &["--version"], None).is_err() {
            eprintln!("skipped: no python3 on this box");
            return;
        }
        let dir = tmp("venv");
        create_project(&dir, Template::Python).unwrap();
        let py = dir.join(".venv/bin/python");
        assert!(py.is_file(), "the venv must contain an interpreter");
        let out = std::process::Command::new(&py).args(["-c", "import sys; print(sys.prefix)"]).output().unwrap();
        assert!(out.status.success(), "the venv interpreter must run");
        let prefix = String::from_utf8_lossy(&out.stdout);
        assert!(
            prefix.trim().starts_with(&*dir.to_string_lossy()),
            "sys.prefix must point INTO the project, not the system python: {prefix}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The Rust templates must actually COMPILE — a hand-written `Cargo.toml` and `main.rs` can
    /// drift from the crates they pin (wgpu/winit APIs move, and the Rune templates call into
    /// sibling checkouts whose API is not frozen). `#[ignore]`d because it resolves and builds real
    /// dependency trees: minutes, and a network on a cold cache.
    ///
    ///     cargo test -p cauldron -- --ignored rust_templates
    #[test]
    #[ignore = "downloads and builds real dependency trees"]
    fn rust_templates_compile() {
        let base = tmp("rustc");
        for t in [Template::CargoBin, Template::RustWgpu, Template::RuneApp, Template::RuneCompute] {
            let dir = base.join(format!("{t:?}"));
            create_project(&dir, t).unwrap();
            // The Rune templates path-depend on sibling checkouts; without them the manifest is
            // deliberately depless and proves nothing, so don't claim it did.
            if matches!(t, Template::RuneApp | Template::RuneCompute)
                && !std::fs::read_to_string(dir.join("Cargo.toml")).unwrap().contains("path = \"/")
            {
                eprintln!("skipped {t:?}: no sibling checkout to link against");
                continue;
            }
            run("cargo", &["check", "--quiet"], Some(&dir))
                .unwrap_or_else(|e| panic!("{t:?} must compile:\n{e}"));
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A sibling checkout reached through a path containing a `"` or a `\` (both legal in a Linux
    /// directory name) must still produce a PARSEABLE Cargo.toml — unescaped, either one closes the
    /// TOML string early and cargo cannot read the manifest at all.
    #[test]
    fn sibling_paths_are_escaped_for_toml() {
        assert_eq!(toml_path(Path::new(r#"/a/b"c/d"#)), r#"/a/b\"c/d"#);
        assert_eq!(toml_path(Path::new(r"/a/b\c/d")), r"/a/b\\c/d");
        assert_eq!(toml_path(Path::new(r#"/a/b\"c"#)), r#"/a/b\\\"c"#, "backslash escaped first");

        // End to end: a project whose PARENT holds a quote, with a livewall-studio checkout beside
        // it, must emit a manifest whose [patch.crates-io] path survives a real TOML parse.
        let base = tmp("toml").join(r#"we"ird"#);
        std::fs::create_dir_all(base.join("livewall-studio/vendor/egui-winit")).unwrap();
        let dir = base.join("proj");
        create_project(&dir, Template::RuneApp).unwrap();
        let manifest = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap();
        assert!(manifest.contains("[patch.crates-io]"), "the sibling must be found: {manifest}");
        let parsed: toml::Value = toml::from_str(&manifest)
            .unwrap_or_else(|e| panic!("manifest must parse:\n{manifest}\n{e}"));
        let patched = parsed["patch"]["crates-io"]["egui-winit"]["path"].as_str().unwrap();
        assert_eq!(
            Path::new(patched),
            base.join("livewall-studio/vendor/egui-winit"),
            "the path must round-trip through TOML with the quote intact"
        );
        let _ = std::fs::remove_dir_all(base.parent().unwrap());
    }

    /// A python3-less box (or one whose venv module is broken) must still get its project: the venv
    /// is a convenience, not a precondition. Simulated by pre-creating an unwritable `.venv` name —
    /// what matters is that create_project returned Ok and the source tree is there.
    #[test]
    fn missing_venv_does_not_fail_the_project() {
        let dir = tmp("novenv");
        std::fs::create_dir_all(dir.join(".venv")).unwrap(); // make_venv sees it and bails
        create_project(&dir, Template::C).expect("project is created regardless of the venv");
        assert!(dir.join("src/main.c").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
