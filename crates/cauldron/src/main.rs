//! Cauldron — Rune-native IDE. The P3 shell: menu bar, split editor groups with real tabs
//! (starting to the RIGHT of the Project tree), bottom dock (multi-shell terminal on the left,
//! Output/Problems tabbed on the right), right pin bar, quick-open + a real file picker,
//! live clangd/rust-analyzer squiggles, the Claude usage meter (5h/7d), and the NASA layer —
//! whole-program Rule-1 findings with exact rule citations + in-editor orange squiggles on
//! cycle call sites.
//!
//! Usage: `cauldron [PATH]` where PATH is a file or a project directory (defaults to cwd).
//! Desktop wiring: app_id = com.coffee.cauldron (custom titlebar — Rune has no SSD).

mod ai;
mod ai_actions;
mod boot_trace;
mod conflicts;
mod checklist;
mod chsig_ui;

/// Generation namespace for the Change Signature dialog's `textDocument/references` requests.
/// Buffer generations count up from 0, so this range is unreachable by them — which is what
/// keeps a dialog reply from being mistaken for a Find Usages reply and vice versa.
const CHSIG_GEN_BASE: u64 = 1 << 60;
mod coverage;
mod deps;
mod blame;
mod diffview;
mod depsinstall;
mod git;
mod history;
mod icons;
mod localhist;
mod mdpreview;
mod nav;
mod webpreview;
mod newproject;
mod openfolder;
mod prpanel;
mod everywhere;
mod palette;
mod psi;
mod quickopen;
mod runconfig;
mod runner;
mod search;
mod settings;
mod state;
mod systheme;
#[cfg(test)]
mod testenv;
mod style;
mod symbols;
mod terminal;
mod testrun;
mod usage;
mod watcher;
mod workspace;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use cauldron_editor::position::{self, Point};
use cauldron_editor::syntax::Lang;
use cauldron_editor::view::ViewDiag;
use cauldron_editor::{Buffer, EditorView};
use cauldron_lsp::{lsp_types, txsync, Encoding, LspEvent, LspManager};
use livewall_uikit::theme::ORANGE;
use ropey::Rope;

use git::GitPanel;
use openfolder::{OpenFolder, PickAction};
use psi::{PsiService, PsiState};
use quickopen::QuickOpen;
use runner::Runner;
use search::RepoSearch;
use style::colors;
use terminal::TerminalPane;
use usage::UsageMeter;
use workspace::{TreeAction, Workspace};

/// One drained batch of editor edits: (path, [(pre-rope, transaction)], post-rope).
/// (path, drained edits, post-rope, edits-came-from-the-focused-editor).
type EditedBatch = (PathBuf, Vec<(Rope, cauldron_editor::Transaction)>, Rope, bool);

/// The exact rule text shown with every Rule-1 finding — the user asked for EXACTLY what is
/// violated, so we cite it verbatim.
/// Where a shell opens: the project root, else `~`. Never anything else, and never a path that
/// does not exist — cider's PTY sets the cwd unconditionally (`pty.rs`: `Some(dir) => cmd.cwd(dir)`,
/// falling back to `$HOME` only when handed `None`), so a missing directory doesn't degrade
/// gracefully, it makes the shell spawn FAIL.
///
/// Two ways the root is not a real directory: no-project mode, whose root is a sentinel path that
/// by design never exists (`state::no_project_root`), and a project dir renamed or deleted under a
/// live session. `/` is the last resort purely because it always exists.
/// Case-folded subsequence test for the file-symbols fuzzy filter ("qfoo" hits "quick_foo").
fn is_subsequence(needle: &str, hay: &str) -> bool {
    let mut it = hay.chars();
    'outer: for n in needle.chars() {
        for h in it.by_ref() {
            if h == n {
                continue 'outer;
            }
        }
        return false;
    }
    true
}

/// Watch eval tag: epoch in the high bits, (index+1) in the low 16 (0 stays the console).
fn watch_tag(epoch: u64, i: usize) -> u64 {
    (epoch << 16) | ((i as u64 + 1) & 0xffff)
}

/// Inverse of [`watch_tag`].
fn watch_untag(tag: u64) -> (u64, usize) {
    (tag >> 16, ((tag & 0xffff) as usize).saturating_sub(1))
}

fn shell_cwd(no_project: bool, root: &Path, home: Option<&Path>) -> PathBuf {
    if !no_project && root.is_dir() {
        return root.to_path_buf();
    }
    match home {
        Some(h) if h.is_dir() => h.to_path_buf(),
        _ => PathBuf::from("/"),
    }
}

/// Coding-standards profile. GSFC 582 (Goddard's Flight Software Branch C standard) is what
/// cFS reviewers actually gate — plus cFE CI's clang-format check; JPL's Power of Ten is the
/// stricter, aspirational tier (Holzmann, JPL D-60411).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub(crate) enum Standards {
    Off,
    /// GSFC 582 / cFS conventions: recursion findings as ADVICE (582 doesn't hard-forbid it;
    /// it's flight-software culture), clang-format drift vs the repo's .clang-format as a
    /// warning (that IS the cFE CI format-check gate).
    #[default]
    Gsfc,
    /// JPL Power of Ten: Rule-1 cycles as errors with verbatim rule text.
    JplPot,
}

const RULE1_TEXT: &str = "JPL Power of Ten — Rule 1: Restrict all code to very simple control \
flow constructs. Do not use goto, setjmp/longjmp, or direct/indirect RECURSION. Recursion makes \
static stack-bound proofs impossible in flight software.";

/// Persist every panic (message + location + backtrace) to `~/.local/share/cauldron/crash.log`.
/// A desktop-launched GUI app has nowhere for stderr to go, so without this a crash vanishes with
/// no trace. Chains the default hook so a terminal run still prints as usual. Backtrace capture is
/// forced on regardless of `RUST_BACKTRACE` because the whole point is the field crash we did not
/// anticipate.
fn install_panic_logger() {
    use std::io::Write;
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let path = std::env::var_os("HOME").map(|h| {
            PathBuf::from(h).join(".local/share/cauldron/crash.log")
        });
        if let Some(path) = path {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Ok(mut f) =
                std::fs::OpenOptions::new().create(true).append(true).open(&path)
            {
                let bt = std::backtrace::Backtrace::force_capture();
                let loc = info
                    .location()
                    .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
                    .unwrap_or_else(|| "unknown".into());
                let _ = writeln!(
                    f,
                    "\n==== cauldron panic @ {loc} ====\n{info}\n--- backtrace ---\n{bt}\n"
                );
            }
        }
        default(info);
    }));
}

fn main() -> eframe::Result {
    boot_trace::init(); // CAULDRON_BOOT_TRACE=1 — t0 for every boot mark below
    boot_trace::mark("main-entry");
    env_logger::init();
    install_panic_logger();
    let arg = std::env::args_os().nth(1).map(PathBuf::from);

    // The vector-master app icon, pre-rendered to raw RGBA (assets/icon/README.md).
    let icon = egui::IconData {
        rgba: include_bytes!("../../../assets/icon/icon-256.rgba").to_vec(),
        width: 256,
        height: 256,
    };
    let options = eframe::NativeOptions {
        // Reopen at the size/position you left the window at. The inner_size below stays as the
        // FIRST-RUN fallback: eframe only applies it when it has no remembered geometry.
        persist_window: true,
        viewport: egui::ViewportBuilder::default()
            .with_title("Cauldron")
            .with_app_id("com.coffee.cauldron")
            .with_icon(icon)
            .with_decorations(false)
            .with_resizable(true)
            // FIRST-RUN fallback only — `persist_window` above means a remembered geometry wins.
            // Measured off the real layout rather than guessed.
            .with_inner_size([2760.0, 1595.0]),
        // Boot-wave item 7: pin the renderer to VULKAN with an explicit power preference. The
        // egui-wgpu default (PRIMARY | GL) instance-probes EGL/GL and every secondary Vulkan
        // ICD before settling on the discrete GPU — pure boot cost on this NVIDIA box.
        // PORTABILITY TRADEOFF: a host with no working Vulkan ICD loses the GL fallback and
        // fails to create a device — acceptable for a Rune-native IDE (Rune itself is a
        // Vulkan-class compositor); the WGPU_BACKEND / WGPU_POWER_PREF env overrides are
        // preserved as the escape hatch (WGPU_BACKEND=gl restores the old fallback by hand).
        wgpu_options: eframe::egui_wgpu::WgpuConfiguration {
            supported_backends: eframe::wgpu::util::backend_bits_from_env()
                .unwrap_or(eframe::wgpu::Backends::VULKAN),
            power_preference: eframe::wgpu::util::power_preference_from_env()
                .unwrap_or(eframe::wgpu::PowerPreference::HighPerformance),
            ..Default::default()
        },
        ..Default::default()
    };
    boot_trace::mark("pre-run_native");
    eframe::run_native(
        "com.coffee.cauldron",
        options,
        Box::new(move |cc| {
            boot_trace::mark("creator-closure-entry");
            // Theme must be set BEFORE apply_ide_style (which reads the palette to build egui
            // Visuals). The editor crate gets the same flip.
            {
                let theme = systheme::resolve(settings::load().theme);
                style::set_theme(theme);
                cauldron_editor::theme::set_light(style::is_light_theme());
            }
            livewall_uikit::theme::apply(&cc.egui_ctx);
            style::apply_ide_style(&cc.egui_ctx); // IDE layer on the Rune base (see style.rs)
            // Crisp, instant scrolling. egui animates scroll_to_rect over 0.1–0.3s by default, so
            // the editor's caret-follow GLIDES on every caret move — reads as the view drifting
            // while you type/click. A code editor wants the view pinned, not easing.
            cc.egui_ctx
                .all_styles_mut(|s| s.scroll_animation = egui::style::ScrollAnimation::none());
            // Staged fonts (boot-wave item 3): only the ~1.5MB core faces (Latin + symbols —
            // all UI chrome) load synchronously before first paint; the full Noto chain
            // (CJK/Arabic/Indic scripts) is built on cider's "cider-fonts" thread and swapped
            // in via set_fonts + request_repaint a frame or two later. Known tradeoff: such
            // text visible on frame 1 shows tofu until the swap lands. The boot summary's
            // `fonts=` field stays honest by timing ONLY the sync core.
            let t = boot_trace::begin();
            cider::fonts::install_core(&cc.egui_ctx);
            boot_trace::end(t, "fonts", "core-only");
            cider::fonts::install_full_async(&cc.egui_ctx);
            boot_trace::mark("fonts-full-spawned");
            let ctx = cc.egui_ctx.clone();
            let notifier: cauldron_lsp::Notifier = Arc::new(move || ctx.request_repaint());
            Ok(Box::new(App::new(arg.clone(), notifier)))
        }),
    )
}

/// What boot decided to open (see [`choose_root`]).
#[derive(Debug, PartialEq)]
enum RootChoice {
    Project { root: PathBuf, initial_file: Option<PathBuf> },
    /// Nothing sensible to open: welcome/empty state with the recents picker, walk NOTHING.
    NoProject,
}

/// PURE boot root selection (boot-wave item 1 — "boot straight into the last project"):
/// - An explicit CLI `arg` keeps exactly the historic behavior: a dir is the project; a file
///   opens with its parent (or cwd) as the project — even `$HOME`, if the user insists.
/// - No arg: cwd wins when it is project-worthy (terminal `cd proj && cauldron` still works),
///   but `$HOME` / `/` / ancestors of `$HOME` are NEVER silently opened (the dock launches
///   with cwd = `$HOME`, which used to walk the entire home dir "takes forever to boot").
/// - Rejected/unreadable cwd falls back to the last-project pointer (`last_project` arrives
///   pre-checked: it still exists on disk); session restore then brings back every tab.
/// - Nothing available → [`RootChoice::NoProject`].
///
/// `arg` carries `(path, is_dir)` so the fs probe stays at the caller; `cwd = None` means
/// `std::env::current_dir()` itself failed.
fn choose_root(
    arg: Option<(PathBuf, bool)>,
    cwd: Option<&Path>,
    home: Option<&Path>,
    last_project: Option<&Path>,
) -> RootChoice {
    match arg {
        Some((p, true)) => return RootChoice::Project { root: p, initial_file: None },
        Some((p, false)) => {
            let root = p
                .parent()
                .map(PathBuf::from)
                .filter(|r| !r.as_os_str().is_empty())
                .unwrap_or_else(|| {
                    cwd.map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."))
                });
            return RootChoice::Project { root, initial_file: Some(p) };
        }
        None => {}
    }
    if let Some(c) = cwd {
        if state::project_worthy(c, home) {
            return RootChoice::Project { root: c.to_path_buf(), initial_file: None };
        }
    }
    if let Some(last) = last_project {
        return RootChoice::Project { root: last.to_path_buf(), initial_file: None };
    }
    RootChoice::NoProject
}

/// One editor tab: text engine buffer + highlighting view + path.
struct OpenFile {
    path: PathBuf,
    buffer: Buffer,
    view: EditorView,
    lang: Option<Lang>,
    /// Buffer differs from disk (dot on the tab; save writes it out).
    dirty: bool,
    /// False = LAZY placeholder (boot-wave item 4): only the path is real — `buffer` is empty
    /// and `view` is a stub. Session restore creates non-active tabs this way (chrome-only);
    /// [`App::ensure_active_loaded`] hydrates on first activation. NOTHING that reads or
    /// writes buffer content may touch an unloaded tab.
    loaded: bool,
}

impl OpenFile {
    fn load(path: PathBuf) -> Result<Self, std::io::Error> {
        // Size guard: this load runs on the UI thread. Past the ceiling we refuse with a
        // clear message instead of freezing the app; in between, LARGE-FILE MODE loads the
        // text but skips tree-sitter and LSP (parsing a 50 MB file stalls for seconds and
        // no language server enjoys a didOpen that size) — rope editing stays fast.
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if size > HUGE_FILE_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "{} MB file — beyond the {} MB editor ceiling (use an external pager)",
                    size / (1024 * 1024),
                    HUGE_FILE_BYTES / (1024 * 1024)
                ),
            ));
        }
        let text = std::fs::read_to_string(&path)?;
        if size > LARGE_FILE_BYTES {
            let buffer = Buffer::from_text(&text);
            let view = EditorView::new(&buffer, ""); // no grammar → no syntax pass
            return Ok(Self { path, buffer, view, lang: None, dirty: false, loaded: true });
        }
        let buffer = Buffer::from_text(&text);
        let view = EditorView::new(&buffer, &path.to_string_lossy());
        let lang = Lang::from_path(&path.to_string_lossy());
        Ok(Self { path, buffer, view, lang, dirty: false, loaded: true })
    }

    /// A chrome-only placeholder tab: real path + lang (tab title, C-deps kick), empty buffer,
    /// stub view built WITHOUT a grammar (no file read, no parse, no query lookup).
    fn lazy(path: PathBuf) -> Self {
        let buffer = Buffer::from_text("");
        let view = EditorView::new(&buffer, "");
        let lang = Lang::from_path(&path.to_string_lossy());
        Self { path, buffer, view, lang, dirty: false, loaded: false }
    }

    /// Load a lazy tab's real content in place (first activation). `pending_caret` = the
    /// session caret parked while the tab was a placeholder. No-op when already loaded.
    fn hydrate(&mut self, pending_caret: Option<usize>) -> Result<(), std::io::Error> {
        if self.loaded {
            return Ok(());
        }
        let mut real = OpenFile::load(self.path.clone())?;
        if let Some(caret) = pending_caret {
            let rope = real.buffer.rope().clone();
            real.view.jump_to(caret.min(rope.len_bytes()), &rope);
        }
        *self = real;
        Ok(())
    }

    fn name(&self) -> String {
        self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }
}

/// Maximum side-by-side editor splits. Three fits a wide screen without the panes getting too
/// narrow to be useful; beyond that egui columns just cramp everything.
const MAX_GROUPS: usize = 3;

/// Above this, a file opens in LARGE-FILE MODE: full editing, no syntax highlighting, no LSP.
const LARGE_FILE_BYTES: u64 = 10 * 1024 * 1024;
/// Above this, opening is refused outright — a UI-thread read this size freezes the app.
const HUGE_FILE_BYTES: u64 = 200 * 1024 * 1024;

/// Where Ctrl+\ sends the focused pane `gi`'s active tab: `(insert_a_new_pane, target_index)`.
/// Under the cap a fresh pane opens immediately to the right (`gi + 1`); at the cap the tab moves
/// into the next existing pane, wrapping. Pure so the index math is unit-tested.
fn split_destination(num_groups: usize, gi: usize) -> (bool, usize) {
    if num_groups < MAX_GROUPS {
        (true, gi + 1)
    } else {
        (false, (gi + 1) % num_groups)
    }
}

/// A tab picked up for dragging (JetBrains drag-to-split): the source pane + tab index and a label
/// for the drag ghost.
struct TabDrag {
    from_group: usize,
    from_index: usize,
    label: String,
}

/// What a drop resolves to, relative to a reference pane: move the tab INTO that pane, or open a
/// new split pane to its left/right.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DropKind {
    Move,
    SplitLeft,
    SplitRight,
}

/// Resolve a drop to `(action, reference pane)` from the panes' horizontal spans and whether the
/// pointer is over the TAB STRIP (the top band) or the editor BODY.
///
/// Over the strip it is always a MOVE into the pane under the pointer — dragging ALONG the tab bar
/// reorders / moves between panes, never splits (the bug otherwise: a small reorder of an edge tab,
/// whose tab sits in a pane's outer quarter, would spawn a split). Only in the editor body do the
/// outer quarters become split zones (JetBrains "drop near the edge → split"); the middle is a
/// move-into, and past the far edges splits at that end. Pure so the geometry is unit-tested.
fn resolve_drop(spans: &[(f32, f32)], x: f32, over_strip: bool) -> Option<(DropKind, usize)> {
    if spans.is_empty() {
        return None;
    }
    let pane_at = |x: f32| -> usize {
        if x < spans[0].0 {
            0
        } else if x > spans[spans.len() - 1].1 {
            spans.len() - 1
        } else {
            spans.iter().position(|&(l, r)| x >= l && x <= r).unwrap_or(0)
        }
    };
    if over_strip {
        return Some((DropKind::Move, pane_at(x)));
    }
    if x < spans[0].0 {
        return Some((DropKind::SplitLeft, 0));
    }
    if x > spans[spans.len() - 1].1 {
        return Some((DropKind::SplitRight, spans.len() - 1));
    }
    let p = pane_at(x);
    let (l, r) = spans[p];
    let w = (r - l).max(1.0);
    let frac = (x - l) / w;
    let kind = if frac < 0.25 {
        DropKind::SplitLeft
    } else if frac > 0.75 {
        DropKind::SplitRight
    } else {
        DropKind::Move
    };
    Some((kind, p))
}

/// One editor split: its own tab row + active index. Splits are JetBrains "move right": a tab
/// lives in exactly one group (no shared-buffer divergence).
struct EditorGroup {
    files: Vec<OpenFile>,
    active: usize,
}

impl EditorGroup {
    fn active_file(&mut self) -> Option<&mut OpenFile> {
        self.files.get_mut(self.active)
    }
}

/// Per-file diagnostics, layered by channel so a publish never clobbers another layer:
/// [0] = LSP push, [1] = LSP pull, [2] = NASA (Rule-1 cycle call sites), [3] = format drift
/// (clang-format vs the repo's .clang-format — the cFE CI format-check gate).
#[derive(Default)]
struct DiagStore {
    layers: HashMap<PathBuf, [Vec<ViewDiag>; 4]>,
}

impl DiagStore {
    fn replace(&mut self, path: &Path, layer: usize, diags: Vec<ViewDiag>) {
        self.layers.entry(path.to_path_buf()).or_default()[layer] = diags;
    }

    /// Drop every file's diagnostics (project switch). The store is keyed by absolute path and
    /// nothing else prunes it, so the Problems panel would otherwise keep listing findings for a
    /// project that is no longer open — and clicking one would open a file outside the root.
    fn clear(&mut self) {
        self.layers.clear();
    }

    /// All layers merged, sorted by range start — what the view paints.
    fn merged(&self, path: &Path) -> Vec<ViewDiag> {
        let mut out: Vec<ViewDiag> = self
            .layers
            .get(path)
            .map(|l| l.iter().flatten().cloned().collect())
            .unwrap_or_default();
        out.sort_by_key(|d| d.range.start);
        out
    }

    /// Keep stored ranges glued to their tokens between publishes.
    fn map_through(&mut self, path: &Path, tx: &cauldron_editor::Transaction) {
        if let Some(layers) = self.layers.get_mut(path) {
            for layer in layers.iter_mut() {
                layer.retain_mut(|d| match txsync::map_range_through_tx(d.range.clone(), tx) {
                    Some(r) => {
                        d.range = r;
                        true
                    }
                    None => false,
                });
            }
        }
    }

    /// Absolute paths of every file whose layers hold at least one ERROR (LSP severity 1 or
    /// NASA 5) — feeds the tree's propagated squiggles.
    fn error_paths(&self) -> Vec<PathBuf> {
        self.layers
            .iter()
            .filter(|(_, layers)| layers.iter().flatten().any(|d| matches!(d.severity, 1 | 5)))
            .map(|(p, _)| p.clone())
            .collect()
    }

    fn counts(&self, path: &Path) -> (usize, usize) {
        let mut err = 0;
        let mut warn = 0;
        if let Some(layers) = self.layers.get(path) {
            for d in layers.iter().flatten() {
                match d.severity {
                    1 | 5 => err += 1,
                    2 => warn += 1,
                    _ => {}
                }
            }
        }
        (err, warn)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsTab {
    Appearance,
    Editor,
    Ai,
    Standards,
    About,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub(crate) enum RightTab {
    Pinned,
    /// Default: the right panel opens on Structure.
    #[default]
    Structure,
    History,
    Ai,
    /// Rendered Markdown preview of the active .md file.
    Preview,
}

/// Which tab the dock's right slot shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub(crate) enum BottomTab {
    Output,
    History,
    Prs,
    /// Default: matches `App::new`, so a project with no saved tab opens on Problems as before.
    #[default]
    Problems,
    Git,
    Usages,
    Debug,
    Tests,
    Checks,
}

/// The completion popup: LSP items anchored at the word start, filtered by the live prefix.
struct CompletionUi {
    path: PathBuf,
    /// Byte where the completed word starts (replace range start + filter anchor).
    anchor: usize,
    items: Vec<lsp_types::CompletionItem>,
    selected: usize,
    pos: egui::Pos2,
    /// Has the user arrowed into the list? Enter only accepts once they have — otherwise Enter is
    /// a NEWLINE. The popup auto-opens on every word char, so accepting on a bare Enter is what
    /// made "press Enter to make a new line" silently insert a completion instead.
    navigated: bool,
}

/// A user-initiated close that would discard unsaved edits, parked until the Save / Discard /
/// Cancel modal resolves it. Tab closes carry the target paths (not indices — closing shifts
/// them); `Exit` is a vetoed window close waiting on the same choice.
/// Inline AI edit: an instruction prompt over a recorded selection. The reply replaces the
/// selection through the generation-guarded apply path (undo-safe; falls back to caret
/// insert when the buffer moved).
struct AiEdit {
    origin: ai_actions::Origin,
    /// The selected code at open time (what the model rewrites).
    code: String,
    lang: String,
    instruction: String,
    in_flight: bool,
    focus_pending: bool,
    error: Option<String>,
    rx: std::sync::mpsc::Receiver<Option<String>>,
    tx: std::sync::mpsc::Sender<Option<String>>,
}

/// The inline name prompt for tree fs operations.
enum NamePrompt {
    NewFile { dir_rel: PathBuf },
    NewFolder { dir_rel: PathBuf },
    Rename { rel: PathBuf },
}

/// Quiet window after the last keystroke before a dirty C buffer's text ships to the PSI
/// worker as an overlay (item 7). Sits inside the design's ~300-500 ms band: long enough to
/// coalesce a typing burst, short enough that squiggles feel live.
const PSI_OVERLAY_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(400);

struct App {
    workspace: Workspace,
    /// NO-PROJECT mode: nothing to open at boot (cwd unworthy, no last-project pointer).
    /// `workspace` holds the never-existing [`state::no_project_root`] sentinel; sessions,
    /// recents, the pointer, and the fs watcher are all inert until open_folder picks a root.
    no_project: bool,
    groups: Vec<EditorGroup>,
    focused: usize,
    error: Option<String>,
    quickopen: QuickOpen,
    /// Double-Shift Search Everywhere overlay + its tap detector.
    everywhere: everywhere::SearchEverywhere,
    double_shift: everywhere::DoubleShift,
    openfolder: OpenFolder,
    /// Repaint hook (the creator closure's `request_repaint`) — hand clones to background
    /// workers spawned outside update() (no ctx there), e.g. run-config detection.
    repaint: cauldron_lsp::Notifier,
    lsp: LspManager,
    diags: DiagStore,
    lsp_message: Option<String>,
    runner: Runner,
    terminal: TerminalPane,
    psi: PsiService,
    psi_scan_pending: bool,
    /// C files saved since the last drain — routed to the PSI service's INCREMENTAL path
    /// (single-file `replace_file_facts`); a full rescan is the fallback when the service
    /// can't take them yet.
    psi_saved_files: Vec<PathBuf>,
    /// Dirty-buffer overlay debounce (item 7): C file -> last edit instant. Once a buffer has
    /// been quiet for [`PSI_OVERLAY_DEBOUNCE`], its LIVE text ships to the PSI worker as an
    /// overlay shadowing the disk facts — NASA squiggles update without saving.
    psi_overlay_pending: HashMap<PathBuf, std::time::Instant>,
    /// C buffers whose last view closed since the previous drain — the PSI worker drops any
    /// live overlay and restores disk facts (close WITHOUT save must not leak buffer facts).
    psi_closed_files: Vec<PathBuf>,
    /// The project-root file watcher. None until the first frame (needs a ctx) and across
    /// project switches; recreated lazily in update() on the current workspace root.
    watcher: Option<watcher::FsWatcher>,
    /// A workspace re-walk is wanted (save / fs action / watcher batch) — drained in update()
    /// into `Workspace::refresh_async`, so the walk + git status never run on the UI thread.
    ws_refresh_pending: bool,
    /// An fs event arrived — check open, non-dirty buffers for on-disk changes and reload them.
    watcher_fired: bool,
    /// A full SymbolIndex rebuild is wanted (open / project switch / watcher burst) — drained
    /// in update() once no build stream is inflight.
    symbols_rebuild_pending: bool,
    /// A watcher FullRescan arrived: the PSI + symbol rebuilds are armed but DEFERRED until the
    /// async workspace re-walk it requested swaps in — draining them immediately would consume
    /// the pre-event universe (a git checkout's new files missing, `has_c_sources` stale) and
    /// nothing would ever re-kick.
    rescan_after_refresh: bool,
    /// Incremental-lane files not (yet) members of the canonical universe: a just-created file
    /// races the async re-walk (retried once when it lands), a gitignored / out-of-universe
    /// file never joins and is dropped on the retry miss — the index must never admit files
    /// the full scan excludes.
    fs_pending_universe: Vec<PathBuf>,
    /// Bottom dock right slot: open + which tab.
    bottom_open: bool,
    bottom_tab: BottomTab,
    project_open: bool,
    pins: Vec<PathBuf>,
    pins_open: bool,
    prompt: Option<(NamePrompt, String)>,
    /// One-shot: the just-opened prompt overlay (name/goto-line/rename) grabs keyboard focus on
    /// its first frame ONLY. Unconditional per-frame request_focus() made clicking back into the
    /// editor impossible — the overlay re-stole focus every frame and Enter submitted it.
    prompt_focus_pending: bool,
    /// A close (tab or window) parked behind the unsaved-changes modal.
    /// Window-close blocked because the auto-save on exit FAILED (read-only file, full disk).
    /// The only surviving confirm case after auto-save; tab closes never prompt.
    exit_confirm: bool,
    /// The unsaved-changes modal approved (or nothing was dirty): the next window-close request
    /// passes instead of being vetoed with `CancelClose`.
    exit_approved: bool,
    /// Background one-liners (cargo fetch etc.) surfaced in the status bar.
    bg_message: Arc<Mutex<Option<String>>>,
    usage: UsageMeter,
    search: RepoSearch,
    /// Side-by-side git diff takeover for one file (None = normal editor view).
    diff_view: Option<diffview::DiffView>,
    /// Background `git blame` service + per-file cache (inline blame annotations).
    blame: blame::BlameService,
    /// The History tab: paginated commit log + per-commit changed files.
    history: history::HistoryPanel,
    /// Coverage runner + last run's per-file line marks.
    coverage: coverage::CoverageService,
    /// GitHub Pull Requests tab (gh CLI).
    prs: prpanel::PrPanel,
    /// Inline blame on the caret line (palette-toggleable).
    inline_blame_enabled: bool,
    /// Active UI theme choice. Mirrors [`settings::Settings::theme`]; live-applied.
    theme_choice: settings::ThemeChoice,
    /// `i.time` of the last OS-theme poll (only while the choice is System).
    system_theme_poll: f64,
    /// The live web-preview server (WebStorm-style), `None` until first used. Kept alive so
    /// it keeps serving; a new root restarts it.
    web_server: Option<webpreview::WebServer>,
    settings_open: bool,
    settings_tab: SettingsTab,
    /// Editor monospace size (Ctrl+± / Ctrl+scroll — editor-ONLY zoom; global zoom lives in
    /// Settings).
    editor_font: f32,
    /// Set when a restored session wants the terminal pane open (needs a ctx → first frame).
    restore_terminal_pending: bool,
    /// Session carets for LAZY (not-yet-loaded) tabs, applied on hydration. Also what
    /// capture_session persists for tabs that were never activated this run.
    lazy_carets: HashMap<PathBuf, usize>,
    /// True only inside restore_session: per-file gutter refreshes are skipped (the restore
    /// tail issues ONE batched `git diff` for everything it loaded).
    restoring_session: bool,
    /// Periodic session autosave clock (on_exit never fires on SIGTERM/kill — see save cadence).
    last_session_save: std::time::Instant,
    /// Quick-fix state: generation we asked codeActions for + the received menu.
    fix_request_gen: Option<u64>,
    fix_menu: Option<(egui::Pos2, PathBuf, Vec<lsp_types::CodeActionOrCommand>)>,
    /// Heading for `fix_menu` — the same popup serves Quick Fixes and Refactor This.
    fix_menu_title: &'static str,
    /// The Change Signature dialog (Ctrl+F6), when open.
    chsig: Option<chsig_ui::ChangeSigUi>,
    /// Generation of the in-flight rust-analyzer references request backing the dialog. Kept
    /// separate from `usages_gen` so the reply fills the dialog instead of the Usages panel.
    chsig_refs_gen: Option<u64>,
    /// Counter behind [`CHSIG_GEN_BASE`], keeping dialog reference requests in their own
    /// generation namespace so they cannot collide with Find Usages.
    chsig_req_seq: u64,
    /// True while the in-flight code-action request was `only: ["refactor", …]`, so the reply
    /// opens as Refactor This (grouped, with the built-in Rename entry) rather than Quick Fixes.
    fix_request_refactor: bool,
    git_panel: GitPanel,
    /// Ctrl+G goto-line overlay: Some(buffer text) while open.
    goto_line: Option<String>,
    /// Shift+F6 rename overlay: (new-name buffer, path, byte, generation).
    rename: Option<(String, PathBuf, usize, u64)>,
    /// Find-usages results for the dock tab: (display rel, path, line, preview).
    usages: Vec<(PathBuf, usize, String)>,
    usages_gen: Option<u64>,
    /// Generation of an in-flight call-hierarchy (incoming calls) request; its result fills
    /// the same Usages panel, labeled as callers.
    call_hierarchy_gen: Option<u64>,
    /// The current Usages panel holds callers (call hierarchy), not references.
    usages_are_callers: bool,
    /// True when `usages` came from the native PSI index (file had no live language server)
    /// rather than LSP references — the panel labels index results explicitly.
    usages_from_index: bool,
    /// Completion popup: request gen + the live list.
    completion: Option<CompletionUi>,
    completion_gen: Option<u64>,
    /// Waiting on completionItem/resolve for auto-import edits (path, post-accept generation).
    resolve_pending: Option<(PathBuf, u64)>,
    /// In-flight `codeAction/resolve` for a deferred refactor: `(path, buffer generation, title)`.
    /// The generation drops replies that land after the buffer moved on; the title is only for
    /// the failure message, since a resolved action never reports which request it answered.
    action_resolve_pending: Option<(PathBuf, u64, String)>,
    /// Signature help popup: (anchor pos, the server's help payload).
    sig_help: Option<(egui::Pos2, lsp_types::SignatureHelp)>,
    sig_gen: Option<u64>,
    /// Structure panel: flattened (depth, glyph, name, detail, target position) rows.
    outline: Vec<(usize, &'static str, String, String, lsp_types::Position)>,
    outline_for: Option<(PathBuf, u64)>,
    outline_requested: Option<(PathBuf, u64)>,
    right_tab: RightTab,
    // ---- ultracode wave: run configs · checklist · AI panel · history · tests · symbols ----
    run_cfgs: runconfig::RunConfigStore,
    checklist: checklist::Checklist,
    ai_panel: ai_actions::AiPanel,
    /// Inline AI edit modal ("AI: Edit Selection…"), `None` = closed.
    ai_edit: Option<AiEdit>,
    /// Idle auto-save: fires once `i.time` passes this (armed by every edit drain).
    autosave_deadline: Option<f64>,
    /// Debug launch parked until the pre-debug `cargo build` in the runner exits 0.
    debug_pending_build: Option<PathBuf>,
    /// Multi-target go-to-definition chooser: `(anchor pos, targets, selected)`.
    def_choices: Option<(egui::Pos2, Vec<(PathBuf, lsp_types::Position)>, usize)>,
    /// Active merge-conflict resolver: the file being resolved. Conflicts are re-parsed each
    /// frame from the live buffer; the resolver always works the FIRST remaining one.
    conflict_file: Option<PathBuf>,
    /// Recent-locations popup (Ctrl+Shift+E): `(rows, selected)`, each row a jump point with
    /// a precomputed one-line snippet. Newest-first; picking jumps there.
    recent_locations: Option<(Vec<(nav::NavPoint, String)>, usize)>,
    history_ui: localhist::HistoryUi,
    testrun: testrun::TestRunner,
    symbols: symbols::SymbolIndex,
    goto_symbol: symbols::GotoSymbolUi,
    /// workspace/symbol fan-out state (cauldron#2 item 9): generation stamped on the inflight
    /// query (stale server answers are dropped) + the query text last fanned out (None = none).
    ws_symbols_gen: u64,
    ws_symbols_sent: Option<String>,
    /// Editor right-click menu: (screen pos, path, clicked byte).
    editor_menu: Option<(egui::Pos2, PathBuf, usize)>,
    /// Claude-powered inline ghost completions.
    ai: ai::AiCompleter,
    // ---- debugger (DAP) ----
    dap: cauldron_dap::DebugManager,
    /// Breakpoints per file, 1-based lines, sorted.
    /// Per-file breakpoints: 1-based line + optional condition. Session-persisted.
    breakpoints: HashMap<PathBuf, Vec<(u32, Option<String>)>>,
    /// Per-file bookmarks: 1-based lines. Session-persisted; F11 toggles, Shift+F11 lists.
    bookmarks: HashMap<PathBuf, Vec<u32>>,
    /// The bookmarks list overlay (Shift+F11): open flag + selected row.
    bookmarks_open: bool,
    bookmarks_sel: usize,
    /// Snapshot rows for the bookmarks overlay: (path, 1-based line, preview text).
    bookmark_rows: Vec<(PathBuf, u32, String)>,
    /// Go-to-symbol-in-file overlay (Ctrl+F12): fuzzy filter over the LSP outline.
    file_symbols_open: bool,
    file_symbols_query: String,
    file_symbols_sel: usize,
    /// Watch expressions re-evaluated on every debugger stop (Watches panel).
    watches: Vec<String>,
    /// Latest value per watch (None = not evaluated yet this stop).
    watch_vals: Vec<Option<String>>,
    /// Draft in the "add watch" box.
    watch_input: String,
    /// Bumped per watch refresh; stale tagged results (from before a removal/refresh) are dropped.
    watch_epoch: u64,
    /// In-progress condition edit in the Breakpoints list: ((file, line), draft text). Committed
    /// on focus loss.
    bp_cond_draft: Option<((PathBuf, u32), String)>,
    /// Per-file test declarations for the gutter ▶: path → (buffer generation, (line, name)).
    test_decls: HashMap<PathBuf, (u64, Vec<(usize, String)>)>,
    /// Throttles the test-decl rescans during typing bursts.
    last_test_scan: std::time::Instant,
    dbg_stack: Vec<cauldron_dap::Frame>,
    /// Live threads `(id, name)` at the last stop — the threads view; click switches active.
    dbg_threads: Vec<(i64, String)>,
    dbg_scopes: Vec<cauldron_dap::Scope>,
    dbg_vars: HashMap<i64, Vec<cauldron_dap::Var>>,
    /// variablesReference requests already out — a reference the adapter never answers must
    /// not be re-requested every frame (the old contains_key gate flooded the wire).
    dbg_vars_pending: std::collections::HashSet<i64>,
    dbg_console: Vec<String>,
    dbg_frame: Option<i64>,
    dbg_eval: String,
    /// Executable candidates when a C workspace has several built binaries (picker in Debug tab).
    dbg_pick: Vec<PathBuf>,
    /// Set by the C dependency resolver when clangd should be bounced onto a fresh compile DB.
    clangd_restart: Arc<std::sync::atomic::AtomicBool>,
    /// One resolver kick per workspace.
    c_deps_kicked: bool,
    /// Multi-ecosystem dependency auto-install. The generation is bumped on every project switch so
    /// a resolve for the project you just LEFT stops writing its status line (see [`depsinstall`]).
    deps_gen: Arc<std::sync::atomic::AtomicU64>,
    /// Whether dependency auto-install runs on open. Mirrors [`settings::Settings::auto_deps`].
    auto_deps: bool,
    /// AI backend config (provider + Ollama models). Mirrors [`settings::Settings::ai`]; the
    /// live copy request workers read is pushed into `ai::set_config` on boot and on edit.
    ai_settings: settings::AiSettings,
    /// Inlay hints on? Mirrors [`settings::Settings::inlay_hints`].
    inlay_hints_on: bool,
    /// `(path, generation)` the CURRENT hints belong to / the outstanding request, mirroring
    /// the outline_for/outline_requested pattern.
    inlay_for: Option<(PathBuf, u64)>,
    inlay_requested: Option<(PathBuf, u64)>,
    /// Back/forward jump list (Alt+Left / Alt+Right).
    nav: nav::NavHistory,
    /// Ctrl+Shift+P command palette (Find Action).
    palette: palette::CommandPalette,
    /// Most-recently-opened files, front = most recent (Ctrl+E recent-files switcher).
    recent: Vec<PathBuf>,
    /// A tab being dragged for split/move (None when not dragging). See [`TabDrag`].
    tab_drag: Option<TabDrag>,
    /// Per-frame horizontal `(left, right)` span of each pane, index = group. Rebuilt every frame
    /// the editor area lays out; used to hit-test a tab drop.
    pane_spans: Vec<(f32, f32)>,
    /// Tab closes requested during a group's render, deferred to AFTER the columns loop:
    /// `(group index, paths to close)`. Applying a close inline can collapse a group and
    /// shrink `self.groups` mid-`ui.columns`, crashing the next column. Paths (not indices)
    /// so a shifted group still resolves correctly.
    pending_tab_closes: Vec<(usize, Vec<PathBuf>)>,
    /// NASA mode: whole-program Power-of-Ten enforcement on/off (status-bar + toolbar toggle).
    standards: Standards,
    /// LSP hover ("code lens") state: (path, byte, since_secs, requested) + the shown popup.
    hover_wait: Option<(PathBuf, usize, f64, bool)>,
    hover_popup: Option<(egui::Pos2, String)>,
}

impl App {
    fn new(arg: Option<PathBuf>, notifier: cauldron_lsp::Notifier) -> Self {
        boot_trace::mark("App::new-entry");
        // Global (cross-project) prefs. Font/standards follow you between projects; panel sizes
        // and window geometry do NOT come from here — eframe restores those from egui Memory.
        let prefs = settings::load();
        // Must precede AiCompleter::new() below — its availability probe reads this config.
        ai::set_config(&prefs.ai);
        let cwd = std::env::current_dir().ok();
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let last = state::load_last_project();
        let arg = arg.map(|p| {
            let is_dir = p.is_dir();
            (p, is_dir)
        });
        let choice = choose_root(arg, cwd.as_deref(), home.as_deref(), last.as_deref());
        let (root, initial_file, no_project) = match choice {
            RootChoice::Project { root, initial_file } => {
                // Canonicalize ONCE at the boundary: a relative CLI root (`cauldron .`) would
                // otherwise flow verbatim into workspace.root and from there into the
                // last-project pointer and the recents — and a literal "." re-read by the
                // next (dock, cwd = $HOME) launch resolves to $HOME and walks the entire
                // home dir, the exact regression the boot-root guard exists to kill.
                let root = root.canonicalize().unwrap_or(root);
                let initial_file = initial_file.map(|f| f.canonicalize().unwrap_or(f));
                (root, initial_file, false)
            }
            // The sentinel never exists on disk: the walk, git status, Cargo.toml probes etc.
            // all no-op. The picker (recents-first) opens below instead of any project.
            RootChoice::NoProject => (state::no_project_root(), None, true),
        };

        let workspace = boot_trace::span("workspace-open", || Workspace::open(root.clone()));
        let mut app = Self {
            workspace,
            no_project,
            groups: vec![EditorGroup { files: Vec::new(), active: 0 }],
            focused: 0,
            error: None,
            quickopen: QuickOpen::default(),
            everywhere: everywhere::SearchEverywhere::default(),
            double_shift: everywhere::DoubleShift::default(),
            openfolder: OpenFolder::default(),
            repaint: notifier.clone(),
            lsp: LspManager::new(notifier.clone()),
            dap: cauldron_dap::DebugManager::new(notifier),
            breakpoints: HashMap::new(),
            bookmarks: HashMap::new(),
            bookmarks_open: false,
            bookmarks_sel: 0,
            bookmark_rows: Vec::new(),
            file_symbols_open: false,
            file_symbols_query: String::new(),
            file_symbols_sel: 0,
            watches: Vec::new(),
            watch_vals: Vec::new(),
            watch_input: String::new(),
            watch_epoch: 0,
            bp_cond_draft: None,
            test_decls: HashMap::new(),
            last_test_scan: std::time::Instant::now(),
            dbg_stack: Vec::new(),
            dbg_threads: Vec::new(),
            dbg_scopes: Vec::new(),
            dbg_vars: HashMap::new(),
            dbg_vars_pending: std::collections::HashSet::new(),
            dbg_console: Vec::new(),
            dbg_frame: None,
            dbg_eval: String::new(),
            dbg_pick: Vec::new(),
            diags: DiagStore::default(),
            lsp_message: None,
            runner: Runner::default(),
            terminal: TerminalPane::new(),
            psi: PsiService::new(),
            psi_scan_pending: true,
            psi_saved_files: Vec::new(),
            psi_overlay_pending: HashMap::new(),
            psi_closed_files: Vec::new(),
            watcher: None,
            watcher_fired: false,
            ws_refresh_pending: false,
            // Build-on-open (replaces the old rebuild-only-when-empty kick).
            symbols_rebuild_pending: true,
            rescan_after_refresh: false,
            fs_pending_universe: Vec::new(),
            // FIRST-RUN layout (a project with no saved session yet). A saved session overrides
            // every one of these in restore_session.
            bottom_open: true,
            bottom_tab: BottomTab::Problems,
            project_open: true,
            pins: Vec::new(),
            pins_open: true,
            prompt: None,
            prompt_focus_pending: false,
            exit_confirm: false,
            exit_approved: false,
            bg_message: Arc::new(Mutex::new(None)),
            usage: UsageMeter::new(),
            search: RepoSearch::default(),
            diff_view: None,
            blame: blame::BlameService::default(),
            history: history::HistoryPanel::default(),
            coverage: coverage::CoverageService::default(),
            prs: prpanel::PrPanel::default(),
            settings_open: false,
            settings_tab: SettingsTab::Appearance,
            editor_font: prefs.editor_font,
            // Open the shell on a fresh project too, not only when a saved session asks for it.
            // Deferred a frame (it needs a ctx) and spawned at terminal_root() — project, else ~.
            restore_terminal_pending: true,
            lazy_carets: HashMap::new(),
            restoring_session: false,
            last_session_save: std::time::Instant::now(),
            fix_request_gen: None,
            fix_menu: None,
            fix_menu_title: "Quick Fixes",
            chsig: None,
            chsig_refs_gen: None,
            chsig_req_seq: 0,
            fix_request_refactor: false,
            git_panel: GitPanel::default(),
            goto_line: None,
            rename: None,
            usages: Vec::new(),
            usages_gen: None,
            call_hierarchy_gen: None,
            usages_are_callers: false,
            usages_from_index: false,
            completion: None,
            completion_gen: None,
            resolve_pending: None,
            action_resolve_pending: None,
            sig_help: None,
            sig_gen: None,
            outline: Vec::new(),
            outline_for: None,
            outline_requested: None,
            right_tab: RightTab::default(),
            run_cfgs: runconfig::RunConfigStore::default(),
            checklist: checklist::Checklist::default(),
            ai_panel: ai_actions::AiPanel::new(),
            ai_edit: None,
            autosave_deadline: None,
            def_choices: None,
            conflict_file: None,
            recent_locations: None,
            debug_pending_build: None,
            history_ui: localhist::HistoryUi::default(),
            testrun: testrun::TestRunner::default(),
            symbols: symbols::SymbolIndex::default(),
            goto_symbol: symbols::GotoSymbolUi::default(),
            ws_symbols_gen: 0,
            ws_symbols_sent: None,
            editor_menu: None,
            ai: ai::AiCompleter::new(),
            clangd_restart: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            c_deps_kicked: false,
            deps_gen: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            auto_deps: prefs.auto_deps,
            ai_settings: prefs.ai.clone(),
            inlay_hints_on: prefs.inlay_hints,
            inline_blame_enabled: prefs.inline_blame,
            theme_choice: prefs.theme,
            system_theme_poll: 0.0,
            web_server: None,
            inlay_for: None,
            inlay_requested: None,
            nav: nav::NavHistory::default(),
            palette: palette::CommandPalette::default(),
            recent: Vec::new(),
            tab_drag: None,
            pane_spans: Vec::new(),
            pending_tab_closes: Vec::new(),
            standards: prefs.standards,
            hover_wait: None,
            hover_popup: None,
        };
        if app.no_project {
            // Nothing to walk/restore — surface the picker (recents listed first) as the
            // welcome state over the existing empty-group constellation.
            app.openfolder.open();
        } else {
            openfolder::record_recent(&root);
            // Boot-wave item 2: only the (cheap) json load stays synchronous; detection — the
            // ELF walk that was the single largest pre-paint cost (recon A1/A7) — runs on the
            // "cauldron-runconfig-detect" worker and is drained in update() via poll_detect.
            boot_trace::span("runconfig-load", || {
                app.run_cfgs = runconfig::RunConfigStore::load(&root);
            });
            let repaint = Arc::clone(&app.repaint);
            app.run_cfgs.kick_detect(&root, move || repaint());
            app.install_deps(false);
            let t = boot_trace::begin();
            let git0 = boot_trace::counter("git-subprocess");
            app.restore_session();
            if boot_trace::enabled() {
                let tabs: usize = app.groups.iter().map(|g| g.files.len()).sum();
                let loaded: usize = app
                    .groups
                    .iter()
                    .map(|g| g.files.iter().filter(|f| f.loaded).count())
                    .sum();
                let gits = boot_trace::counter("git-subprocess") - git0;
                boot_trace::end(
                    t,
                    "restore-session",
                    &format!("tabs={tabs} loaded={loaded} git-subprocesses={gits}"),
                );
            }
        }
        if let Some(f) = initial_file {
            app.open_file(f);
        }
        boot_trace::mark("App::new-exit");
        app
    }

    // ---- file/group plumbing -----------------------------------------------------------------

    fn find_file_mut(&mut self, path: &Path) -> Option<&mut OpenFile> {
        self.groups.iter_mut().flat_map(|g| g.files.iter_mut()).find(|f| f.path == path)
    }

    /// The LIVE text of an open buffer (any group) — the overlay lane ships this, not disk.
    /// Lazy (unloaded) tabs have no live text: their empty stub buffer must never shadow disk.
    fn buffer_text(&self, path: &Path) -> Option<String> {
        self.groups
            .iter()
            .flat_map(|g| g.files.iter())
            .find(|f| f.path == path && f.loaded)
            .map(|f| f.buffer.rope().to_string())
    }

    /// Is an OPEN buffer for `path` dirty? The PSI save lane uses this to classify a disk
    /// change as external (buffer diverges — keep its overlay authoritative) vs an IDE save
    /// (save() clears `dirty` before queueing, so buffer and disk just converged).
    fn buffer_is_dirty(&self, path: &Path) -> bool {
        self.groups.iter().flat_map(|g| g.files.iter()).any(|f| f.path == path && f.dirty)
    }

    /// Route watcher-touched, universe-member files into the incremental index lanes: C files
    /// to the PSI save lane (hash-keyed; our own just-saved files no-op), everything to the
    /// symbol re-extraction (full rebuild fallback while a build stream is inflight).
    fn route_incremental(&mut self, files: Vec<PathBuf>, ctx: &egui::Context) {
        for f in &files {
            let is_c = matches!(f.extension().and_then(|e| e.to_str()), Some("c") | Some("h"));
            if is_c && !self.psi_saved_files.contains(f) {
                self.psi_saved_files.push(f.clone());
            }
        }
        // Symbols: re-extract just the touched files; if a build stream is already inflight
        // (its batches would be orphaned) or a full rebuild is already pending (it supersedes
        // per-file work), fall back to / stay on the full rebuild.
        self.symbols.poll();
        if self.symbols_rebuild_pending || self.symbols.is_building() {
            self.symbols_rebuild_pending = true;
        } else {
            self.symbols.refresh_files(&files, ctx);
        }
    }

    /// Move `path` to the front of the recent-files list (Ctrl+E). Deduped, bounded, and only for
    /// real files — the no-project sentinel is never a file. Restored session tabs count as
    /// "recent" too, which is what you want after reopening a project.
    fn record_recent_file(&mut self, path: &Path) {
        self.recent.retain(|p| p != path);
        self.recent.insert(0, path.to_path_buf());
        self.recent.truncate(50);
    }

    /// Open `path` in the focused group — activating an existing tab (any group) if present.
    fn open_file(&mut self, path: PathBuf) {
        self.record_recent_file(&path);
        for (gi, g) in self.groups.iter().enumerate() {
            if let Some(i) = g.files.iter().position(|f| f.path == path) {
                self.focused = gi;
                self.groups[gi].active = i;
                // A restored-lazy tab hydrates NOW: callers jump carets right after open_file.
                self.ensure_active_loaded(gi);
                return;
            }
        }
        match OpenFile::load(path) {
            Ok(mut f) => {
                if let Some(bps) = self.breakpoints.get(&f.path) {
                    f.view.breakpoint_lines = bps.iter().map(|(l, _)| (*l - 1) as usize).collect();
                }
                if let Some(marks) = self.bookmarks.get(&f.path) {
                    f.view.bookmark_lines = marks.iter().map(|l| l.saturating_sub(1) as usize).collect();
                }
                if let Some(lang) = f.lang {
                    self.lsp.open_doc(
                        lang,
                        &self.workspace.root,
                        &f.path,
                        &f.buffer.rope().to_string(),
                    );
                }
                let path = f.path.clone();
                let g = &mut self.groups[self.focused];
                g.files.push(f);
                g.active = g.files.len() - 1;
                self.error = None;
                self.refresh_nasa_squiggles();
                self.refresh_gutter(&path);
                let merged = self.diags.merged(&path);
                if let Some(f) = self.find_file_mut(&path) {
                    f.view.set_diagnostics(merged);
                }
            }
            Err(e) => self.error = Some(format!("failed to open: {e}")),
        }
    }

    /// Hydrate group `gi`'s ACTIVE tab if it is a lazy session placeholder (boot-wave item 4):
    /// synchronous single-file load, parked session caret, then exactly the wiring open_file
    /// gives a fresh tab (breakpoints, LSP didOpen — first-per-language spawn included —
    /// squiggles, gutter, stored diagnostics). A vanished file closes its tab.
    fn ensure_active_loaded(&mut self, gi: usize) {
        let Some(g) = self.groups.get(gi) else { return };
        let i = g.active;
        let Some(f) = g.files.get(i) else { return };
        if f.loaded {
            return;
        }
        let path = f.path.clone();
        let pending = self.lazy_carets.remove(&path);
        match self.groups[gi].files[i].hydrate(pending) {
            Ok(()) => {
                if let Some(bps) = self.breakpoints.get(&path) {
                    let lines = bps.iter().map(|(l, _)| (*l - 1) as usize).collect();
                    self.groups[gi].files[i].view.breakpoint_lines = lines;
                }
                if let Some(marks) = self.bookmarks.get(&path) {
                    let lines = marks.iter().map(|l| l.saturating_sub(1) as usize).collect();
                    self.groups[gi].files[i].view.bookmark_lines = lines;
                }
                let (lang, text) = {
                    let f = &self.groups[gi].files[i];
                    (f.lang, f.buffer.rope().to_string())
                };
                if let Some(lang) = lang {
                    self.lsp.open_doc(lang, &self.workspace.root, &path, &text);
                }
                self.refresh_nasa_squiggles();
                self.refresh_gutter(&path);
                let merged = self.diags.merged(&path);
                if let Some(f) = self.find_file_mut(&path) {
                    f.view.set_diagnostics(merged);
                }
            }
            Err(e) => {
                // The file disappeared between sessions: drop the placeholder tab (leaving it
                // unloaded would retry every frame; there is nothing to show).
                self.error = Some(format!("failed to open: {e}"));
                self.close_tab(gi, i);
            }
        }
    }

    /// Close one tab at the user's request — parks behind the unsaved-changes modal when the
    /// buffer is dirty. Programmatic closes (project switch after flush, vanished files, tree
    /// deletes the user already confirmed) call [`close_tab`](Self::close_tab) directly.
    fn request_close_tab(&mut self, gi: usize, i: usize) {
        let Some(p) = self.groups.get(gi).and_then(|g| g.files.get(i)).map(|f| f.path.clone())
        else {
            return;
        };
        self.request_close_tabs(gi, vec![p]);
    }

    /// Close `paths` in group `gi`. AUTO-SAVE POLICY: closing saves, it never asks (the
    /// JetBrains model — see also the idle auto-save in update()). Only a FAILED write keeps
    /// the affected tabs open, with the error in the status line.
    fn request_close_tabs(&mut self, gi: usize, paths: Vec<PathBuf>) {
        if self.save_dirty(Some(&paths)) {
            self.close_tabs_by_path(gi, &paths);
        }
    }

    /// The mechanical half of a multi-tab close: indices shift as tabs go, so each target is
    /// re-located by path. Group collapse is safe here — an emptied group can only be the LAST
    /// close (every caller keeps at least one tab or closes back-to-front within one group).
    fn close_tabs_by_path(&mut self, gi: usize, paths: &[PathBuf]) {
        for p in paths {
            if let Some(i) =
                self.groups.get(gi).and_then(|g| g.files.iter().position(|f| &f.path == p))
            {
                self.close_tab(gi, i);
            }
        }
    }

    fn close_tab(&mut self, gi: usize, i: usize) {
        let Some(g) = self.groups.get_mut(gi) else { return };
        if i >= g.files.len() {
            return;
        }
        let path = g.files[i].path.clone();
        let was_c = matches!(g.files[i].lang, Some(Lang::C) | Some(Lang::Cpp));
        let was_loaded = g.files[i].loaded;
        g.files.remove(i);
        // Removing a tab LEFT of the active one shifts every later index down — without this
        // decrement the pane silently switched to the next file over (and Ctrl+W/save then
        // targeted a tab the user never picked).
        if i < g.active {
            g.active -= 1;
        }
        if g.active >= g.files.len() && !g.files.is_empty() {
            g.active = g.files.len() - 1;
        }
        // A never-activated lazy tab has no parked caret to keep.
        self.lazy_carets.remove(&path);
        // Only close the LSP doc when NO other group still shows it — and never for a lazy
        // placeholder (it was never didOpen'ed; nothing server-side to close).
        let still_open = self.groups.iter().any(|g| g.files.iter().any(|f| f.path == path));
        if !still_open && was_loaded {
            self.lsp.close_doc(&path);
            if was_c {
                // The buffer is gone: cancel any pending overlay tick (no text to read) and
                // tell the PSI worker to drop a live overlay + restore disk facts (drained in
                // update(), where a ctx exists). Saved closes are a cheap worker no-op.
                self.psi_overlay_pending.remove(&path);
                self.psi_closed_files.push(path.clone());
            }
        }
        // An emptied non-last group collapses. Same shift as tabs: removing a group LEFT of
        // the focused one must decrement `focused` or keyboard focus jumps one pane right.
        if self.groups.len() > 1 && self.groups[gi].files.is_empty() {
            self.groups.remove(gi);
            if gi < self.focused {
                self.focused -= 1;
            } else if self.focused >= self.groups.len() {
                self.focused = self.groups.len() - 1;
            }
        }
    }

    /// Close every open tab in every group, leaving one empty group focused.
    ///
    /// Routed through [`close_tab`](Self::close_tab) rather than clearing `groups` directly: each
    /// tab owes the LSP a `didClose` and the PSI worker an overlay drop, and dropping the buffers
    /// on the floor would leak both — the servers would keep diagnosing files from a project that
    /// is no longer open. Iterated back-to-front because `close_tab` shifts the tabs after `i` (and
    /// collapses an emptied non-last group).
    fn close_all_tabs(&mut self) {
        for gi in (0..self.groups.len()).rev() {
            for i in (0..self.groups[gi].files.len()).rev() {
                self.close_tab(gi, i);
            }
        }
        // close_tab collapses emptied groups but always keeps the last one; make it the focus.
        self.groups.truncate(1);
        self.groups[0].files.clear();
        self.groups[0].active = 0;
        self.focused = 0;
    }

    /// Ctrl+\: move the focused group's active tab into a split pane to the RIGHT (up to
    /// [`MAX_GROUPS`] side-by-side, JetBrains-style). Under the cap this OPENS a new pane; at the
    /// cap it moves the tab into the next pane over (wrapping), so files keep shuffling across the
    /// existing splits.
    ///
    /// Requires the source pane to hold at least two tabs — moving a pane's only tab would just
    /// recreate an identical single-tab pane and collapse the empty source (a no-op the user reads
    /// as "nothing happened"). With one tab open, open another first.
    fn split_move_right(&mut self) {
        let gi = self.focused;
        let Some(g) = self.groups.get(gi) else { return };
        if g.files.len() < 2 {
            return;
        }
        let active = self.groups[gi].active;
        let file = self.groups[gi].files.remove(active);
        let g = &mut self.groups[gi];
        if g.active >= g.files.len() {
            g.active = g.files.len() - 1; // still ≥1 tab (started with ≥2), never empty
        }
        let (grow, target) = split_destination(self.groups.len(), gi);
        if grow {
            self.groups.insert(target, EditorGroup { files: Vec::new(), active: 0 });
        }
        let t = &mut self.groups[target];
        t.files.push(file);
        t.active = t.files.len() - 1;
        self.focused = target;
    }

    /// Called once per frame after the panes lay out: while a tab is dragged, preview the drop
    /// zone under the pointer; on release, move/split the tab there. This is the JetBrains
    /// drag-a-tab-to-split gesture.
    fn finish_tab_drag(&mut self, ui: &mut egui::Ui) {
        let Some(drag) = self.tab_drag.as_ref() else { return };
        let (from_g, from_i, label) = (drag.from_group, drag.from_index, drag.label.clone());
        let ptr = ui.input(|i| i.pointer.interact_pos());
        let released = ui.input(|i| i.pointer.any_released());
        let down = ui.input(|i| i.pointer.any_down());
        let area = ui.max_rect();
        // The tab strip occupies the top TAB_H band of the editor area; a drop there is a
        // reorder/move, not a split. Below it, the editor body's edges are split zones.
        let strip_bottom = area.top() + style::sizes::TAB_H;
        let target = ptr.and_then(|p| resolve_drop(&self.pane_spans, p.x, p.y <= strip_bottom));

        if down && !released {
            if let Some((kind, pane)) = target {
                // Don't preview a no-op (dropping back onto the same pane's middle).
                if !(kind == DropKind::Move && pane == from_g) {
                    self.paint_drop_zone(ui, area, kind, pane);
                }
            }
            // A small floating label of the tab being dragged, following the pointer.
            if let Some(p) = ptr {
                let painter = ui.ctx().layer_painter(egui::LayerId::new(
                    egui::Order::Tooltip,
                    egui::Id::new("tab-drag-ghost"),
                ));
                let pos = p + egui::vec2(12.0, 8.0);
                let font = egui::FontId::proportional(13.0);
                let galley =
                    painter.layout_no_wrap(label, font, colors::TEXT());
                let bg = egui::Rect::from_min_size(pos, galley.size())
                    .expand2(egui::vec2(6.0, 3.0));
                painter.rect_filled(bg, 4.0, colors::BG_PANEL());
                painter.galley(pos, galley, colors::TEXT());
            }
            return;
        }
        // Pointer released (or lost): resolve and clear the drag.
        self.tab_drag = None;
        // Only act on a release INSIDE the editor area — dropping on the terminal, the tree, or off
        // the window just cancels (the tab stays put).
        if released {
            if let (Some(p), Some((kind, pane))) = (ptr, target) {
                if area.contains(p) {
                    self.perform_tab_drop(from_g, from_i, kind, pane);
                }
            }
        }
    }

    /// Translucent highlight of where a dropped tab will land: the whole pane for a move, or the
    /// left/right half for a split.
    fn paint_drop_zone(&self, ui: &egui::Ui, area: egui::Rect, kind: DropKind, pane: usize) {
        let Some(&(l, r)) = self.pane_spans.get(pane) else { return };
        let zone = match kind {
            DropKind::Move => egui::Rect::from_min_max(
                egui::pos2(l, area.top()),
                egui::pos2(r, area.bottom()),
            ),
            DropKind::SplitLeft => egui::Rect::from_min_max(
                egui::pos2(l, area.top()),
                egui::pos2(l + (r - l) * 0.5, area.bottom()),
            ),
            DropKind::SplitRight => egui::Rect::from_min_max(
                egui::pos2(r - (r - l) * 0.5, area.top()),
                egui::pos2(r, area.bottom()),
            ),
        };
        let painter = ui.ctx().layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("tab-drop-zone"),
        ));
        painter.rect(
            zone,
            4.0,
            egui::Color32::from_rgba_premultiplied(60, 30, 10, 60),
            egui::Stroke::new(2.0, colors::ACCENT()),
        );
    }

    /// Move the dragged tab (`from_g`:`from_i`) to `pane` per `kind`. Split-under-cap inserts a new
    /// pane; at the cap it drops into the bordering pane. No-ops (same pane, a single-tab split, or
    /// a file already open in the target) are skipped. Index-shift-proof: it collapses emptied
    /// panes and re-locates focus by the moved file's PATH rather than tracking shifting indices.
    fn perform_tab_drop(&mut self, from_g: usize, from_i: usize, kind: DropKind, pane: usize) {
        if from_g >= self.groups.len() || from_i >= self.groups[from_g].files.len() {
            return;
        }
        if kind == DropKind::Move && pane == from_g {
            return; // dropped back on its own pane
        }
        let is_split = matches!(kind, DropKind::SplitLeft | DropKind::SplitRight);
        if is_split && self.groups[from_g].files.len() < 2 {
            return; // splitting a lone tab just recreates the same pane
        }
        let path = self.groups[from_g].files[from_i].path.clone();
        let last = self.groups.len() - 1;
        let at_cap = self.groups.len() >= MAX_GROUPS;
        // Resolve to a concrete op: push INTO an existing pane, or INSERT a new pane at a position.
        let (insert, pos) = match kind {
            DropKind::Move => (false, pane),
            DropKind::SplitLeft if at_cap => (false, pane),
            DropKind::SplitRight if at_cap => (false, (pane + 1).min(last)),
            DropKind::SplitLeft => (true, pane),
            DropKind::SplitRight => (true, pane + 1),
        };
        if !insert {
            // Into an existing pane — a no-op if that's the source, or the file is already there.
            if pos == from_g || self.groups[pos].files.iter().any(|f| f.path == path) {
                return;
            }
        }
        let file = self.groups[from_g].files.remove(from_i);
        if self.groups[from_g].active >= self.groups[from_g].files.len() {
            self.groups[from_g].active = self.groups[from_g].files.len().saturating_sub(1);
        }
        if insert {
            self.groups.insert(pos, EditorGroup { files: vec![file], active: 0 });
        } else {
            let g = &mut self.groups[pos];
            g.files.push(file);
            g.active = g.files.len() - 1;
        }
        // Collapse any pane emptied by the move, then re-focus the moved file by path.
        self.groups.retain(|g| !g.files.is_empty());
        if self.groups.is_empty() {
            self.groups.push(EditorGroup { files: Vec::new(), active: 0 });
        }
        if let Some(gi) = self.groups.iter().position(|g| g.files.iter().any(|f| f.path == path)) {
            self.focused = gi;
            if let Some(fi) = self.groups[gi].files.iter().position(|f| f.path == path) {
                self.groups[gi].active = fi;
            }
        }
        self.focused = self.focused.min(self.groups.len().saturating_sub(1));
    }

    /// Compute git gutter marks for `path` by diffing the SAVED file against HEAD
    /// (`git diff -U0`): added / modified line ranges + deletion boundaries.
    fn refresh_gutter(&mut self, path: &Path) {
        self.blame.invalidate(path); // disk content changed → blame is stale
        if self.restoring_session {
            return; // the restore tail issues ONE batched diff for everything it loaded
        }
        let root = self.workspace.root.clone();
        let Ok(rel) = path.strip_prefix(&root) else { return };
        let rel = rel.to_path_buf();
        boot_trace::count("git-subprocess", 1);
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["diff", "--no-color", "-U0", "HEAD", "--"])
            .arg(&rel)
            .output();
        let marks = match out {
            Ok(o) if o.status.success() => parse_diff_gutter(&String::from_utf8_lossy(&o.stdout)),
            _ => Vec::new(),
        };
        if let Some(f) = self.find_file_mut(path) {
            f.view.set_gutter_marks(marks);
        }
    }

    /// Gutter marks for MANY files with ONE `git diff` subprocess (session restore used to
    /// spawn one per tab — recon A5). Files absent from the diff (unchanged, untracked, or
    /// outside the repo) get empty marks, exactly like the per-file path.
    fn refresh_gutter_batch(&mut self, paths: &[PathBuf]) {
        let root = self.workspace.root.clone();
        let rels: Vec<PathBuf> = paths
            .iter()
            .filter_map(|p| p.strip_prefix(&root).ok().map(PathBuf::from))
            .collect();
        if rels.is_empty() {
            return;
        }
        boot_trace::count("git-subprocess", 1);
        let per_file = batch_gutter_diff(&root, &rels);
        for path in paths {
            let Ok(rel) = path.strip_prefix(&root) else { continue };
            let marks =
                per_file.get(rel.to_string_lossy().as_ref()).cloned().unwrap_or_default();
            if let Some(f) = self.find_file_mut(path) {
                f.view.set_gutter_marks(marks);
            }
        }
    }

    fn save(&mut self) {
        let mut ok = false;
        if let Some(f) = self.groups[self.focused].active_file() {
            if !f.loaded {
                return; // lazy stub: writing its empty buffer would TRUNCATE the real file
            }
            let text = f.buffer.rope().to_string();
            match std::fs::write(&f.path, &text) {
                Ok(()) => {
                    localhist::record(&f.path, &text);
                    f.dirty = false;
                    let is_c = matches!(f.lang, Some(Lang::C) | Some(Lang::Cpp));
                    let p = f.path.clone();
                    self.lsp.did_save(&p);
                    if is_c {
                        // Incremental, not full: the saved path is drained in update() (where
                        // a ctx exists) into PsiService::file_saved. Disk truth supersedes any
                        // pending overlay tick — buffer and disk just converged.
                        self.psi_overlay_pending.remove(&p);
                        self.psi_saved_files.push(p.clone());
                        if self.standards != Standards::Off {
                            self.check_format(&p);
                        }
                    }
                    ok = true;
                }
                Err(e) => self.error = Some(format!("save failed: {e}")),
            }
        }
        if ok {
            self.ws_refresh_pending = true; // async re-walk, drained in update()
            if let Some(f) = self.groups[self.focused].active_file() {
                let p = f.path.clone();
                self.refresh_gutter(&p);
            }
        }
    }

    /// Write every dirty buffer to disk. Returns false if ANY write failed.
    ///
    /// Cauldron has no autosave: `save()` is Ctrl+S and nothing else writes a buffer. That was
    /// survivable while a project switch KEPT its tabs open — the edits stayed in memory and you
    /// could still save them. Now that switching closes them, an unflushed dirty buffer would be
    /// destroyed silently, so the switch flushes first (and, on failure, does not happen at all).
    /// Each write also lands in local history, so the pre-switch content is recoverable.
    fn flush_dirty_buffers(&mut self) -> bool {
        self.save_dirty(None)
    }

    /// Write dirty buffers to disk — every one, or with `only` just those paths (the
    /// unsaved-changes modal saves exactly the tabs being closed). Returns false if ANY write
    /// failed.
    fn save_dirty(&mut self, only: Option<&[PathBuf]>) -> bool {
        // Collected first: writing borrows `self.lsp`/`self.error` while the buffers are borrowed.
        // A lazy stub is skipped — its buffer is empty and writing it would TRUNCATE the real file.
        let dirty: Vec<(PathBuf, String)> = self
            .groups
            .iter()
            .flat_map(|g| g.files.iter())
            .filter(|f| f.dirty && f.loaded && only.is_none_or(|ps| ps.contains(&f.path)))
            .map(|f| (f.path.clone(), f.buffer.rope().to_string()))
            .collect();
        let mut ok = true;
        for (path, text) in dirty {
            match std::fs::write(&path, &text) {
                Ok(()) => {
                    localhist::record(&path, &text);
                    self.lsp.did_save(&path);
                    if let Some(f) = self.find_file_mut(&path) {
                        f.dirty = false;
                    }
                }
                Err(e) => {
                    self.error = Some(format!("save failed: {}: {e}", path.display()));
                    ok = false;
                }
            }
        }
        ok
    }

    /// Reload open buffers whose on-disk copy changed under us (git checkout, external editor,
    /// codegen, a formatter). Only CLEAN buffers reload — a dirty tab keeps its unsaved edits and
    /// is reported as a conflict rather than silently clobbered. Our own saves are a no-op here:
    /// buffer == disk, so nothing differs.
    fn reload_externally_changed_buffers(&mut self, ctx: &egui::Context) {
        let now = ctx.input(|i| i.time);
        // Phase 1: mutate the buffers, collecting what LSP/UI need refreshed afterwards.
        let mut synced: Vec<(PathBuf, Vec<(Rope, cauldron_editor::Transaction)>, Rope)> = Vec::new();
        let mut conflicts = 0usize;
        for g in &mut self.groups {
            for f in &mut g.files {
                if !f.loaded {
                    continue;
                }
                let Ok(disk) = std::fs::read_to_string(&f.path) else { continue };
                if disk == f.buffer.rope().to_string() {
                    continue; // unchanged (includes our own just-written saves)
                }
                if f.dirty {
                    conflicts += 1; // unsaved edits — keep them, don't reload
                    continue;
                }
                let pre = f.buffer.rope().clone();
                let tx = cauldron_editor::Transaction::replace(0, pre.len_bytes(), disk);
                f.view.apply_external(&mut f.buffer, &tx, now);
                let post = f.buffer.rope().clone();
                // Drain the edit this produced HERE, so the didChange goes out before anything
                // else can talk to the server this frame (the per-frame take_edits loop also
                // visits every loaded tab, but only later in update()).
                let edits = f.view.take_edits();
                synced.push((f.path.clone(), edits, post));
            }
        }
        // Phase 2: keep the language server in sync and refresh derived UI for each reloaded file.
        for (path, edits, post) in &synced {
            for (i, (pre, tx)) in edits.iter().enumerate() {
                let post_i: Rope =
                    edits.get(i + 1).map(|(p, _)| p.clone()).unwrap_or_else(|| post.clone());
                self.lsp.did_change(path, pre, &post_i, tx);
                self.diags.map_through(path, tx);
            }
            self.refresh_gutter(path);
            let merged = self.diags.merged(path);
            if let Some(f) = self.find_file_mut(path) {
                f.view.set_diagnostics(merged);
            }
        }
        if !synced.is_empty() {
            let n = synced.len();
            self.lsp_message = Some(format!("reloaded {n} file(s) changed on disk"));
        }
        if conflicts > 0 {
            self.lsp_message =
                Some(format!("{conflicts} open file(s) changed on disk — unsaved edits kept"));
        }
    }

    fn open_folder(&mut self, dir: PathBuf) {
        // Unsaved work FIRST, before ANY teardown: a failed write (read-only mount, vanished
        // dir) ABORTS the switch outright, and aborting is only honest if nothing has been
        // destroyed yet — this gate used to sit after the debug/bookmark clears, so a failed
        // switch left the user in their project with every breakpoint, condition, bookmark and
        // watch already wiped (and the next autosave persisted the empty sets over the old
        // project's session file). The error is already on screen.
        if !self.flush_dirty_buffers() {
            return;
        }
        // Order matters: the OLD project's session is captured (tabs, carets, pins, panels,
        // breakpoints, bookmarks) before anything is torn down, so switching away and back
        // reopens exactly what was on screen — capture_session reads self.breakpoints and
        // self.bookmarks, so this must precede the clears below too.
        self.save_session();
        self.blame.clear();
        // Debug/test state is PER-PROJECT: without this, project A's breakpoints show in B's
        // Breakpoints list, replay into B's debug launches, and get WRITTEN INTO B's session
        // on the next autosave — cross-contamination that never ages out.
        self.dap.stop();
        for path in self.breakpoints.keys().cloned().collect::<Vec<_>>() {
            self.dap.set_breakpoints(&path, Vec::new());
        }
        self.breakpoints.clear();
        self.bookmarks.clear();
        self.bookmarks_open = false;
        self.watches.clear();
        self.watch_vals.clear();
        self.test_decls.clear();
        self.coverage.files.clear();
        // The old project's files now CLOSE. A tab from the project you just left is never what you
        // want: its path is outside the new root (invisible to the tree, the search, the git
        // gutter), and leaving it open would also let it bleed into the NEW project's session on
        // the next save. Pins and parked carets belong to that project too and go with it — the
        // restore below repopulates all of it from the new root's own session.
        self.close_all_tabs();
        self.pins.clear();
        self.lazy_carets.clear();
        self.nav.clear(); // the jump list points into the old project's files
        self.recent.clear(); // recent files belonged to the old project
        // Problems belong to the project that produced them: a stale entry here would list a file
        // outside the new root and open it on click.
        self.diags.clear();
        self.error = None;
        self.no_project = false;
        self.workspace = Workspace::open(dir.clone());
        // Shells are re-rooted at the new project (the old project's are SIGHUPed): a shell's cwd
        // is fixed at spawn, so surviving ones would answer for the wrong tree.
        let shell_root = self.terminal_root();
        self.terminal.set_root(&shell_root);
        openfolder::record_recent(&dir);
        state::save_last_project(&dir);
        // Point the file watcher at the new root (recreated lazily in update()).
        self.watcher = None;
        // Clear BOTH indexes NOW: switching projects must never show stale symbols/findings,
        // not even for the frames until the rebuild kicks land.
        self.symbols.clear();
        self.symbols_rebuild_pending = true;
        self.psi.invalidate();
        self.psi_saved_files.clear();
        self.psi_overlay_pending.clear();
        self.psi_closed_files.clear();
        self.psi_scan_pending = true;
        self.rescan_after_refresh = false;
        self.fs_pending_universe.clear();
        self.c_deps_kicked = false;
        // Fresh store (fresh channel — any inflight detect for the OLD root sends into a
        // dropped receiver); detection for the new root runs on the background worker.
        self.run_cfgs = runconfig::RunConfigStore::load(&dir);
        let repaint = Arc::clone(&self.repaint);
        self.run_cfgs.kick_detect(&dir, move || repaint());
        self.install_deps(false);
        self.restore_session();
    }

    /// Where terminal shells start: the project root — or `$HOME` in no-project mode, where
    /// the workspace root is a sentinel that does not exist (spawning there would fail).
    /// Where a shell opens: the project root — or `~` whenever there is no real directory to stand
    /// in. Two ways that happens: no-project mode, whose root is a SENTINEL path that by design
    /// never exists (`state::no_project_root`), and a project dir renamed or deleted under a live
    /// session.
    ///
    /// The existence check is the point. cider's PTY sets the cwd unconditionally
    /// (`pty.rs`: `Some(dir) => cmd.cwd(dir)`) and only falls back to `$HOME` when handed `None` —
    /// so a path that isn't there doesn't degrade, it makes the shell spawn FAIL. This must never
    /// return one. `/` is the last resort purely because it always exists.
    fn terminal_root(&self) -> PathBuf {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        shell_cwd(self.no_project, &self.workspace.root, home.as_deref())
    }

    /// Auto-resolve dependencies for EVERY ecosystem the project uses — cargo, npm/pnpm/yarn/bun,
    /// NuGet, pip (into the venv), Go, Maven/Gradle, and the rest ([`depsinstall`]) — in one
    /// background worker, so the first build or completion doesn't stall on the network.
    ///
    /// Content-stamped: unchanged manifests do no work, so repeat opens are free, while a `git
    /// checkout` to a branch with different deps re-resolves. `force` (the manual "Install
    /// dependencies" action) ignores the stamp. Honors the `auto_deps` switch — a user who does not
    /// want a just-opened tree's package scripts running unbidden can still invoke it by hand.
    fn install_deps(&mut self, force: bool) {
        if self.no_project || (!force && !self.auto_deps) {
            return;
        }
        let root = self.workspace.root.clone();
        // Bump FIRST: any in-flight worker from the previous project is now stale and stops
        // reporting (its packages stay installed; it just goes quiet).
        let generation = Arc::clone(&self.deps_gen);
        generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let status = Arc::clone(&self.bg_message);
        let repaint = Arc::clone(&self.repaint);
        depsinstall::install(root, force, generation, status, move || repaint());
    }

    fn run_project(&mut self, ctx: &egui::Context, run: bool) {
        // Save every dirty buffer first. Building/running the on-disk copy while the editor shows
        // unsaved edits runs code that isn't what you're looking at — the classic "why didn't my
        // change take effect" trap. (run_current_file already does this for single-file runs.)
        if !self.flush_dirty_buffers() {
            self.lsp_message = Some("save failed — not running stale code".into());
            return;
        }
        let root = self.workspace.root.clone();
        // A selected run configuration wins for RUN (build keeps the project default).
        if run {
            // RUN executes in a real PTY (terminal tab "run"): colors, progress bars, and
            // interactive stdin all work — piped output gave programs a dumb non-tty.
            if let Some(cfg) = self.run_cfgs.selected().cloned() {
                let (prog, args, cwd) = runconfig::command_line(&cfg, &root);
                let mut line = String::new();
                for (k, v) in &cfg.env {
                    line.push_str(&format!("{k}={} ", runconfig::sh_quote(v)));
                }
                line.push_str(&runconfig::sh_quote(&prog));
                for a in &args {
                    line.push(' ');
                    line.push_str(&runconfig::sh_quote(a));
                }
                self.terminal.run_command(&line, &cwd, ctx);
                return;
            }
            if root.join("Cargo.toml").exists() {
                self.terminal.run_command("cargo run", &root, ctx);
            } else if root.join("Makefile").exists() {
                self.terminal.run_command("make", &root, ctx);
            } else {
                self.lsp_message = Some("no Cargo.toml or Makefile at the project root".into());
            }
            return;
        }
        // BUILD stays on the piped Output pane — non-interactive by nature.
        if root.join("Cargo.toml").exists() {
            self.runner.start("cargo", &["build"], &root, ctx);
        } else if root.join("Makefile").exists() {
            self.runner.start("make", &[], &root, ctx);
        } else {
            self.lsp_message = Some("no Cargo.toml or Makefile at the project root".into());
        }
        self.bottom_open = true;
        self.bottom_tab = BottomTab::Output;
    }

    /// Ctrl+Shift+F10: run the file in the focused editor tab, whatever it is — a python script, a
    /// lone C file, a cargo example. The command comes from the extension
    /// ([`runconfig::RunKind::File`]); nothing is added to the project's saved configs, because
    /// "run this file once" is an action, not a configuration the user wants to curate.
    ///
    /// The file is SAVED first. Running the on-disk copy of a buffer you have edited would execute
    /// code that is not what you are looking at — the single most confusing thing a Run button can
    /// do.
    fn run_current_file(&mut self, ctx: &egui::Context) {
        let Some((path, dirty)) =
            self.groups[self.focused].active_file().map(|f| (f.path.clone(), f.dirty))
        else {
            self.lsp_message = Some("no file to run".into());
            return;
        };
        // Save only a DIRTY buffer. save() writes unconditionally, and rewriting an unchanged file
        // bumps its mtime — which for a compiled language forces cargo/make to rebuild the whole
        // crate on every single Run. A clean buffer already matches disk; leave it, and the
        // incremental build stays incremental.
        if dirty {
            self.save();
        }
        let cfg = runconfig::RunConfig {
            name: format!("run {}", path.file_name().unwrap_or_default().to_string_lossy()),
            kind: runconfig::RunKind::File,
            program: path.to_string_lossy().into_owned(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
        };
        // In no-project mode the workspace root is a SENTINEL that by design never exists, so
        // standing in it would make the spawn fail outright — and "open one file, run it" is
        // precisely what that mode is for. Stand in the file's own directory instead.
        let root = match self.no_project || !self.workspace.root.is_dir() {
            true => path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from("/")),
            false => self.workspace.root.clone(),
        };
        let (prog, args, cwd) = runconfig::command_line(&cfg, &root);
        let arg_refs: Vec<&str> = args.iter().map(|a| a.as_str()).collect();
        self.runner.start(&prog, &arg_refs, &cwd, ctx);
        self.bottom_open = true;
        self.bottom_tab = BottomTab::Output;
    }

    /// Route a debugger message where the user is actually looking: the Debug tab.
    fn dbg_say(&mut self, msg: impl Into<String>) {
        self.dbg_console.push(msg.into());
        self.bottom_open = true;
        self.bottom_tab = BottomTab::Debug;
    }

    /// Launch the debugger for the current context: an active .py file goes through debugpy;
    /// a Cargo workspace debugs target/debug/<root-name>; any other workspace (C/C++, e.g. cFS)
    /// scans for BUILT executables and debugs the pick under lldb-dap.
    fn start_debug(&mut self, ctx: &egui::Context) {
        if self.dap.is_running() {
            self.dbg_say("debug session already running — Stop it first");
            return;
        }
        let root = self.workspace.root.clone();
        // The selected run configuration also drives DEBUG.
        if let Some((bin, args, cwd, adapter)) =
            self.run_cfgs.selected().and_then(|c| runconfig::debug_target(c, &root))
        {
            // RunConfig.env reaches the DEBUGGEE too — Run honored it, debug dropped it.
            let env = self.run_cfgs.selected().map(|c| c.env.clone()).unwrap_or_default();
            use runconfig::DebugAdapter;
            let r = match adapter {
                DebugAdapter::Python => {
                    let py = debugpy_env(&root);
                    self.dap.launch_full(
                        cauldron_dap::AdapterKind::Debugpy,
                        &bin,
                        &args,
                        &cwd,
                        Some(py),
                        &env,
                    )
                }
                DebugAdapter::Dotnet => {
                    if !std::process::Command::new("netcoredbg").arg("--version").output().map(|o| o.status.success()).unwrap_or(false) {
                        self.dbg_say("netcoredbg not found — install it (e.g. samsung/netcoredbg) to debug .NET");
                        return;
                    }
                    self.dbg_say(format!("launching {} under netcoredbg…", bin.display()));
                    self.dap.launch_full(cauldron_dap::AdapterKind::Netcoredbg, &bin, &args, &cwd, None, &env)
                }
                DebugAdapter::Native => {
                    if !std::process::Command::new("lldb-dap").arg("--help").output().map(|o| o.status.success()).unwrap_or(false) {
                        self.dbg_say("lldb-dap not found — install lldb (sudo pacman -S lldb)");
                        return;
                    }
                    self.dbg_say(format!("launching {} under lldb-dap…", bin.display()));
                    self.dap.launch_full(cauldron_dap::AdapterKind::LldbDap, &bin, &args, &cwd, None, &env)
                }
            };
            self.after_launch(r);
            return;
        }
        let active = self.groups.get(self.focused).and_then(|g| g.files.get(g.active)).map(|f| f.path.clone());
        if let Some(p) = active.filter(|p| p.extension().and_then(|e| e.to_str()) == Some("py")) {
            let py = debugpy_env(&root);
            let r = self.dap.launch_with_python(
                cauldron_dap::AdapterKind::Debugpy,
                &p,
                &[],
                &root,
                Some(py),
            );
            self.after_launch(r);
            return;
        }
        if root.join("Cargo.toml").exists() {
            let name = root.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
            let bin = root.join("target/debug").join(&name);
            // BUILD-BEFORE-DEBUG: never debug a stale binary. cargo build streams into the
            // Output pane; the pending watcher in update() launches lldb when it exits 0.
            self.dbg_say(format!("building before debug (cargo build) → target/debug/{name}…"));
            self.runner.start("cargo", &["build"], &root, ctx);
            self.debug_pending_build = Some(bin);
            return;
        }
        // C/C++ workspace: debug a built executable.
        let mut exes = find_executables(&root);
        match exes.len() {
            0 => self.dbg_say(
                "no built executable found in the workspace — run Build (Ctrl+F9) first,                  then Debug again",
            ),
            1 => {
                let bin = exes.remove(0);
                self.launch_lldb(bin);
            }
            _ => {
                self.dbg_say(format!("{} executables found — pick one:", exes.len()));
                self.dbg_pick = exes;
            }
        }
    }

    fn launch_lldb(&mut self, bin: PathBuf) {
        if !std::process::Command::new("lldb-dap").arg("--help").output().map(|o| o.status.success()).unwrap_or(false) {
            self.dbg_say("lldb-dap not found — install lldb (sudo pacman -S lldb)");
            return;
        }
        let root = self.workspace.root.clone();
        // cFS-style binaries expect to run from their install dir (relative cf/ paths).
        let cwd = bin.parent().map(|p| p.to_path_buf()).unwrap_or(root);
        self.dbg_say(format!("launching {} under lldb-dap…", bin.display()));
        let r = self.dap.launch(cauldron_dap::AdapterKind::LldbDap, &bin, &[], &cwd);
        self.after_launch(r);
    }

    fn after_launch(&mut self, r: Result<(), String>) {
        match r {
            Ok(()) => {
                self.dbg_stack.clear();
                self.dbg_threads.clear();
                self.dbg_scopes.clear();
                self.dbg_vars.clear();
                    self.dbg_vars_pending.clear();
                self.dbg_pick.clear();
                self.bottom_open = true;
                self.bottom_tab = BottomTab::Debug;
            }
            Err(e) => self.dbg_say(format!("debug launch failed: {e}")),
        }
    }

    /// Paint the last coverage run's marks onto every open, loaded view.
    fn push_coverage_marks(&mut self) {
        for g in &mut self.groups {
            for f in &mut g.files {
                if !f.loaded {
                    continue;
                }
                let marks = self.coverage.files.get(&f.path).cloned().unwrap_or_default();
                f.view.set_coverage_marks(marks);
            }
        }
    }

    /// Push one file's breakpoint set to its open view (dots) and the DAP manager (adapter).
    fn sync_breakpoints(&mut self, path: &Path) {
        let bps = self.breakpoints.get(path).cloned().unwrap_or_default();
        if let Some(f) = self.find_file_mut(path) {
            f.view.breakpoint_lines = bps.iter().map(|(l, _)| (*l - 1) as usize).collect();
        }
        self.dap.set_breakpoints(path, bps);
    }

    /// Bookmarks ride their code exactly like breakpoints do (see [`Self::remap_breakpoints`]).
    fn remap_bookmarks(&mut self, path: &Path, pre: &Rope, tx: &cauldron_editor::Transaction) {
        let Some(marks) = self.bookmarks.get_mut(path) else { return };
        let mut changed = false;
        for ch in tx.changes.iter().rev() {
            let len = pre.len_bytes();
            let start_l = pre.byte_to_line(ch.start.min(len)) as i64;
            let end_l = pre.byte_to_line(ch.end.min(len)) as i64;
            let added = ch.text.bytes().filter(|b| *b == b'\n').count() as i64;
            let delta = added - (end_l - start_l);
            if delta == 0 {
                continue;
            }
            for l1 in marks.iter_mut() {
                let l0 = *l1 as i64 - 1;
                if l0 <= start_l {
                    continue;
                }
                let moved = if l0 <= end_l { start_l } else { l0 + delta };
                let moved = moved.max(0) as u32 + 1;
                if moved != *l1 {
                    *l1 = moved;
                    changed = true;
                }
            }
        }
        if changed {
            marks.sort_unstable();
            marks.dedup();
            self.sync_bookmarks(path);
        }
    }

    /// Push one file's bookmark set to its open view.
    fn sync_bookmarks(&mut self, path: &Path) {
        let marks = self.bookmarks.get(path).cloned().unwrap_or_default();
        if let Some(f) = self.find_file_mut(path) {
            f.view.bookmark_lines = marks.iter().map(|l| l.saturating_sub(1) as usize).collect();
        }
    }

    /// F11: toggle a bookmark on the focused file's caret line.
    fn toggle_bookmark(&mut self) {
        let Some(f) = self.groups[self.focused].active_file() else { return };
        if !f.loaded {
            return;
        }
        let path = f.path.clone();
        let line1 = f.buffer.rope().byte_to_line(f.view.caret_byte()) as u32 + 1;
        let marks = self.bookmarks.entry(path.clone()).or_default();
        match marks.binary_search(&line1) {
            Ok(i) => {
                marks.remove(i);
            }
            Err(i) => marks.insert(i, line1),
        }
        if marks.is_empty() {
            self.bookmarks.remove(&path);
        }
        self.sync_bookmarks(&path);
    }

    /// Shift `path`'s stored breakpoints through one transaction: an edit above a breakpoint
    /// moves it by the net line delta; a breakpoint whose lines were deleted pins to the edit
    /// start. View dots + the adapter are re-synced only when something actually moved.
    fn remap_breakpoints(&mut self, path: &Path, pre: &Rope, tx: &cauldron_editor::Transaction) {
        let Some(bps) = self.breakpoints.get_mut(path) else { return };
        let mut changed = false;
        // Process changes back-to-front so earlier shifts don't skew later comparisons
        // (a Transaction's changes are ascending + disjoint).
        for ch in tx.changes.iter().rev() {
            let len = pre.len_bytes();
            let start_l = pre.byte_to_line(ch.start.min(len)) as i64;
            let end_l = pre.byte_to_line(ch.end.min(len)) as i64;
            let added = ch.text.bytes().filter(|b| *b == b'\n').count() as i64;
            let delta = added - (end_l - start_l);
            if delta == 0 {
                continue;
            }
            for (l1, _) in bps.iter_mut() {
                let l0 = *l1 as i64 - 1;
                if l0 <= start_l {
                    continue; // edit at/below the breakpoint's line start — unaffected
                }
                let moved = if l0 <= end_l {
                    start_l // breakpoint's line was consumed by the edit: pin to its start
                } else {
                    l0 + delta
                };
                let moved = moved.max(0) as u32 + 1;
                if moved != *l1 {
                    *l1 = moved;
                    changed = true;
                }
            }
        }
        if changed {
            bps.sort_by_key(|(l, _)| *l);
            bps.dedup_by_key(|(l, _)| *l);
            self.sync_breakpoints(path);
        }
    }

    /// Fire (or re-fire) every watch expression against the current frame; results land as
    /// tagged [`cauldron_dap::DebugEvent::Evaluated`]s (tag = index + 1; 0 is the console).
    fn refresh_watches(&mut self) {
        self.watch_epoch += 1;
        self.watch_vals = vec![None; self.watches.len()];
        let frame = self.dbg_frame;
        for (i, expr) in self.watches.clone().into_iter().enumerate() {
            self.dap.evaluate_watch(&expr, frame, watch_tag(self.watch_epoch, i));
        }
    }

    /// Jump an (already open) file's view to a 0-based line.
    fn goto_file_line(&mut self, path: &Path, line: usize) {
        if let Some(f) = self.find_file_mut(path) {
            let rope = f.buffer.rope().clone();
            let byte = rope.line_to_byte(line.min(rope.len_lines().saturating_sub(1)));
            f.view.jump_to(byte, &rope);
        }
    }

    fn clear_debug_marks(&mut self) {
        for g in &mut self.groups {
            for f in &mut g.files {
                f.view.debug_line = None;
                f.view.set_debug_values(Vec::new());
            }
        }
    }

    /// Drain DAP events; jump the editor to the paused frame and prefetch its scopes.
    fn pump_debugger(&mut self) {
        for ev in self.dap.pump() {
            use cauldron_dap::DebugEvent as E;
            match ev {
                E::Output { text, .. } => {
                    for l in text.lines() {
                        self.dbg_console.push(l.to_string());
                    }
                    if self.dbg_console.len() > 5000 {
                        let cut = self.dbg_console.len() - 5000;
                        self.dbg_console.drain(..cut);
                    }
                }
                E::Stopped { reason, description, .. } => {
                    self.dbg_console.push(format!(
                        "— stopped: {reason}{}",
                        description.map(|d| format!(" ({d})")).unwrap_or_default()
                    ));
                    self.bottom_open = true;
                    self.bottom_tab = BottomTab::Debug;
                }
                E::Stack { frames } => {
                    if let Some(top) = frames.first() {
                        self.dbg_frame = Some(top.id);
                        self.dap.request_scopes(top.id);
                        self.refresh_watches();
                        if let Some(path) = top.path.clone() {
                            let line = top.line.saturating_sub(1) as usize;
                            self.open_file(path.clone());
                            if let Some(f) = self.find_file_mut(&path) {
                                f.view.debug_line = Some(line);
                                let byte = f.buffer.rope().line_to_byte(line.min(f.buffer.rope().len_lines().saturating_sub(1)));
                                let rope = f.buffer.rope().clone();
                                f.view.jump_to(byte, &rope);
                            }
                        }
                    }
                    self.dbg_stack = frames;
                    self.dbg_scopes.clear();
                    self.dbg_vars.clear();
                    self.dbg_vars_pending.clear();
                }
                E::Threads { threads } => self.dbg_threads = threads,
                E::Scopes { scopes, .. } => {
                    for sc in &scopes {
                        self.dbg_vars_pending.insert(sc.variables_reference);
                        self.dap.request_variables(sc.variables_reference);
                    }
                    self.dbg_scopes = scopes;
                }
                E::Variables { reference, vars } => {
                    self.dbg_vars_pending.remove(&reference);
                    self.dbg_vars.insert(reference, vars);
                    self.refresh_debug_values();
                }
                E::Continued => {
                    self.clear_debug_marks();
                    self.dbg_stack.clear();
                }
                E::Exited { code } => {
                    self.dbg_console.push(format!("— exited with code {code}"));
                    self.clear_debug_marks();
                    self.dbg_stack.clear();
                    self.dbg_threads.clear();
                }
                E::Terminated => {
                    self.dbg_console.push("— terminated".into());
                    self.clear_debug_marks();
                    self.dbg_stack.clear();
                    self.dbg_threads.clear();
                }
                E::Evaluated { tag: 0, result } => {
                    self.dbg_console.push(format!("= {result}"));
                }
                E::Evaluated { tag, result } => {
                    // Epoch-tagged: a result from before a watch removal/refresh must not land
                    // in a shifted (wrong) row.
                    let (epoch, i) = watch_untag(tag);
                    if epoch == self.watch_epoch {
                        if let Some(slot) = self.watch_vals.get_mut(i) {
                            *slot = Some(result);
                        }
                    }
                }
                E::BreakpointsResolved { .. } => {}
                E::Started => {
                    // Fresh session: last session's values/frame are meaningless.
                    self.dbg_frame = None;
                    self.watch_epoch += 1;
                    self.watch_vals = vec![None; self.watches.len()];
                }
                E::Error(e) => self.dbg_console.push(format!("! {e}")),
            }
        }
    }

    /// The Debug bottom tab: controls · stack · variables · console.
    fn debug_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let stopped = self.dap.is_stopped();
            let running = self.dap.is_running();
            if !running {
                if ui.button("▶ Debug").on_hover_text("Shift+F9").clicked_by(egui::PointerButton::Primary) {
                    let ctx = ui.ctx().clone();
                    self.start_debug(&ctx);
                }
            } else {
                if ui.add_enabled(stopped, egui::Button::new("▶ Continue")).on_hover_text("F9").clicked_by(egui::PointerButton::Primary) {
                    self.dap.continue_run();
                }
                if ui.add_enabled(stopped, egui::Button::new("⤵ Over")).on_hover_text("F8").clicked_by(egui::PointerButton::Primary) {
                    self.dap.next();
                }
                if ui.add_enabled(stopped, egui::Button::new("⤷ Into")).on_hover_text("F7").clicked_by(egui::PointerButton::Primary) {
                    self.dap.step_in();
                }
                if ui.add_enabled(stopped, egui::Button::new("⤴ Out")).on_hover_text("Shift+F8").clicked_by(egui::PointerButton::Primary) {
                    self.dap.step_out();
                }
                {
                    let mut ex = self.dap.break_on_exceptions();
                    if ui
                        .checkbox(&mut ex, "Exceptions")
                        .on_hover_text("Break on uncaught exceptions (applies live and to future sessions)")
                        .changed()
                    {
                        self.dap.set_break_on_exceptions(ex);
                    }
                }
                if ui.add_enabled(!stopped, egui::Button::new("⏸ Pause")).clicked_by(egui::PointerButton::Primary) {
                    self.dap.pause();
                }
                if ui.button("⏹ Stop").clicked_by(egui::PointerButton::Primary) {
                    self.dap.stop();
                    self.clear_debug_marks();
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.colored_label(colors::TEXT_FAINT(), self.dap.state());
            });
        });
        style::hairline(ui);
        if !self.dbg_pick.is_empty() {
            let picks = self.dbg_pick.clone();
            let root = self.workspace.root.clone();
            ui.colored_label(colors::TEXT_FAINT(), "Choose the executable to debug:");
            for bin in picks {
                let rel = bin.strip_prefix(&root).unwrap_or(&bin).display().to_string();
                if ui.selectable_label(false, egui::RichText::new(rel).size(12.0).monospace()).clicked_by(egui::PointerButton::Primary) {
                    self.launch_lldb(bin.clone());
                }
            }
            style::hairline(ui);
        }
        let avail_h = ui.available_height();
        ui.horizontal_top(|ui| {
            let col_w = ui.available_width() / 3.0 - 8.0;
            // --- stack ---
            ui.allocate_ui(egui::vec2(col_w, avail_h), |ui| {
                // --- breakpoints: every file's set, editable conditions, removable ------------
                if !self.breakpoints.is_empty() {
                    let n: usize = self.breakpoints.values().map(|v| v.len()).sum();
                    egui::CollapsingHeader::new(
                        egui::RichText::new(format!("Breakpoints ({n})")).size(12.0),
                    )
                    .default_open(false)
                    .show(ui, |ui| {
                        // (file, line, condition) rows; edits are applied after the borrow ends.
                        let mut edits: Vec<(PathBuf, u32, Option<String>)> = Vec::new();
                        let mut removals: Vec<(PathBuf, u32)> = Vec::new();
                        let root = self.workspace.root.clone();
                        let mut files: Vec<_> = self.breakpoints.iter().collect();
                        files.sort_by(|a, b| a.0.cmp(b.0));
                        for (path, bps) in files {
                            let rel = path.strip_prefix(&root).unwrap_or(path);
                            for (line, cond) in bps {
                                ui.horizontal(|ui| {
                                    if ui.small_button("✕").clicked_by(egui::PointerButton::Primary) {
                                        removals.push((path.clone(), *line));
                                    }
                                    ui.colored_label(
                                        colors::TEXT_MUTED(),
                                        egui::RichText::new(format!("{}:{line}", rel.display()))
                                            .size(11.5)
                                            .monospace(),
                                    );
                                    // Draft-buffered: the row edits a scratch string and only
                                    // COMMITS on focus loss — editing per keystroke both ate
                                    // interior spaces (trim-per-char) and fired a setBreakpoints
                                    // request at a live adapter for every character typed.
                                    let key = (path.clone(), *line);
                                    let editing =
                                        self.bp_cond_draft.as_ref().map(|(k, _)| k) == Some(&key);
                                    let mut c = if editing {
                                        self.bp_cond_draft.as_ref().unwrap().1.clone()
                                    } else {
                                        cond.clone().unwrap_or_default()
                                    };
                                    let resp = ui.add(
                                        egui::TextEdit::singleline(&mut c)
                                            .hint_text("condition…")
                                            .desired_width(120.0)
                                            .font(egui::TextStyle::Small),
                                    );
                                    if resp.changed() || (resp.has_focus() && !editing) {
                                        self.bp_cond_draft = Some((key.clone(), c.clone()));
                                    }
                                    if editing && resp.lost_focus() {
                                        self.bp_cond_draft = None;
                                        let v = c.trim();
                                        edits.push((
                                            path.clone(),
                                            *line,
                                            (!v.is_empty()).then(|| v.to_string()),
                                        ));
                                    }
                                });
                            }
                        }
                        for (path, line, cond) in edits {
                            if let Some(bps) = self.breakpoints.get_mut(&path) {
                                if let Some(bp) =
                                    bps.iter_mut().find(|(l, _)| *l == line)
                                {
                                    bp.1 = cond;
                                }
                            }
                            self.sync_breakpoints(&path);
                        }
                        for (path, line) in removals {
                            if let Some(bps) = self.breakpoints.get_mut(&path) {
                                bps.retain(|(l, _)| *l != line);
                                if bps.is_empty() {
                                    self.breakpoints.remove(&path);
                                }
                            }
                            self.sync_breakpoints(&path);
                        }
                    });
                }
                // --- threads (only worth showing when there's more than one) ---
                if self.dbg_threads.len() > 1 {
                    ui.colored_label(colors::TEXT_FAINT(), format!("Threads ({})", self.dbg_threads.len()));
                    let active = self.dap.active_thread();
                    let threads = self.dbg_threads.clone();
                    let mut switch: Option<i64> = None;
                    egui::ScrollArea::vertical().id_salt("dbg-threads").max_height(90.0).auto_shrink([false, true]).show(ui, |ui| {
                        for (id, name) in &threads {
                            let sel = active == Some(*id);
                            let label = format!("#{id}  {name}");
                            if ui
                                .selectable_label(sel, egui::RichText::new(label).size(12.0).monospace())
                                .clicked_by(egui::PointerButton::Primary)
                            {
                                switch = Some(*id);
                            }
                        }
                    });
                    if let Some(tid) = switch {
                        // Switch the debugger's focus; the new thread's stack arrives via Stack.
                        self.dbg_frame = None;
                        self.dbg_scopes.clear();
                        self.dbg_vars.clear();
                        self.dbg_vars_pending.clear();
                        self.dap.set_active_thread(tid);
                    }
                    ui.add_space(4.0);
                }
                ui.colored_label(colors::TEXT_FAINT(), "Frames");
                let frames = self.dbg_stack.clone();
                egui::ScrollArea::vertical().id_salt("dbg-stack").auto_shrink([false, false]).show(ui, |ui| {
                    for fr in &frames {
                        let sel = self.dbg_frame == Some(fr.id);
                        let label = match (&fr.path, fr.line) {
                            (Some(p), l) => format!(
                                "{}  {}:{}",
                                fr.name,
                                p.file_name().and_then(|n| n.to_str()).unwrap_or(""),
                                l
                            ),
                            _ => fr.name.clone(),
                        };
                        if ui.selectable_label(sel, egui::RichText::new(label).size(12.0).monospace()).clicked_by(egui::PointerButton::Primary) {
                            self.dbg_frame = Some(fr.id);
                            self.dbg_scopes.clear();
                            self.dbg_vars.clear();
                    self.dbg_vars_pending.clear();
                            self.dap.request_scopes(fr.id);
                            self.refresh_watches();
                            if let Some(path) = fr.path.clone() {
                                let line = fr.line.saturating_sub(1) as usize;
                                self.open_file(path.clone());
                                if let Some(f) = self.find_file_mut(&path) {
                                    f.view.debug_line = Some(line);
                                    let byte = f.buffer.rope().line_to_byte(line.min(f.buffer.rope().len_lines().saturating_sub(1)));
                                    let rope = f.buffer.rope().clone();
                                    f.view.jump_to(byte, &rope);
                                }
                            }
                        }
                    }
                });
            });
            ui.separator();
            // --- variables ---
            ui.allocate_ui(egui::vec2(col_w, avail_h), |ui| {
                // --- watches: expressions re-evaluated on every stop --------------------------
                ui.colored_label(colors::TEXT_FAINT(), "Watches");
                ui.horizontal(|ui| {
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.watch_input)
                            .hint_text("add watch expression…")
                            .desired_width(col_w - 40.0)
                            .font(egui::TextStyle::Monospace),
                    );
                    let submit = resp.lost_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter))
                        && !self.watch_input.trim().is_empty();
                    if submit {
                        let expr = std::mem::take(&mut self.watch_input).trim().to_string();
                        self.watches.push(expr.clone());
                        self.watch_vals.push(None);
                        if self.dap.is_stopped() {
                            // Only the NEW watch — re-running every existing one would re-fire
                            // their (possibly side-effecting) expressions.
                            let i = self.watches.len() - 1;
                            let (frame, epoch) = (self.dbg_frame, self.watch_epoch);
                            self.dap.evaluate_watch(&expr, frame, watch_tag(epoch, i));
                        }
                        resp.request_focus();
                    }
                });
                let mut remove: Option<usize> = None;
                for (i, expr) in self.watches.iter().enumerate() {
                    ui.horizontal(|ui| {
                        if ui.small_button("✕").clicked_by(egui::PointerButton::Primary) {
                            remove = Some(i);
                        }
                        let val = self
                            .watch_vals
                            .get(i)
                            .and_then(|v| v.as_deref())
                            .unwrap_or("…");
                        ui.colored_label(
                            colors::TEXT_MUTED(),
                            egui::RichText::new(format!("{expr} = {val}")).size(12.0).monospace(),
                        );
                    });
                }
                if let Some(i) = remove {
                    self.watches.remove(i);
                    self.watch_vals.remove(i);
                    // In-flight evals carry the old epoch — bump so they can't land shifted.
                    self.watch_epoch += 1;
                }
                style::hairline(ui);
                ui.colored_label(colors::TEXT_FAINT(), "Variables");
                let scopes = self.dbg_scopes.clone();
                egui::ScrollArea::vertical().id_salt("dbg-vars").auto_shrink([false, false]).show(ui, |ui| {
                    for sc in &scopes {
                        egui::CollapsingHeader::new(egui::RichText::new(&sc.name).size(12.0))
                            .default_open(sc.name.to_lowercase().contains("local"))
                            .show(ui, |ui| {
                                self.var_rows(ui, sc.variables_reference, 0);
                            });
                    }
                });
            });
            ui.separator();
            // --- console + evaluate ---
            ui.allocate_ui(egui::vec2(ui.available_width(), avail_h), |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(colors::TEXT_FAINT(), "Console");
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.dbg_eval)
                            .hint_text("evaluate expression…")
                            .desired_width(f32::INFINITY),
                    );
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && !self.dbg_eval.is_empty() {
                        let expr = std::mem::take(&mut self.dbg_eval);
                        self.dbg_console.push(format!("> {expr}"));
                        self.dap.evaluate(&expr, self.dbg_frame);
                        resp.request_focus();
                    }
                });
                egui::ScrollArea::vertical().id_salt("dbg-con").stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
                    for l in &self.dbg_console {
                        ui.label(egui::RichText::new(l).size(12.0).monospace());
                    }
                });
            });
        });
    }

    /// Recursive variable rows; expanding a composite lazily requests its children.
    fn var_rows(&mut self, ui: &mut egui::Ui, reference: i64, depth: usize) {
        if depth > 6 {
            return;
        }
        let Some(vars) = self.dbg_vars.get(&reference).cloned() else {
            ui.colored_label(colors::TEXT_FAINT(), "…");
            return;
        };
        for v in &vars {
            let text = egui::RichText::new(format!("{} = {}", v.name, v.value)).size(12.0).monospace();
            if v.variables_reference > 0 {
                let resp = egui::CollapsingHeader::new(text)
                    .id_salt((reference, &v.name))
                    .show(ui, |ui| {
                        self.var_rows(ui, v.variables_reference, depth + 1);
                    });
                if resp.fully_open()
                    && !self.dbg_vars.contains_key(&v.variables_reference)
                    && self.dbg_vars_pending.insert(v.variables_reference)
                {
                    self.dap.request_variables(v.variables_reference);
                }
            } else {
                ui.label(text);
            }
        }
    }

    fn title(&self) -> String {
        match self.groups.get(self.focused).and_then(|g| g.files.get(g.active)) {
            Some(f) => format!(
                "Cauldron — {} — {}{}",
                self.workspace.name,
                f.name(),
                if f.dirty { " •" } else { "" }
            ),
            None => format!("Cauldron — {}", self.workspace.name),
        }
    }

    // ---- session persistence -------------------------------------------------------------------

    fn capture_session(&mut self) -> state::Session {
        let mut carets = Vec::new();
        for g in &self.groups {
            for f in &g.files {
                // A lazy tab's view is a stub (caret 0): persist its PARKED session caret so
                // a tab never activated this run still reopens where it was left.
                let caret = if f.loaded {
                    f.view.caret_byte()
                } else {
                    self.lazy_carets.get(&f.path).copied().unwrap_or(0)
                };
                carets.push((f.path.clone(), caret));
            }
        }
        state::Session {
            groups: self.groups.iter().map(|g| g.files.iter().map(|f| f.path.clone()).collect()).collect(),
            actives: self.groups.iter().map(|g| g.active).collect(),
            focused: self.focused,
            carets,
            pins: self.pins.clone(),
            project_open: self.project_open,
            pins_open: self.pins_open,
            terminal_open: self.terminal.open,
            bottom_open: self.bottom_open,
            bottom_tab: self.bottom_tab,
            right_tab: self.right_tab,
            breakpoints: {
                let mut v: Vec<_> =
                    self.breakpoints.iter().map(|(p, b)| (p.clone(), b.clone())).collect();
                v.sort(); // stable JSON across saves
                v
            },
            bookmarks: {
                let mut v: Vec<_> =
                    self.bookmarks.iter().map(|(p, b)| (p.clone(), b.clone())).collect();
                v.sort();
                v
            },
        }
    }

    /// Persist the GLOBAL prefs (font, standards). Unlike the session this is not keyed to a
    /// project, so it is saved even in no-project mode — the font you picked at the welcome
    /// screen is still the font you picked.
    fn save_settings(&self) {
        settings::save(&settings::Settings {
            editor_font: self.editor_font,
            standards: self.standards,
            auto_deps: self.auto_deps,
            inlay_hints: self.inlay_hints_on,
            inline_blame: self.inline_blame_enabled,
            theme: self.theme_choice,
            ai: self.ai_settings.clone(),
        });
    }

    fn save_session(&mut self) {
        if self.no_project {
            return; // nothing open — never key a session (or the pointer) to the sentinel
        }
        let root = self.workspace.root.clone();
        let sess = self.capture_session();
        state::save(&root, &sess);
        // The autosave/exit path is what makes "boot into the last project" survive a
        // SIGTERM/compositor kill, exactly like the session content itself.
        state::save_last_project(&root);
    }

    /// Reopen exactly where the project was left: files per split, order, actives, carets,
    /// pins and panel states. Terminal reopen is deferred one frame (needs a ctx).
    ///
    /// Boot-wave item 4: restore cost is no longer linear in tab count. Only each group's
    /// ACTIVE tab is loaded (read + parse + LSP didOpen — which also keys the first-per-
    /// language server spawn) eagerly; every other tab is a chrome-only [`OpenFile::lazy`]
    /// placeholder hydrated on first activation. Gutter marks for whatever WAS loaded come
    /// from ONE batched `git diff` at the tail instead of a serial subprocess per tab.
    fn restore_session(&mut self) {
        let root = self.workspace.root.clone();
        let Some(sess) = state::load(&root) else { return };
        self.restoring_session = true;
        for (gi, files) in sess.groups.iter().enumerate() {
            if gi > 0 && self.groups.len() <= gi && self.groups.len() < MAX_GROUPS {
                self.groups.push(EditorGroup { files: Vec::new(), active: 0 });
            }
            self.focused = gi.min(self.groups.len() - 1);
            let active = sess.actives.get(gi).copied().unwrap_or(0).min(files.len().saturating_sub(1));
            for (i, path) in files.iter().enumerate() {
                if i == active {
                    self.open_file(path.clone());
                } else if !self
                    .groups
                    .iter()
                    .any(|g| g.files.iter().any(|f| f.path == *path))
                {
                    let gi = self.focused;
                    self.groups[gi].files.push(OpenFile::lazy(path.clone()));
                }
            }
        }
        for (gi, active) in sess.actives.iter().enumerate() {
            if let Some(g) = self.groups.get_mut(gi) {
                g.active = (*active).min(g.files.len().saturating_sub(1));
            }
        }
        self.focused = sess.focused.min(self.groups.len() - 1);
        for (path, caret) in &sess.carets {
            let loaded = self
                .groups
                .iter()
                .flat_map(|g| g.files.iter())
                .find(|f| f.path == *path)
                .map(|f| f.loaded);
            match loaded {
                Some(true) => {
                    if let Some(f) = self.find_file_mut(path) {
                        let rope = f.buffer.rope().clone();
                        f.view.jump_to((*caret).min(rope.len_bytes()), &rope);
                    }
                }
                // Parked: applied by ensure_active_loaded on first activation, and persisted
                // as-is by capture_session until then.
                Some(false) => {
                    self.lazy_carets.insert(path.clone(), *caret);
                }
                None => {}
            }
        }
        // Breakpoints: adopt the saved set, light the dots on whatever is loaded, and hand
        // everything to the DAP manager so the next launch replays them.
        for (path, bps) in &sess.breakpoints {
            let mut bps = bps.clone();
            bps.sort_by_key(|(l, _)| *l);
            bps.dedup_by_key(|(l, _)| *l);
            if !bps.is_empty() {
                self.breakpoints.insert(path.clone(), bps);
            }
        }
        let paths: Vec<PathBuf> = self.breakpoints.keys().cloned().collect();
        for p in paths {
            self.sync_breakpoints(&p);
        }
        for (path, marks) in &sess.bookmarks {
            let mut marks = marks.clone();
            marks.sort_unstable();
            marks.dedup();
            if !marks.is_empty() {
                self.bookmarks.insert(path.clone(), marks);
            }
        }
        let paths: Vec<PathBuf> = self.bookmarks.keys().cloned().collect();
        for p in paths {
            self.sync_bookmarks(&p);
        }
        self.restoring_session = false;
        // ONE batched `git diff` for every tab the restore actually loaded (the actives).
        let loaded: Vec<PathBuf> = self
            .groups
            .iter()
            .flat_map(|g| g.files.iter())
            .filter(|f| f.loaded)
            .map(|f| f.path.clone())
            .collect();
        self.refresh_gutter_batch(&loaded);
        self.pins = sess.pins;
        self.project_open = sess.project_open;
        self.pins_open = sess.pins_open;
        self.bottom_open = sess.bottom_open;
        self.bottom_tab = sess.bottom_tab;
        self.right_tab = sess.right_tab;
        self.restore_terminal_pending = sess.terminal_open;
        // …and the restore closes it too. `restore_terminal_pending` can only ever OPEN the pane
        // (it is consumed by a `toggle` guarded on `!open`), which is all boot ever needs — but a
        // project SWITCH inherits the previous project's open terminal, so a project whose session
        // says "no terminal" has to say it explicitly.
        if !sess.terminal_open {
            self.terminal.open = false;
        }
    }

    // ---- NASA layer ----------------------------------------------------------------------------

    /// clang-format drift check — mirrors cFE CI's format-check job. One warning squiggle at
    /// the first drifting spot; empty layer when clean or when no .clang-format governs the file.
    fn check_format(&mut self, path: &Path) {
        let has_style = path.ancestors().skip(1).any(|d| d.join(".clang-format").exists());
        if !has_style {
            return;
        }
        // cFE CI format-checks the PR DIFF — upstream files you haven't touched must not yell.
        boot_trace::count("git-subprocess", 1);
        let unchanged = std::process::Command::new("git")
            .arg("-C")
            .arg(&self.workspace.root)
            .args(["diff", "--quiet", "HEAD", "--"])
            .arg(path)
            .status()
            .map(|st| st.success())
            .unwrap_or(false);
        if unchanged {
            self.diags.replace(path, 3, Vec::new());
            let merged = self.diags.merged(path);
            if let Some(f) = self.find_file_mut(path) {
                f.view.set_diagnostics(merged);
            }
            return;
        }
        let Some(f) = self.find_file_mut(path) else { return };
        let current = f.buffer.rope().to_string();
        let formatted = run_formatter(
            "clang-format",
            &[&format!("--assume-filename={}", path.display())],
            path,
            &current,
        );
        let diags = match formatted {
            Some(want) if want != current => {
                let at = current
                    .bytes()
                    .zip(want.bytes())
                    .position(|(a, b)| a != b)
                    .unwrap_or(0)
                    .min(current.len().saturating_sub(1));
                let line_end = current[at..].find('\n').map(|i| at + i).unwrap_or(current.len());
                vec![ViewDiag {
                    range: at..line_end.max(at + 1),
                    severity: 4,
                    message: "[format] clang-format drift — cFE CI format-check will reject \
                              this; Code ▸ Reformat File (Ctrl+Alt+L)"
                        .into(),
                }]
            }
            _ => Vec::new(),
        };
        self.diags.replace(path, 3, diags);
        let merged = self.diags.merged(path);
        if let Some(f) = self.find_file_mut(path) {
            f.view.set_diagnostics(merged);
        }
    }

    /// Reformat the active file with the language's canonical tool (clang-format / rustfmt),
    /// applied as ONE undoable transaction through the normal edit path.
    /// Run an editor command (comment/move-line/join/…) on the focused, loaded file. The edits it
    /// produces are drained by the normal `take_edits` path next frame — dirty flag, LSP didChange
    /// and PSI overlay all fire exactly as they do for typing, so nothing else is needed here.
    fn editor_command(&mut self, ctx: &egui::Context, run: impl FnOnce(&mut EditorView, &mut Buffer, f64)) {
        let now = ctx.input(|i| i.time);
        let g = &mut self.groups[self.focused];
        let active = g.active;
        if let Some(f) = g.files.get_mut(active) {
            if f.loaded {
                run(&mut f.view, &mut f.buffer, now);
            }
        }
    }

    /// Where the caret is right now — the origin recorded before a jump, and what Back returns to.
    fn current_location(&self) -> Option<nav::NavPoint> {
        let g = self.groups.get(self.focused)?;
        let f = g.files.get(g.active)?;
        Some(nav::NavPoint::new(f.path.clone(), f.view.caret_byte()))
    }

    /// Open `path` (if not already open) and move the caret to `byte`. The mechanical half of
    /// navigation, shared by [`Self::navigate_to`] and Back/Forward.
    /// The Shift+F11 bookmarks list: every bookmark across the project, Enter/click jumps,
    /// ✕ removes, Esc closes.
    fn bookmarks_overlay_ui(&mut self, ctx: &egui::Context) {
        // The editor must drop Enter/arrows/Esc while the list is open — otherwise Enter-to-jump
        // ALSO inserts a newline into the (still egui-focused) buffer behind the overlay.
        // (Single writer for the flag, so every TextEdit-less modal ORs in here.)
        let stolen = self.bookmarks_open || self.exit_confirm;
        if let Some(f) = self.groups[self.focused].active_file() {
            f.view.modal_keys_stolen = stolen;
        }
        if !self.bookmarks_open {
            return;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.bookmarks_open = false;
            return;
        }
        let rows = self.bookmark_rows.clone();
        let root = self.workspace.root.clone();
        let mut jump: Option<(PathBuf, u32)> = None;
        let mut remove: Option<(PathBuf, u32)> = None;
        let shown = rows.len();
        if shown > 0 {
            if ctx.input(|i| i.key_pressed(egui::Key::ArrowDown)) {
                self.bookmarks_sel = (self.bookmarks_sel + 1) % shown;
            }
            if ctx.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
                self.bookmarks_sel = (self.bookmarks_sel + shown - 1) % shown;
            }
            self.bookmarks_sel = self.bookmarks_sel.min(shown - 1);
            if ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
                let (p, l, _) = rows[self.bookmarks_sel].clone();
                jump = Some((p, l));
            }
        }
        egui::Area::new("bookmarks-overlay".into())
            .anchor(egui::Align2::CENTER_TOP, [0.0, 80.0])
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .inner_margin(egui::Margin::same(style::sizes::OVERLAY_PAD))
                    .show(ui, |ui| {
                        ui.set_width(560.0);
                        style::panel_header_inline(ui, "Bookmarks");
                        if rows.is_empty() {
                            ui.colored_label(colors::TEXT_FAINT(), "no bookmarks — F11 toggles one");
                            return;
                        }
                        egui::ScrollArea::vertical().max_height(400.0).show(ui, |ui| {
                            for (i, (path, line, preview)) in rows.iter().enumerate() {
                                let rel = path.strip_prefix(&root).unwrap_or(path);
                                let selected = i == self.bookmarks_sel;
                                ui.horizontal(|ui| {
                                    if ui.small_button("✕").clicked_by(egui::PointerButton::Primary) {
                                        remove = Some((path.clone(), *line));
                                    }
                                    let label =
                                        format!("{}:{}  {}", rel.display(), line, preview);
                                    let text = if selected {
                                        egui::RichText::new(label).color(colors::ACCENT_HI())
                                    } else {
                                        egui::RichText::new(label).color(colors::TEXT_MUTED())
                                    };
                                    if ui
                                        .selectable_label(selected, text)
                                        .clicked_by(egui::PointerButton::Primary)
                                    {
                                        jump = Some((path.clone(), *line));
                                    }
                                });
                            }
                        });
                    });
            });
        if let Some((path, line)) = remove {
            if let Some(ls) = self.bookmarks.get_mut(&path) {
                ls.retain(|l| *l != line);
                if ls.is_empty() {
                    self.bookmarks.remove(&path);
                }
            }
            self.sync_bookmarks(&path);
            self.rebuild_bookmark_rows();
        }
        if let Some((path, line)) = jump {
            self.bookmarks_open = false;
            self.open_file(path.clone());
            if let Some(f) = self.groups[self.focused].active_file() {
                let rope = f.buffer.rope().clone();
                let byte = rope.line_to_byte((line as usize - 1).min(rope.len_lines().saturating_sub(1)));
                f.view.jump_to(byte, &rope);
                f.view.grab_focus();
            }
        }
    }

    /// Ctrl+F12 file-structure popup: fuzzy filter over the active file's LSP outline.
    fn file_symbols_overlay_ui(&mut self, ctx: &egui::Context) {
        if !self.file_symbols_open {
            return;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.file_symbols_open = false;
            return;
        }
        let grab_focus = std::mem::take(&mut self.prompt_focus_pending);
        // The outline is a single global cache shared with the Structure panel — it may still
        // hold a DIFFERENT file's symbols (server slow, or the language has no server). Showing
        // (or worse, jumping from) foreign symbols against the current buffer is a lie; gate on
        // the cache being FOR the active file.
        let active_path = self.groups[self.focused].active_file().map(|f| f.path.clone());
        let outline_fresh = match (&self.outline_for, &active_path) {
            (Some((p, _)), Some(ap)) => p == ap,
            _ => false,
        };
        // Filter: rank substring hits first, then subsequence matches. (rank, index) tuples —
        // arithmetic key-packing corrupted ordering past 100k symbols.
        let q = self.file_symbols_query.to_lowercase();
        let mut rows: Vec<(u8, usize, String)> = Vec::new();
        if outline_fresh {
            for (i, (depth, glyph, name, detail, _)) in self.outline.iter().enumerate() {
                let hay = name.to_lowercase();
                let rank = if q.is_empty() || hay.contains(&q) {
                    0u8
                } else if is_subsequence(&q, &hay) {
                    1
                } else {
                    continue;
                };
                let display = format!(
                    "{}{} {}{}",
                    "  ".repeat(*depth),
                    glyph,
                    name,
                    if detail.is_empty() { String::new() } else { format!("  {detail}") },
                );
                rows.push((rank, i, display));
            }
        }
        rows.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
        let shown = rows.len();
        let mut chosen: Option<usize> = None;
        if shown > 0 {
            if ctx.input(|i| i.key_pressed(egui::Key::ArrowDown)) {
                self.file_symbols_sel = (self.file_symbols_sel + 1) % shown;
            }
            if ctx.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
                self.file_symbols_sel = (self.file_symbols_sel + shown - 1) % shown;
            }
            self.file_symbols_sel = self.file_symbols_sel.min(shown - 1);
            if ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
                chosen = Some(rows[self.file_symbols_sel].1);
            }
        }
        egui::Area::new("file-symbols".into())
            .anchor(egui::Align2::CENTER_TOP, [0.0, 80.0])
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .inner_margin(egui::Margin::same(style::sizes::OVERLAY_PAD))
                    .show(ui, |ui| {
                        ui.set_width(560.0);
                        style::panel_header_inline(ui, "Go to Symbol in File");
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut self.file_symbols_query)
                                .hint_text("symbol name…")
                                .desired_width(f32::INFINITY)
                                .font(egui::TextStyle::Monospace),
                        );
                        if grab_focus {
                            resp.request_focus();
                        }
                        if resp.changed() {
                            self.file_symbols_sel = 0;
                        }
                        if !outline_fresh || self.outline.is_empty() {
                            ui.colored_label(
                                colors::TEXT_FAINT(),
                                "no symbols for this file yet (language server still indexing?)",
                            );
                            return;
                        }
                        egui::ScrollArea::vertical().max_height(400.0).show(ui, |ui| {
                            for (row, (_, idx, display)) in rows.iter().enumerate() {
                                let selected = row == self.file_symbols_sel;
                                let text = if selected {
                                    egui::RichText::new(display).monospace().color(colors::ACCENT_HI())
                                } else {
                                    egui::RichText::new(display).monospace().color(colors::TEXT_MUTED())
                                };
                                if ui
                                    .selectable_label(selected, text)
                                    .clicked_by(egui::PointerButton::Primary)
                                {
                                    chosen = Some(*idx);
                                }
                            }
                        });
                    });
            });
        if let Some(idx) = chosen {
            self.file_symbols_open = false;
            let pos = self.outline.get(idx).map(|(_, _, _, _, p)| *p);
            if let Some(pos) = pos {
                if let Some(f) = self.groups[self.focused].active_file() {
                    // (self.lsp directly: lsp_encoding(&self) can't be called under the
                    // self.groups borrow; an active file is always didOpen'ed anyway.)
                    let enc = self.lsp.encoding_for(&f.path).unwrap_or(Encoding::Utf16);
                    let rope = f.buffer.rope().clone();
                    let byte = pos_to_byte(&rope, &pos, enc);
                    f.view.jump_to(byte, &rope);
                    f.view.grab_focus();
                }
            }
        }
    }

    fn open_and_jump(&mut self, path: PathBuf, byte: usize) {
        self.open_file(path);
        if let Some(f) = self.groups[self.focused].active_file() {
            let rope = f.buffer.rope().clone();
            f.view.jump_to(byte.min(rope.len_bytes()), &rope);
        }
    }

    /// Jump to `path`:`byte` AND record it in the back/forward history. Every deliberate navigation
    /// (go-to-definition, a search hit, goto-line, a symbol jump) goes through here so Alt+Left can
    /// retrace it — ordinary caret motion does not, which is the whole point of a jump list.
    fn navigate_to(&mut self, path: PathBuf, byte: usize) {
        let from = self.current_location();
        self.open_and_jump(path.clone(), byte);
        if let Some(from) = from {
            self.nav.record(from, nav::NavPoint::new(path, byte));
        }
    }

    /// Ctrl+E — the recent-files switcher. Reuses the quick-open overlay, seeded with the MRU list
    /// (existing files only, most-recent first) so its empty-query view IS the recent list and
    /// typing narrows it.
    fn open_recent_files(&mut self) {
        let recent: Vec<PathBuf> = self.recent.iter().filter(|p| p.is_file()).cloned().collect();
        if recent.is_empty() {
            self.lsp_message = Some("no recent files yet".into());
            return;
        }
        self.close_overlays();
        self.quickopen.open(&recent, &self.workspace.root);
    }

    /// Ctrl+Shift+E — open the recent-locations popup over the jump history, each row a
    /// file:line with a one-line snippet (read from the open buffer, else from disk). The
    /// snippets are computed once here, not per frame.
    fn open_recent_locations(&mut self) {
        let points = self.nav.recent();
        if points.is_empty() {
            self.lsp_message = Some("no recent locations yet".into());
            return;
        }
        let rows: Vec<(nav::NavPoint, String)> = points
            .into_iter()
            .take(30)
            .map(|np| {
                let snippet = self.line_snippet_at(&np.path, np.byte);
                (np, snippet)
            })
            .collect();
        self.close_overlays();
        self.recent_locations = Some((rows, 0));
    }

    /// The trimmed text of the line containing `byte` in `path`: from the open buffer when the
    /// file is loaded, otherwise a bounded disk read. Empty on any failure.
    fn line_snippet_at(&self, path: &Path, byte: usize) -> String {
        let from_open = self
            .groups
            .iter()
            .flat_map(|g| g.files.iter())
            .find(|f| f.path == path && f.loaded)
            .map(|f| {
                let rope = f.buffer.rope();
                let b = byte.min(rope.len_bytes());
                rope.line(rope.byte_to_line(b)).to_string()
            });
        let line = from_open.or_else(|| {
            let text = std::fs::read_to_string(path).ok()?;
            let b = byte.min(text.len());
            let start = text[..b].rfind('\n').map(|i| i + 1).unwrap_or(0);
            let end = text[b..].find('\n').map(|i| b + i).unwrap_or(text.len());
            Some(text[start..end].to_string())
        });
        line.unwrap_or_default().trim().chars().take(80).collect()
    }

    /// The recent-locations popup: ↑/↓ move, Enter/click jump, Esc closes.
    fn recent_locations_ui(&mut self, ctx: &egui::Context) {
        let Some((rows, selected)) = &mut self.recent_locations else { return };
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.recent_locations = None;
            return;
        }
        let n = rows.len();
        if ctx.input(|i| i.key_pressed(egui::Key::ArrowDown)) {
            *selected = (*selected + 1) % n;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
            *selected = (*selected + n - 1) % n;
        }
        let mut chosen: Option<usize> = None;
        if ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
            chosen = Some(*selected);
        }
        let root = self.workspace.root.clone();
        egui::Area::new("recent-locations".into())
            .anchor(egui::Align2::CENTER_TOP, [0.0, 80.0])
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .inner_margin(egui::Margin::same(style::sizes::OVERLAY_PAD))
                    .show(ui, |ui| {
                        ui.set_width(640.0);
                        ui.colored_label(colors::TEXT_FAINT(), "Recent locations — ↑↓, Enter jumps, Esc closes");
                        egui::ScrollArea::vertical().max_height(400.0).show(ui, |ui| {
                            for (i, (np, snippet)) in rows.iter().enumerate() {
                                let rel = np.path.strip_prefix(&root).unwrap_or(&np.path);
                                let name = rel.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                                let mut job = egui::text::LayoutJob::default();
                                let font = egui::TextStyle::Monospace.resolve(ui.style());
                                job.append(
                                    &format!("{name}  "),
                                    0.0,
                                    egui::TextFormat { font_id: font.clone(), color: colors::AMBER(), ..Default::default() },
                                );
                                job.append(
                                    snippet,
                                    0.0,
                                    egui::TextFormat { font_id: font, color: colors::TEXT_MUTED(), ..Default::default() },
                                );
                                if ui
                                    .selectable_label(i == *selected, job)
                                    .clicked_by(egui::PointerButton::Primary)
                                {
                                    chosen = Some(i);
                                }
                            }
                        });
                    });
            });
        if let Some(i) = chosen {
            if let Some((rows, _)) = self.recent_locations.take() {
                if let Some((np, _)) = rows.into_iter().nth(i) {
                    self.open_and_jump(np.path, np.byte);
                }
            }
        }
    }

    /// Alt+Left — return to the previous location in the jump list.
    /// Recompute inline debug values for the stopped frame and push them to that file's view.
    /// Heuristic (DAP gives no per-variable source position): for each top-level local/argument,
    /// annotate the last line at-or-above the stopped line — within a 40-line window — where the
    /// variable name appears as a whole word, with `name = value`. Cleared on resume/teardown.
    fn refresh_debug_values(&mut self) {
        // The active frame: the selected one, else the top of the stack.
        let frame = self
            .dbg_frame
            .and_then(|id| self.dbg_stack.iter().find(|f| f.id == id))
            .or_else(|| self.dbg_stack.first());
        let Some(frame) = frame else { return };
        let (Some(path), stop_line) = (frame.path.clone(), frame.line.saturating_sub(1) as usize)
        else {
            return;
        };
        // Collect top-level (name → value) from every scope's variables (Locals, Arguments).
        let mut vals: Vec<(String, String)> = Vec::new();
        for sc in &self.dbg_scopes {
            if let Some(vars) = self.dbg_vars.get(&sc.variables_reference) {
                for v in vars {
                    if !vals.iter().any(|(n, _)| n == &v.name) {
                        vals.push((v.name.clone(), v.value.clone()));
                    }
                }
            }
        }
        if vals.is_empty() {
            return;
        }
        // Clear stale annotations on every OTHER file (a frame switch may change the file).
        for g in &mut self.groups {
            for f in &mut g.files {
                if f.path != path {
                    f.view.set_debug_values(Vec::new());
                }
            }
        }
        let Some(f) = self.groups.iter_mut().flat_map(|g| g.files.iter_mut()).find(|f| f.path == path && f.loaded)
        else {
            return;
        };
        let rope = f.buffer.rope();
        let lo = stop_line.saturating_sub(40);
        let hi = stop_line.min(rope.len_lines().saturating_sub(1));
        // For each var, the LAST line in [lo, hi] where its name appears as a whole word.
        let mut per_line: std::collections::BTreeMap<usize, Vec<String>> = std::collections::BTreeMap::new();
        for (name, value) in &vals {
            let mut best: Option<usize> = None;
            for ln in lo..=hi {
                let text = rope.line(ln).to_string();
                if word_appears(&text, name) {
                    best = Some(ln);
                }
            }
            if let Some(ln) = best {
                per_line.entry(ln).or_default().push(format!("{name} = {}", truncate_val(value)));
            }
        }
        let annotations: Vec<(usize, String)> =
            per_line.into_iter().map(|(ln, parts)| (ln, parts.join("   "))).collect();
        f.view.set_debug_values(annotations);
    }

    /// Run a commit action from the History panel's right-click menu (synchronous git — the
    /// house style for repo mutations), then invalidate blame + git + history caches. A
    /// failure (conflict, dirty tree) surfaces its git message in the status line.
    fn run_history_action(&mut self, action: history::HistoryAction, root: &Path) {
        use history::HistoryAction as A;
        let args: Vec<&str> = match &action {
            A::CherryPick(sha) => vec!["cherry-pick", sha],
            A::Revert(sha) => vec!["revert", "--no-edit", sha],
            A::SoftReset(sha) => vec!["reset", "--soft", sha],
        };
        let out = std::process::Command::new("git").arg("-C").arg(root).args(&args).output();
        match out {
            Ok(o) if o.status.success() => {
                let verb = match &action {
                    A::CherryPick(_) => "cherry-picked",
                    A::Revert(_) => "reverted",
                    A::SoftReset(_) => "soft-reset to commit",
                };
                self.lsp_message = Some(format!("git: {verb}"));
            }
            Ok(o) => {
                let err = String::from_utf8_lossy(&o.stderr);
                let msg = err.lines().next().unwrap_or("git action failed").to_string();
                self.lsp_message = Some(format!("git: {msg}"));
            }
            Err(e) => self.lsp_message = Some(format!("git: {e}")),
        }
        // Repo moved under every cache. Blame + history invalidate now; the git panel
        // re-reads on its next show (its own staleness cadence).
        self.blame.clear();
        self.history.mark_stale();
    }

    fn nav_back(&mut self) {
        if let Some(pt) = self.nav.back() {
            self.open_and_jump(pt.path, pt.byte);
        }
    }

    /// Alt+Right — re-follow a jump you stepped back from.
    fn nav_forward(&mut self) {
        if let Some(pt) = self.nav.forward() {
            self.open_and_jump(pt.path, pt.byte);
        }
    }

    /// Perform a command chosen from the palette. The single place that knows how to run each
    /// action — the same code the menus and keybinds reach, routed here so the palette stays a
    /// pure chooser.
    fn run_command(&mut self, cmd: palette::Command, ctx: &egui::Context) {
        use palette::Command as C;
        match cmd {
            C::QuickOpenFile => self.open_quickopen_files(),
            C::RecentFiles => self.open_recent_files(),
            C::RecentLocations => self.open_recent_locations(),
            C::OpenFile => self.open_file_picker(),
            C::OpenProject => self.open_project_picker(),
            C::NewProject => self.open_new_project_picker(),
            C::SaveFile => self.save(),
            C::SearchInFiles => self.open_search(""),
            C::FormatFile => self.reformat_file(ctx),
            C::CommentLines => self.editor_command(ctx, |v, b, now| v.toggle_line_comment(b, now)),
            C::CommentBlock => self.editor_command(ctx, |v, b, now| v.toggle_block_comment(b, now)),
            C::MoveLineUp => self.editor_command(ctx, |v, b, now| v.move_lines(b, false, now)),
            C::MoveLineDown => self.editor_command(ctx, |v, b, now| v.move_lines(b, true, now)),
            C::JoinLines => self.editor_command(ctx, |v, b, now| v.join_lines(b, now)),
            C::JumpMatchingBracket => {
                self.editor_command(ctx, |v, b, _| v.jump_to_matching_bracket(b))
            }
            C::FoldRegion => self.editor_command(ctx, |v, b, _| v.toggle_fold_at_caret(b)),
            C::ToggleWrap => self.editor_command(ctx, |v, _, _| v.toggle_wrap()),
            C::GoToDefinition => {
                if let Some(f) = self.groups[self.focused].active_file() {
                    let (path, gen, byte) =
                        (f.path.clone(), f.buffer.generation, f.view.caret_byte());
                    let rope = f.buffer.rope().clone();
                    self.lsp.request_definition(&path, &rope, byte, gen);
                }
            }
            C::GoToImplementation => {
                if let Some(f) = self.groups[self.focused].active_file() {
                    let (path, gen, byte) =
                        (f.path.clone(), f.buffer.generation, f.view.caret_byte());
                    let rope = f.buffer.rope().clone();
                    self.lsp.request_implementation(&path, &rope, byte, gen);
                }
            }
            C::FindUsages => self.find_usages(),
            C::CallHierarchy => self.call_hierarchy(),
            C::RenameSymbol => self.start_rename(),
            C::QuickFix => {
                if let Some(f) = self.groups[self.focused].active_file() {
                    // The real selection, not caret..caret — extract/inline refactorings
                    // are only offered over a non-empty range.
                    let (p, r) = (f.path.clone(), f.view.selection_byte_range());
                    self.request_quick_fixes(p, r);
                }
            }
            C::GoToLine => self.open_goto_line(),
            C::ToggleBookmark => self.toggle_bookmark(),
            C::ShowBookmarks => self.open_bookmarks(),
            C::GoToFileSymbol => self.open_file_symbols(),
            C::RunCoverage => {
                if self.coverage.running {
                    self.lsp_message = Some("coverage run already in progress…".into());
                } else {
                    self.flush_dirty_buffers();
                    let root = self.workspace.root.clone();
                    self.coverage.run(&root, ctx);
                    self.lsp_message = Some("running tests with coverage…".into());
                }
            }
            C::ClearCoverage => {
                self.coverage.files.clear();
                for g in &mut self.groups {
                    for f in &mut g.files {
                        f.view.set_coverage_marks(Vec::new());
                    }
                }
            }
            C::ShowPullRequests => {
                self.bottom_open = true;
                self.bottom_tab = BottomTab::Prs;
            }
            C::ShowHistory => {
                self.bottom_open = true;
                self.bottom_tab = BottomTab::History;
            }
            C::ToggleBlame => {
                self.inline_blame_enabled = !self.inline_blame_enabled;
                self.save_settings();
                if !self.inline_blame_enabled {
                    if let Some(f) = self.groups[self.focused].active_file() {
                        f.view.set_inline_blame(None);
                    }
                }
            }
            C::ShowDiff => {
                if let Some(f) = self.groups[self.focused].active_file() {
                    let path = f.path.clone();
                    self.open_diff(&path);
                }
            }
            C::NavBack => self.nav_back(),
            C::NavForward => self.nav_forward(),
            C::Run => self.run_project(ctx, true),
            C::RunCurrentFile => self.run_current_file(ctx),
            C::Build => self.run_project(ctx, false),
            C::StopRun => {
                self.runner.stop();
                self.terminal.stop_run_tab();
            }
            C::InstallDependencies => {
                self.install_deps(true);
                self.bottom_open = true;
            }
            C::ToggleTerminal => {
                let root = self.terminal_root();
                self.terminal.toggle(ctx, &root);
            }
            C::ToggleProjectPanel => self.project_open = !self.project_open,
            C::MarkdownPreview => {
                if self.pins_open && self.right_tab == RightTab::Preview {
                    self.pins_open = false;
                } else {
                    self.right_tab = RightTab::Preview;
                    self.pins_open = true;
                }
            }
            C::WebPreview => self.web_preview_active_file(),
            C::RunWebDevServer => self.run_web_dev_server(ctx),
            C::ResolveConflicts => self.open_conflict_resolver(),
            C::SplitRight => self.split_move_right(),
            C::Settings => self.settings_open = true,
        }
    }

    fn reformat_file(&mut self, ctx: &egui::Context) {
        let Some(f) = self.groups[self.focused].active_file() else { return };
        if !f.loaded {
            return; // lazy stub — no content to reformat
        }
        let path = f.path.clone();
        let lang = f.lang;
        let gen = f.buffer.generation;
        let current = f.buffer.rope().to_string();
        let formatted = match f.lang {
            Some(Lang::C) | Some(Lang::Cpp) => run_formatter(
                "clang-format",
                &[&format!("--assume-filename={}", path.display())],
                &path,
                &current,
            ),
            Some(Lang::Rust) => run_formatter("rustfmt", &["--emit", "stdout", "--edition", "2021"], &path, &current),
            _ => None,
        };
        let Some(want) = formatted else {
            // No bundled CLI formatter for this language — ask the language server. It applies
            // asynchronously via LspEvent::Formatting. Editor indents with 4-space soft tabs.
            if self.lsp.has_live_server(&path) {
                self.lsp.request_formatting(&path, gen, 4, true);
                self.lsp_message = Some("formatting…".into());
            } else {
                self.lsp_message =
                    Some("no formatter for this file type (needs a language server)".into());
            }
            return;
        };
        if want == current {
            self.lsp_message = Some("already formatted".into());
            return;
        }
        let now = ctx.input(|i| i.time);
        let mut sync: Option<(ropey::Rope, ropey::Rope, cauldron_editor::Transaction)> = None;
        if let Some(f) = self.find_file_mut(&path) {
            let pre = f.buffer.rope().clone();
            let tx = cauldron_editor::Transaction::replace(0, pre.len_bytes(), want);
            f.view.apply_external(&mut f.buffer, &tx, now);
            let post = f.buffer.rope().clone();
            f.dirty = true;
            sync = Some((pre, post, tx));
        }
        if let Some((pre, post, tx)) = sync {
            self.lsp.did_change(&path, &pre, &post, &tx);
            // Reformat dirties the buffer without a keystroke: arm the PSI overlay debounce
            // too, so NASA witness offsets track the reflowed text before any save.
            if self.standards != Standards::Off
                && matches!(lang, Some(Lang::C) | Some(Lang::Cpp))
            {
                self.psi_overlay_pending.insert(path.clone(), std::time::Instant::now());
            }
        }
        self.diags.replace(&path, 3, Vec::new());
        let merged = self.diags.merged(&path);
        if let Some(f) = self.find_file_mut(&path) {
            f.view.set_diagnostics(merged);
        }
    }

    /// Push severity-5 (NASA orange) squiggles onto every open file that hosts a witness call
    /// site of a Rule-1 cycle, with the exact violation in the message.
    fn refresh_nasa_squiggles(&mut self) {
        let PsiState::Ready { findings, .. } = &self.psi.state else { return };
        let mut per_file: HashMap<PathBuf, Vec<ViewDiag>> = HashMap::new();
        for finding in findings {
            if finding.macro_textual {
                continue; // config artifacts stay panel-only
            }
            let n = finding.members.len();
            let guard = finding.hops.iter().find_map(|h| h.guard.clone());
            // JPL PoT: hard Rule-1 error with verbatim rule text. GSFC 582 tier: the same
            // finding as convention ADVICE — Goddard's standard doesn't hard-forbid recursion,
            // but cFS reviewers still flag it. A recognized re-entry guard is cited either way.
            let base = match self.standards {
                Standards::JplPot => 5u8,
                _ => 2u8, // GSFC tier: unguarded cycles read as warnings
            };
            // Orchid (guarded) is a GSFC-tier courtesy; JPL PoT is strict. Tooling recursion
            // (code generators, host scripts) reads muted salmon in BOTH tiers — it can't
            // crash the vehicle, only the build box.
            let severity = if guard.is_some() && self.standards != Standards::JplPot {
                6
            } else if finding.tooling {
                7
            } else {
                base
            };
            let (_, msg_of): (u8, Box<dyn Fn(&str, &str) -> String>) = match self.standards {
                Standards::JplPot => (
                    5,
                    Box::new(move |func: &str, next: &str| {
                        format!(
                            "[pot-1] this call ({func} → {next}) closes a {n}-member recursion cycle. {RULE1_TEXT}"
                        )
                    }),
                ),
                _ => (
                    4,
                    Box::new(move |func: &str, next: &str| {
                        format!(
                            "[cfs-conv] this call ({func} → {next}) closes a {n}-member recursion                              cycle — GSFC 582 doesn't hard-forbid recursion, but flight-software                              convention (and JPL PoT Rule 1) does; expect reviewer pushback."
                        )
                    }),
                ),
            };
            for (hi, hop) in finding.hops.iter().enumerate() {
                let next = &finding.hops[(hi + 1) % finding.hops.len()].func;
                let len = next.chars().count().max(4);
                per_file.entry(hop.file.clone()).or_default().push(ViewDiag {
                    range: hop.offset..hop.offset + len,
                    severity,
                    message: msg_of(&hop.func, next),
                });
            }
        }
        let open_paths: Vec<PathBuf> =
            self.groups.iter().flat_map(|g| g.files.iter().map(|f| f.path.clone())).collect();
        for path in open_paths {
            let diags = per_file.remove(&path).unwrap_or_default();
            self.diags.replace(&path, 2, diags);
            let merged = self.diags.merged(&path);
            if let Some(f) = self.find_file_mut(&path) {
                f.view.set_diagnostics(merged);
            }
        }
    }

    // ---- LSP events ------------------------------------------------------------------------------

    fn handle_lsp_event(&mut self, ev: LspEvent) -> Option<PathBuf> {
        match ev {
            LspEvent::Diagnostics { path, version, diags } => {
                if let (Some(v), Some(cur)) = (version, self.lsp.doc_version(&path)) {
                    if v < cur {
                        return None;
                    }
                }
                self.store_diags(&path, 0, diags)
            }
            LspEvent::PullDiagnostics { path, version, diags } => {
                if let Some(cur) = self.lsp.doc_version(&path) {
                    if version < cur {
                        return None;
                    }
                }
                self.store_diags(&path, 1, diags)
            }
            LspEvent::Degraded { reason } | LspEvent::Message(reason) => {
                self.lsp_message = Some(reason);
                None
            }
            LspEvent::Exited => {
                // The manager owns respawn AND doc re-open: on a crash it carries the dead
                // server's doc set to the replacement and synthesizes didOpen from its own rope
                // store on the new handshake. The app must NOT re-open here — doing so
                // re-sent didOpen to HEALTHY servers (protocol violation) and, worse,
                // open_doc respawns a server the manager deliberately gave up on past
                // MAX_RESTARTS, defeating the crash cap.
                None
            }
            LspEvent::CodeActions { generation, path, actions } => {
                if self.fix_request_gen == Some(generation) {
                    self.fix_request_gen = None;
                    self.lsp_message = None;
                    let refactor = self.fix_request_refactor;
                    self.fix_menu_title = if refactor { "Refactor This" } else { "Quick Fixes" };
                    if actions.is_empty() {
                        self.lsp_message = Some(if refactor {
                            "no refactorings available here".into()
                        } else {
                            "no quick fixes available here".into()
                        });
                    } else if let Some(pos) = ctx_pointer_pos() {
                        self.fix_menu = Some((pos, path, sort_actions_by_kind(actions)));
                    }
                }
                None
            }
            LspEvent::ResolvedCodeAction { generation, path, action } => {
                if self.action_resolve_pending.as_ref().is_some_and(|(p, g, _)| *p == path && *g == generation)
                {
                    let title = self
                        .action_resolve_pending
                        .take()
                        .map_or_else(String::new, |(_, _, t)| t);
                    self.lsp_message = None;
                    match action {
                        // Resolve is allowed to come back still empty (the server changed its
                        // mind, or the range stopped qualifying) — that is a failure to report,
                        // not a success to stay quiet about.
                        Some(a) if a.edit.is_some() || a.command.is_some() => {
                            // 0.0 matches the ApplyEdit arm: external edits never coalesce
                            // into a typing group, so the timestamp is immaterial.
                            if let Some(edit) = &a.edit {
                                self.apply_workspace_edit(edit, 0.0);
                            }
                            if let Some(cmd) = &a.command {
                                self.lsp.execute_command(&path, cmd);
                            }
                        }
                        _ => self.lsp_message = Some(format!("‘{title}’ could not be applied")),
                    }
                }
                None
            }
            LspEvent::ApplyEdit(params) => {
                // Server-initiated edits (executeCommand results) — same undo-safe path.
                let edit = params.edit.clone();
                self.apply_workspace_edit(&edit, 0.0);
                None
            }
            LspEvent::SignatureHelp { generation, help } => {
                if self.sig_gen == Some(generation) {
                    let g = &mut self.groups[self.focused];
                    if let Some(f) = g.files.get_mut(g.active) {
                        if f.buffer.generation == generation {
                            match (help, f.view.caret_screen_pos()) {
                                (Some(h), Some(pos)) if !h.signatures.is_empty() => {
                                    self.sig_help = Some((pos, h));
                                }
                                _ => self.sig_help = None,
                            }
                        }
                    }
                }
                None
            }
            LspEvent::InlayHints { generation, path, hints } => {
                if self.inlay_requested == Some((path.clone(), generation)) {
                    self.inlay_requested = None;
                    self.inlay_for = Some((path.clone(), generation));
                    let enc = self.lsp_encoding(&path);
                    if let Some(f) = self
                        .groups
                        .iter_mut()
                        .flat_map(|g| g.files.iter_mut())
                        .find(|f| f.path == path && f.loaded)
                    {
                        if f.buffer.generation == generation {
                            let rope = f.buffer.rope().clone();
                            // (line, label) pairs, merged per line for the end-of-line paint.
                            let mut raw: Vec<(usize, String)> = hints
                                .iter()
                                .map(|h| {
                                    let b = pos_to_byte(&rope, &h.position, enc);
                                    let line = rope.byte_to_line(b.min(rope.len_bytes()));
                                    let label = match &h.label {
                                        lsp_types::InlayHintLabel::String(s) => s.clone(),
                                        lsp_types::InlayHintLabel::LabelParts(ps) => {
                                            ps.iter().map(|p| p.value.as_str()).collect()
                                        }
                                    };
                                    (line, label.trim().to_string())
                                })
                                .filter(|(_, l)| !l.is_empty())
                                .collect();
                            raw.sort_by(|a, b| a.0.cmp(&b.0));
                            let mut merged: Vec<(usize, String)> = Vec::new();
                            for (line, label) in raw {
                                match merged.last_mut() {
                                    Some((l, acc)) if *l == line => {
                                        acc.push_str("  ");
                                        acc.push_str(&label);
                                    }
                                    _ => merged.push((line, label)),
                                }
                            }
                            f.view.set_inlay_hints(merged);
                        }
                    }
                }
                None
            }
            LspEvent::DocumentSymbols { generation, path, symbols } => {
                if self.outline_requested == Some((path.clone(), generation)) {
                    self.outline.clear();
                    match symbols {
                        Some(lsp_types::DocumentSymbolResponse::Nested(list)) => {
                            fn walk(
                                out: &mut Vec<(usize, &'static str, String, String, lsp_types::Position)>,
                                items: &[lsp_types::DocumentSymbol],
                                depth: usize,
                            ) {
                                if depth > 64 {
                                    return; // server-controlled nesting must not overflow us
                                }
                                for s in items {
                                    out.push((
                                        depth,
                                        symbol_kind_glyph(s.kind),
                                        s.name.clone(),
                                        s.detail.clone().unwrap_or_default(),
                                        s.selection_range.start,
                                    ));
                                    if let Some(children) = &s.children {
                                        walk(out, children, depth + 1);
                                    }
                                }
                            }
                            walk(&mut self.outline, &list, 0);
                        }
                        Some(lsp_types::DocumentSymbolResponse::Flat(list)) => {
                            for s in list {
                                self.outline.push((
                                    0,
                                    symbol_kind_glyph(s.kind),
                                    s.name.clone(),
                                    String::new(),
                                    s.location.range.start,
                                ));
                            }
                        }
                        None => {}
                    }
                    self.outline_for = Some((path, generation));
                }
                None
            }
            LspEvent::WorkspaceSymbols { generation, symbols } => {
                // One event per answering server, all stamped with the fan-out generation:
                // matching answers ACCUMULATE into the LSP tier (SymbolIndex dedupes inside
                // it by (path, line)); a stale generation = the query moved on — drop.
                if generation == self.ws_symbols_gen && self.goto_symbol.is_open() {
                    self.symbols.extend_lsp(symbols::lsp_symbol_entries(&symbols));
                }
                None
            }
            LspEvent::ResolvedCompletion { generation, path, item } => {
                if self.resolve_pending == Some((path.clone(), generation)) {
                    self.resolve_pending = None;
                    // Only apply if the buffer hasn't moved on since the accept.
                    let current = self
                        .groups
                        .iter()
                        .flat_map(|g| g.files.iter())
                        .find(|f| f.path == path && f.loaded)
                        .map(|f| f.buffer.generation);
                    if current == Some(generation) {
                        if let Some(extra) = &item.additional_text_edits {
                            if !extra.is_empty() {
                                let edits = extra.clone();
                                self.apply_additional_edits(&path, &edits, 0.0);
                                self.lsp_message = Some("import added".into());
                            }
                        }
                    }
                }
                None
            }
            LspEvent::RenameEdit { generation, edit } => {
                let g = &self.groups[self.focused];
                let fresh = g.files.get(g.active).map(|f| f.buffer.generation) == Some(generation);
                if fresh {
                    match edit {
                        Some(e) => {
                            let files = txsync::workspace_edit_to_file_edits(&e).len();
                            self.apply_workspace_edit(&e, 0.0);
                            self.lsp_message =
                                Some(format!("renamed across {files} file(s) — Ctrl+Z per file"));
                        }
                        None => self.lsp_message = Some("server refused the rename".into()),
                    }
                }
                None
            }
            LspEvent::Formatting { generation, path, edits } => {
                // Stale-drop: a keystroke between request and reply bumps the generation, and the
                // edits' ranges no longer line up with the buffer — applying them would corrupt it.
                let fresh = self
                    .find_file_mut(&path)
                    .map(|f| f.buffer.generation == generation)
                    .unwrap_or(false);
                if fresh {
                    if edits.is_empty() {
                        self.lsp_message = Some("already formatted".into());
                    } else {
                        // Reuse the multi-file edit applier: wrap the single doc's edits in a
                        // WorkspaceEdit keyed by its uri (the applier sorts ranges itself).
                        let uri = cauldron_lsp::capabilities::file_uri(&path);
                        let mut changes = std::collections::HashMap::new();
                        changes.insert(uri, edits);
                        let we = lsp_types::WorkspaceEdit {
                            changes: Some(changes),
                            ..Default::default()
                        };
                        self.apply_workspace_edit(&we, 0.0);
                        self.lsp_message = Some("formatted".into());
                    }
                }
                None
            }
            LspEvent::IncomingCalls { generation, calls } => {
                if self.call_hierarchy_gen == Some(generation) {
                    self.call_hierarchy_gen = None;
                    self.lsp_message = None;
                    self.usages.clear();
                    self.usages_from_index = false;
                    self.usages_are_callers = true;
                    for (name, loc) in &calls {
                        let Some(path) = cauldron_lsp::capabilities::uri_to_path(&loc.uri) else {
                            continue;
                        };
                        let line = loc.range.start.line as usize;
                        // The caller's name IS the preview here — call hierarchy is about who,
                        // not the exact source line.
                        self.usages.push((path, line, name.clone()));
                    }
                    if !self.usages.is_empty() {
                        self.bottom_open = true;
                        self.bottom_tab = BottomTab::Usages;
                    } else {
                        self.lsp_message = Some("no callers found".into());
                    }
                }
                None
            }
            LspEvent::References { generation, locations } => {
                // The Change Signature dialog issues its own references request; that reply
                // belongs to the dialog, not the Usages panel.
                if self.chsig_refs_gen == Some(generation) && self.chsig.is_some() {
                    self.chsig_refs_gen = None;
                    self.chsig_take_references(&locations);
                    return None;
                }
                if self.usages_gen == Some(generation) {
                    self.usages_gen = None;
                    self.lsp_message = None;
                    self.usages.clear();
                    // A server answered: LSP is primary, any prior index-fallback label clears.
                    self.usages_from_index = false;
                    self.usages_are_callers = false;
                    for loc in &locations {
                        let Some(path) = cauldron_lsp::capabilities::uri_to_path(&loc.uri) else {
                            continue;
                        };
                        let line = loc.range.start.line as usize;
                        // Preview from the open buffer when available, else from disk.
                        let preview = self
                            .groups
                            .iter()
                            .flat_map(|g| g.files.iter())
                            .find(|f| f.path == path && f.loaded)
                            .map(|f| {
                                let rope = f.buffer.rope();
                                let l = line.min(rope.len_lines().saturating_sub(1));
                                rope.line(l).to_string()
                            })
                            .or_else(|| {
                                std::fs::read_to_string(&path).ok().and_then(|t| {
                                    t.lines().nth(line).map(|s| s.to_string())
                                })
                            })
                            .unwrap_or_default();
                        self.usages.push((
                            path,
                            line,
                            preview.trim().chars().take(140).collect(),
                        ));
                    }
                    if !self.usages.is_empty() {
                        self.bottom_open = true;
                        self.bottom_tab = BottomTab::Usages;
                    } else {
                        self.lsp_message = Some("no usages found".into());
                    }
                }
                None
            }
            LspEvent::Definition { generation, locations } => {
                let g = &self.groups[self.focused];
                let fresh = g.files.get(g.active).map(|f| f.buffer.generation) == Some(generation);
                if fresh {
                    let mut targets: Vec<(PathBuf, lsp_types::Position)> = locations
                        .iter()
                        .filter_map(|loc| {
                            cauldron_lsp::capabilities::uri_to_path(&loc.uri)
                                .map(|p| (p, loc.range.start))
                        })
                        .collect();
                    // Trait impls / overloads often resolve to several sites — silently taking
                    // the first was wrong more often than right. One target jumps directly;
                    // more open the chooser at the caret.
                    if targets.len() == 1 {
                        let (path, target) = targets.remove(0);
                        self.jump_to_lsp_location(path, target);
                    } else if targets.len() > 1 {
                        let pos = self
                            .groups
                            .get(self.focused)
                            .and_then(|g| g.files.get(g.active))
                            .and_then(|f| f.view.caret_screen_pos())
                            .unwrap_or(egui::Pos2::new(300.0, 300.0));
                        self.def_choices = Some((pos, targets, 0));
                    }
                }
                None
            }
            LspEvent::Completions { generation, items } => {
                if self.completion_gen == Some(generation) && !items.is_empty() {
                    let g = &mut self.groups[self.focused];
                    if let Some(f) = g.files.get_mut(g.active) {
                        if f.buffer.generation == generation {
                            let caret = f.view.caret_byte();
                            let rope = f.buffer.rope().clone();
                            let anchor = word_start(&rope, caret);
                            let pos = f
                                .view
                                .caret_screen_pos()
                                .unwrap_or(egui::Pos2::new(300.0, 300.0));
                            let path = f.path.clone();
                            self.completion = Some(CompletionUi {
                                path,
                                anchor,
                                items,
                                selected: 0,
                                pos,
                                navigated: false,
                            });
                        }
                    }
                }
                None
            }
            LspEvent::Hover { generation, contents } => {
                // Show only if the buffer hasn't changed since the request and the pointer is
                // still parked where we asked.
                let g = &self.groups[self.focused];
                if let (Some(f), Some(h)) = (g.files.get(g.active), contents) {
                    if f.buffer.generation == generation {
                        if let Some(text) = hover_text(&h) {
                            if let Some(pos) = ctx_pointer_pos() {
                                self.hover_popup = Some((pos, text));
                            }
                        }
                    }
                }
                None
            }
            _ => None,
        }
    }

    fn store_diags(
        &mut self,
        path: &Path,
        layer: usize,
        diags: Vec<lsp_types::Diagnostic>,
    ) -> Option<PathBuf> {
        let enc = self.lsp_encoding(path);
        // Lazy tabs: no live rope to convert against — skip; the server re-publishes after
        // the didOpen that hydration sends.
        let f =
            self.groups.iter().flat_map(|g| g.files.iter()).find(|f| f.path == path && f.loaded)?;
        let converted = to_view_diags(&diags, f.buffer.rope(), enc);
        self.diags.replace(path, layer, converted);
        Some(path.to_path_buf())
    }

    // ---- chrome -------------------------------------------------------------------------------------

    fn menu_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("menubar").exact_height(style::sizes::MENU_BAR_H).show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("New File…    Ctrl+N").clicked_by(egui::PointerButton::Primary) {
                        self.open_prompt(
                            NamePrompt::NewFile { dir_rel: PathBuf::new() },
                            String::new(),
                        );
                        ui.close_menu();
                    }
                    if ui.button("New Project…    Ctrl+Shift+N").clicked_by(egui::PointerButton::Primary) {
                        self.open_new_project_picker();
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("Open File…    Ctrl+O").clicked_by(egui::PointerButton::Primary) {
                        self.open_file_picker();
                        ui.close_menu();
                    }
                    if ui.button("Open Project…    Ctrl+Shift+O").clicked_by(egui::PointerButton::Primary) {
                        self.open_project_picker();
                        ui.close_menu();
                    }
                    if ui.button("Go to File…    Ctrl+P").clicked_by(egui::PointerButton::Primary) {
                        self.open_quickopen_files();
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("Save    Ctrl+S").clicked_by(egui::PointerButton::Primary) {
                        self.save();
                        ui.close_menu();
                    }
                    if ui.button("Close Tab    Ctrl+W").clicked_by(egui::PointerButton::Primary) {
                        let (g, a) = (self.focused, self.groups[self.focused].active);
                        self.request_close_tab(g, a);
                        ui.close_menu();
                    }
                });
                ui.menu_button("Run", |ui| {
                    if ui.button("▶ Run    Shift+F10").clicked_by(egui::PointerButton::Primary) {
                        self.run_project(ctx, true);
                        ui.close_menu();
                    }
                    let current = self
                        .groups
                        .get_mut(self.focused)
                        .and_then(|g| g.active_file())
                        .map(|f| f.name().to_string());
                    let label = match &current {
                        Some(name) => format!("▶ Run {name}    Ctrl+Shift+F10"),
                        None => "▶ Run current file    Ctrl+Shift+F10".to_string(),
                    };
                    if ui.add_enabled(current.is_some(), egui::Button::new(label)).clicked_by(egui::PointerButton::Primary) {
                        self.run_current_file(ctx);
                        ui.close_menu();
                    }
                    if ui.button("Build    Ctrl+F9").clicked_by(egui::PointerButton::Primary) {
                        self.run_project(ctx, false);
                        ui.close_menu();
                    }
                    if self.runner.running && ui.button("■ Stop").clicked_by(egui::PointerButton::Primary) {
                        self.runner.stop();
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui
                        .button("⟳ Install dependencies")
                        .on_hover_text(
                            "Resolve every ecosystem in this project (cargo, npm, NuGet, pip, …) now",
                        )
                        .clicked_by(egui::PointerButton::Primary)
                    {
                        // force = true: ignore the content stamp and re-resolve even if nothing
                        // changed — this is the manual escape hatch for a half-installed tree.
                        self.install_deps(true);
                        self.bottom_open = true;
                        ui.close_menu();
                    }
                });
                ui.menu_button("Code", |ui| {
                    if ui.button("Reformat File    Ctrl+Alt+L").clicked_by(egui::PointerButton::Primary) {
                        self.reformat_file(ctx);
                        ui.close_menu();
                    }
                    if ui.button("Comment Lines    Ctrl+/").clicked_by(egui::PointerButton::Primary) {
                        self.editor_command(ctx, |v, b, now| v.toggle_line_comment(b, now));
                        ui.close_menu();
                    }
                    if ui.button("Comment Block    Ctrl+Shift+/").clicked_by(egui::PointerButton::Primary) {
                        self.editor_command(ctx, |v, b, now| v.toggle_block_comment(b, now));
                        ui.close_menu();
                    }
                    if ui.button("Move Line Up    Alt+Shift+↑").clicked_by(egui::PointerButton::Primary) {
                        self.editor_command(ctx, |v, b, now| v.move_lines(b, false, now));
                        ui.close_menu();
                    }
                    if ui.button("Move Line Down    Alt+Shift+↓").clicked_by(egui::PointerButton::Primary) {
                        self.editor_command(ctx, |v, b, now| v.move_lines(b, true, now));
                        ui.close_menu();
                    }
                    if ui.button("Join Lines    Ctrl+Shift+J").clicked_by(egui::PointerButton::Primary) {
                        self.editor_command(ctx, |v, b, now| v.join_lines(b, now));
                        ui.close_menu();
                    }
                    if ui.button("Jump to Matching Bracket    Ctrl+Shift+\\").clicked_by(egui::PointerButton::Primary) {
                        self.editor_command(ctx, |v, b, _| v.jump_to_matching_bracket(b));
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("Rename Symbol…    Shift+F6").clicked_by(egui::PointerButton::Primary) {
                        self.start_rename();
                        ui.close_menu();
                    }
                    if ui.button("Find Usages    Alt+F7").clicked_by(egui::PointerButton::Primary) {
                        self.find_usages();
                        ui.close_menu();
                    }
                    if ui.button("Quick Fix…    Alt+Enter").clicked_by(egui::PointerButton::Primary) {
                        if let Some(f) = self.groups[self.focused].active_file() {
                            let (p, r) = (f.path.clone(), f.view.selection_byte_range());
                            self.request_quick_fixes(p, r);
                        }
                        ui.close_menu();
                    }
                    if ui
                        .button("Refactor This…    Ctrl+Alt+Shift+T")
                        .clicked_by(egui::PointerButton::Primary)
                    {
                        if let Some(f) = self.groups[self.focused].active_file() {
                            let path = f.path.clone();
                            let range = f.view.selection_byte_range();
                            let range = if range.is_empty() {
                                let b = f.view.caret_byte();
                                b..b
                            } else {
                                range
                            };
                            self.request_refactorings(path, range);
                        }
                        ui.close_menu();
                    }
                    if self.change_signature_available()
                        && ui
                            .button("Change Signature…    Ctrl+F6")
                            .clicked_by(egui::PointerButton::Primary)
                    {
                        self.start_change_signature();
                        ui.close_menu();
                    }
                    if ui.button("Go to Implementation    Ctrl+Alt+B").clicked_by(egui::PointerButton::Primary) {
                        if let Some(f) = self.groups[self.focused].active_file() {
                            let (path, gen, byte) =
                                (f.path.clone(), f.buffer.generation, f.view.caret_byte());
                            let rope = f.buffer.rope().clone();
                            self.lsp.request_implementation(&path, &rope, byte, gen);
                        }
                        ui.close_menu();
                    }
                    if ui.button("Go to Definition    Ctrl+B").clicked_by(egui::PointerButton::Primary) {
                        if let Some(f) = self.groups[self.focused].active_file() {
                            let (path, gen, byte) =
                                (f.path.clone(), f.buffer.generation, f.view.caret_byte());
                            let rope = f.buffer.rope().clone();
                            self.lsp.request_definition(&path, &rope, byte, gen);
                        }
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui
                        .add_enabled(self.nav.can_back(), egui::Button::new("Back    Alt+←"))
                        .clicked_by(egui::PointerButton::Primary)
                    {
                        self.nav_back();
                        ui.close_menu();
                    }
                    if ui
                        .add_enabled(self.nav.can_forward(), egui::Button::new("Forward    Alt+→"))
                        .clicked_by(egui::PointerButton::Primary)
                    {
                        self.nav_forward();
                        ui.close_menu();
                    }
                });
                ui.menu_button("View", |ui| {
                    if ui.button("Project panel    Alt+1").clicked_by(egui::PointerButton::Primary) {
                        self.project_open = !self.project_open;
                        ui.close_menu();
                    }
                    if ui.button("Pins bar").clicked_by(egui::PointerButton::Primary) {
                        self.pins_open = !self.pins_open;
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("Problems    Ctrl+J").clicked_by(egui::PointerButton::Primary) {
                        self.bottom_open = true;
                        self.bottom_tab = BottomTab::Problems;
                        ui.close_menu();
                    }
                    if ui.button("Output").clicked_by(egui::PointerButton::Primary) {
                        self.bottom_open = true;
                        self.bottom_tab = BottomTab::Output;
                        ui.close_menu();
                    }
                    if ui.button("Terminal    Alt+F12").clicked_by(egui::PointerButton::Primary) {
                        let root = self.terminal_root();
                        self.terminal.toggle(ctx, &root);
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("Split tab right    Ctrl+\\").clicked_by(egui::PointerButton::Primary) {
                        self.split_move_right();
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("Zoom in    Ctrl+=").clicked_by(egui::PointerButton::Primary) {
                        zoom(ctx, 0.1);
                        ui.close_menu();
                    }
                    if ui.button("Zoom out    Ctrl+-").clicked_by(egui::PointerButton::Primary) {
                        zoom(ctx, -0.1);
                        ui.close_menu();
                    }
                    if ui.button("Zoom reset    Ctrl+0").clicked_by(egui::PointerButton::Primary) {
                        ctx.set_zoom_factor(1.0);
                        ui.close_menu();
                    }
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    if self.runner.running {
                        if ui.button("■").on_hover_text("Stop").clicked_by(egui::PointerButton::Primary) {
                            self.runner.stop();
                        }
                        ui.spinner();
                    } else {
                        {
                        let root = self.workspace.root.clone();
                        if self.run_cfgs.selector_ui(ui, &root) {
                            self.run_cfgs.save(&root);
                        }
                    }
                    if icons::tool_icon_button(ui, icons::ToolIcon::Run, true, "Run (Shift+F10)")
                            .clicked_by(egui::PointerButton::Primary)
                        {
                            self.run_project(ctx, true);
                        }
                        if icons::tool_icon_button(ui, icons::ToolIcon::Build, true, "Build (Ctrl+F9)")
                            .clicked_by(egui::PointerButton::Primary)
                        {
                            self.run_project(ctx, false);
                        }
                    }
                    if icons::tool_icon_button(
                        ui,
                        icons::ToolIcon::Debug,
                        true,
                        "Debug (Shift+F9) — .py via debugpy, Rust/C binary via lldb-dap; \
                         click the gutter to set breakpoints first",
                    )
                    .clicked_by(egui::PointerButton::Primary)
                    {
                        self.start_debug(ctx);
                    }
                    if icons::tool_icon_button(ui, icons::ToolIcon::Settings, true, "Settings")
                        .clicked_by(egui::PointerButton::Primary)
                    {
                        self.settings_open = !self.settings_open;
                    }
                    if icons::tool_icon_button(
                        ui,
                        icons::ToolIcon::Search,
                        true,
                        "Find in Files (Ctrl+Shift+F)",
                    )
                    .clicked_by(egui::PointerButton::Primary)
                    {
                        self.open_search("");
                    }
                    ui.add_space(10.0);
                    // Git: repo name + branch, JetBrains top-bar style.
                    if let Some(branch) = self.workspace.git_branch() {
                        ui.colored_label(
                            colors::TEXT_FAINT(),
                            egui::RichText::new(format!("⎇ {} · {}", branch, self.workspace.name))
                                .size(12.0),
                        );
                    }
                });
            });
        });
    }

    /// One group's tab row + editor, inside its central column. Tabs render HERE — to the right
    /// of the Project tree, like a real IDE.
    fn group_ui(&mut self, ui: &mut egui::Ui, gi: usize) {
        // Crash guard: `ui.columns(n, …)` is sized for the group count captured before the loop,
        // but a tab close can collapse a group mid-loop (self.groups shrinks). A later column
        // then calls group_ui with a now-out-of-bounds gi — render nothing instead of panicking
        // (this was the "closing a split tab killed the whole app" crash). Group-shrinking
        // closes are also DEFERRED (see pending_tab_closes) so this is belt-and-braces.
        if gi >= self.groups.len() {
            return;
        }
        let is_focused_group = gi == self.focused;
        let strip_h = style::sizes::TAB_H;
        let strip_rect = egui::Rect::from_min_size(
            ui.cursor().min,
            egui::Vec2::new(ui.available_width(), strip_h),
        );
        ui.painter().rect_filled(strip_rect, 0.0, colors::BG_PANEL());

        let mut activate: Option<usize> = None;
        let mut close: Option<usize> = None;
        let mut move_split: Option<usize> = None;
        let mut pin: Option<usize> = None;
        let mut close_others: Option<usize> = None;
        let mut close_right: Option<usize> = None;
        // Drag-reorder: swap with a neighbor once the pointer crosses its midpoint.
        let mut swap: Option<(usize, usize)> = None;
        // A tab that just started being dragged (index + label) — becomes self.tab_drag after loop.
        let mut start_drag: Option<(usize, String)> = None;

        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(strip_rect), |ui| {
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                for (i, f) in self.groups[gi].files.iter().enumerate() {
                    let t = style::tab(
                        ui,
                        &f.name(),
                        i == self.groups[gi].active && is_focused_group,
                        f.dirty,
                    );
                    if t.clicked {
                        activate = Some(i);
                    }
                    if t.closed {
                        close = Some(i);
                    }
                    if t.response.drag_started() {
                        // Pick the tab up for a possible split/move (resolved on release in
                        // finish_tab_drag). Deferred like the other actions — `f` borrows groups.
                        start_drag = Some((i, f.name().to_string()));
                    }
                    if t.response.dragged() {
                        if let Some(pos) = t.response.interact_pointer_pos() {
                            let r = t.response.rect;
                            // Reorder ONLY while the pointer is still within this pane's strip
                            // (vertically over the tab row); once it leaves, the drag becomes a
                            // split/move and must not keep shuffling neighbors.
                            let in_strip = pos.y >= r.top() && pos.y <= r.bottom();
                            if in_strip && pos.x > r.right() && i + 1 < self.groups[gi].files.len() {
                                swap = Some((i, i + 1));
                            } else if in_strip && pos.x < r.left() && i > 0 {
                                swap = Some((i, i - 1));
                            }
                        }
                    }
                    t.response.context_menu(|ui| {
                        if ui.button("Close").clicked_by(egui::PointerButton::Primary) {
                            close = Some(i);
                            ui.close_menu();
                        }
                        if ui.button("Close Others").clicked_by(egui::PointerButton::Primary) {
                            close_others = Some(i);
                            ui.close_menu();
                        }
                        if ui.button("Close All to the Right").clicked_by(egui::PointerButton::Primary) {
                            close_right = Some(i);
                            ui.close_menu();
                        }
                        ui.separator();
                        if ui.button("Pin file    Alt+P").clicked_by(egui::PointerButton::Primary) {
                            pin = Some(i);
                            ui.close_menu();
                        }
                        if ui.button("Move to other split    Ctrl+\\").clicked_by(egui::PointerButton::Primary) {
                            move_split = Some(i);
                            ui.close_menu();
                        }
                        ui.separator();
                        if ui.button("Copy Path").clicked_by(egui::PointerButton::Primary) {
                            ui.output_mut(|o| o.copied_text = f.path.display().to_string());
                            ui.close_menu();
                        }
                        if ui.button("Copy Relative Path").clicked_by(egui::PointerButton::Primary) {
                            let rel = f.path.strip_prefix(&self.workspace.root).unwrap_or(&f.path);
                            ui.output_mut(|o| o.copied_text = rel.display().to_string());
                            ui.close_menu();
                        }
                        if ui.button("Reveal in Files").clicked_by(egui::PointerButton::Primary) {
                            if let Some(dir) = f.path.parent() {
                                let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
                            }
                            ui.close_menu();
                        }
                    });
                }
            });
        });
        ui.advance_cursor_after_rect(strip_rect);

        if let Some((i, label)) = start_drag {
            self.tab_drag = Some(TabDrag { from_group: gi, from_index: i, label });
        }
        if let Some((a, b)) = swap {
            let g = &mut self.groups[gi];
            g.files.swap(a, b);
            if g.active == a {
                g.active = b;
            } else if g.active == b {
                g.active = a;
            }
            // The dragged tab moved indices — keep tab_drag pointing at it so the eventual drop
            // relocates the RIGHT file.
            if let Some(d) = &mut self.tab_drag {
                if d.from_group == gi && d.from_index == a {
                    d.from_index = b;
                }
            }
        }
        if let Some(i) = activate {
            self.groups[gi].active = i;
            self.focused = gi;
        }
        // A just-clicked lazy tab hydrates BEFORE the editor paints. This may close the tab
        // (file vanished since the session) and collapse an emptied group — hence the guard.
        self.ensure_active_loaded(gi);

        // --- the editor -------------------------------------------------------------------------
        let Some(g) = self.groups.get_mut(gi) else { return };
        if let Some(f) = g.files.get_mut(g.active) {
            let clicked_inside = ui.rect_contains_pointer(ui.available_rect_before_wrap())
                && ui.input(|i| i.pointer.any_pressed());
            if clicked_inside {
                self.focused = gi;
            }
            f.view.ui(ui, &mut f.buffer);
        } else {
            empty_state(ui);
        }

        // --- deferred tab actions ------------------------------------------------------------------
        if let Some(i) = pin {
            let p = self.groups[gi].files[i].path.clone();
            if !self.pins.contains(&p) {
                self.pins.push(p);
                self.pins_open = true;
            }
        }
        if let Some(i) = move_split {
            self.focused = gi;
            self.groups[gi].active = i;
            self.split_move_right();
        }
        // Closes are DEFERRED to after the columns loop (drained in the caller): applying them
        // here can collapse a group and shrink self.groups mid-render, crashing the next column.
        // Paths are resolved NOW (indices are valid here) and re-located by path at apply time.
        if let Some(i) = close_others {
            if let Some(g) = self.groups.get(gi) {
                if let Some(keep) = g.files.get(i).map(|f| f.path.clone()) {
                    let paths: Vec<PathBuf> =
                        g.files.iter().map(|f| f.path.clone()).filter(|p| *p != keep).collect();
                    self.pending_tab_closes.push((gi, paths));
                }
            }
        }
        if let Some(i) = close_right {
            if let Some(g) = self.groups.get(gi) {
                let paths: Vec<PathBuf> =
                    g.files.iter().skip(i + 1).map(|f| f.path.clone()).collect();
                self.pending_tab_closes.push((gi, paths));
            }
        }
        if let Some(i) = close {
            if let Some(p) = self.groups.get(gi).and_then(|g| g.files.get(i)).map(|f| f.path.clone()) {
                self.pending_tab_closes.push((gi, vec![p]));
            }
        }
    }

    /// Apply the tab closes deferred by [`Self::group_ui`] — called AFTER the columns loop so a
    /// group collapse can't shrink `self.groups` while the panes are still rendering.
    fn apply_pending_tab_closes(&mut self) {
        for (gi, paths) in std::mem::take(&mut self.pending_tab_closes) {
            self.request_close_tabs(gi, paths);
        }
    }

    /// The dock's right slot: [Output | Problems] tab header + the active tab's body.
    fn right_slot_ui(&mut self, ui: &mut egui::Ui) -> (Option<usize>, Option<(PathBuf, usize)>) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            // Contextual tab row: quiet tools stay hidden until they have something to show
            // (or are active) — Output/Problems/Tests/PR Checks are the always-on core.
            let show = |tab: BottomTab, app: &App| -> bool {
                match tab {
                    BottomTab::Usages => !app.usages.is_empty(),
                    BottomTab::Debug => app.dap.is_running() || !app.dbg_console.is_empty(),
                    BottomTab::Git => app.workspace.git_branch().is_some(),
                    _ => true,
                }
            };
            for (tab, label) in [
                (BottomTab::Output, "Output"),
                (BottomTab::Problems, "Problems"),
                (BottomTab::Git, "Git"),
                (BottomTab::History, "History"),
                (BottomTab::Prs, "PRs"),
                (BottomTab::Usages, "Usages"),
                (BottomTab::Debug, "Debug"),
                (BottomTab::Tests, "Tests"),
                (BottomTab::Checks, "PR Checks"),
            ] {
                let active = self.bottom_tab == tab;
                if !active && !show(tab, self) {
                    continue;
                }
                let text = if active {
                    egui::RichText::new(label).size(12.0)
                } else {
                    egui::RichText::new(label).size(11.5).color(colors::TEXT_FAINT())
                };
                if ui.selectable_label(active, text).clicked_by(egui::PointerButton::Primary) {
                    self.bottom_tab = tab;
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("✕").clicked_by(egui::PointerButton::Primary) {
                    self.bottom_open = false;
                }
            });
        });
        style::hairline(ui);
        match self.bottom_tab {
            BottomTab::Output => {
                self.runner.ui_embedded(ui);
                (None, None)
            }
            BottomTab::Problems => self.problems_ui(ui),
            BottomTab::Git => {
                let root = self.workspace.root.clone();
                if self.git_panel.take_repo_changed() {
                    // A commit/checkout/pull just landed: every cached blame (and the
                    // "uncommitted changes" labels especially) is stale repo-wide, and so is
                    // the History tab's log.
                    self.blame.clear();
                    self.history.mark_stale();
                }
                if let Some((abs, staged)) = self.git_panel.ui(ui, &root) {
                    // JetBrains behaviour: clicking a changed file shows its DIFF; the diff
                    // header's "Open in Editor" jumps to the real file. Staged rows open the
                    // index-vs-HEAD comparison, unstaged rows the worktree-vs-index one.
                    let mode = if staged {
                        diffview::DiffMode::Staged
                    } else {
                        diffview::DiffMode::Unstaged
                    };
                    self.open_diff_mode(&abs, mode);
                }
                (None, None)
            }
            BottomTab::History => {
                let root = self.workspace.root.clone();
                if let Some((abs, sha)) = self.history.ui(ui, &root) {
                    if let Some(v) = diffview::open_commit(&root, &abs, &sha) {
                        self.diff_view = Some(v);
                    }
                }
                if let Some(action) = self.history.take_action() {
                    self.run_history_action(action, &root);
                }
                (None, None)
            }
            BottomTab::Prs => {
                let root = self.workspace.root.clone();
                match self.prs.ui(ui, &root) {
                    Some(prpanel::PrAction::OpenFileDiff(rel, chunk, label)) => {
                        self.diff_view =
                            Some(diffview::from_diff_text(&root, &rel, &chunk, &label));
                    }
                    Some(prpanel::PrAction::CheckedOut) => {
                        // The worktree just moved to the PR branch: same invalidation set as
                        // any repo-mutating git action.
                        let ctx2 = ui.ctx().clone();
                        self.git_panel.refresh(&root, &ctx2);
                        self.blame.clear();
                        self.history.mark_stale();
                        self.reload_externally_changed_buffers(&ctx2);
                        self.lsp_message = Some("checked out the pull request branch".into());
                    }
                    None => {}
                }
                (None, None)
            }
            BottomTab::Debug => {
                self.debug_ui(ui);
                (None, None)
            }
            BottomTab::Tests => {
                ui.horizontal(|ui| {
                    if ui.add_enabled(!self.testrun.running, egui::Button::new("▶ Run Tests")).clicked_by(egui::PointerButton::Primary) {
                        let root = self.workspace.root.clone();
                        self.testrun.start(&root, ui.ctx());
                    }
                    if self.testrun.running {
                        ui.spinner();
                        if ui.button("⏹ Stop").clicked_by(egui::PointerButton::Primary) {
                            self.testrun.stop();
                        }
                    }
                });
                let mut open_at = None;
                if let Some((path, line)) = self.testrun.ui(ui) {
                    open_at = Some((path, line));
                }
                if let Some((path, line)) = open_at {
                    // Test locations are workspace-relative and 1-based (parse_file_line): a
                    // raw relative path resolves against the PROCESS cwd (failed open, or a
                    // duplicate tab shadowing the absolute-path one) — same conversions as the
                    // Checks arm below.
                    let abs = if path.is_absolute() {
                        path.clone()
                    } else {
                        self.workspace.root.join(&path)
                    };
                    self.open_file(abs.clone());
                    self.goto_file_line(&abs, line.saturating_sub(1));
                }
                (None, None)
            }
            BottomTab::Checks => {
                ui.horizontal(|ui| {
                    if ui.add_enabled(!self.checklist.is_running(), egui::Button::new("↻ Run PR checks")).clicked_by(egui::PointerButton::Primary) {
                        let root = self.workspace.root.clone();
                        self.checklist.run(root, ui.ctx().clone());
                    }
                    if self.checklist.is_running() {
                        ui.spinner();
                    }
                    ui.colored_label(
                        colors::TEXT_FAINT(),
                        "the real cFE CI gates, run locally against your branch",
                    );
                });
                let mut open_at = None;
                if let Some((path, line)) = self.checklist.ui(ui) {
                    open_at = Some((path, line));
                }
                if let Some((path, line)) = open_at {
                    let abs = if path.is_absolute() { path.clone() } else { self.workspace.root.join(&path) };
                    self.open_file(abs.clone());
                    self.goto_file_line(&abs, line.saturating_sub(1));
                }
                (None, None)
            }
            BottomTab::Usages => {
                let mut jump: Option<(PathBuf, usize)> = None;
                if self.usages.is_empty() {
                    ui.colored_label(colors::TEXT_FAINT(), "Alt+F7 on a symbol to find its usages");
                } else if self.usages_are_callers {
                    ui.colored_label(
                        colors::TEXT_FAINT(),
                        format!("{} callers (Ctrl+Alt+H) — click to jump to the call site", self.usages.len()),
                    );
                } else if self.usages_from_index {
                    ui.colored_label(
                        colors::TEXT_FAINT(),
                        format!(
                            "{} usages — PSI index results (no language server for this file)",
                            self.usages.len()
                        ),
                    );
                } else {
                    ui.colored_label(
                        colors::TEXT_FAINT(),
                        format!("{} usages", self.usages.len()),
                    );
                }
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    for (path, line, preview) in &self.usages {
                        let rel = path.strip_prefix(&self.workspace.root).unwrap_or(path);
                        let mut job = egui::text::LayoutJob::default();
                        let font = egui::TextStyle::Monospace.resolve(ui.style());
                        job.append(
                            &format!("{}:{}  ", rel.display(), line + 1),
                            0.0,
                            egui::TextFormat {
                                font_id: font.clone(),
                                color: colors::AMBER(),
                                ..Default::default()
                            },
                        );
                        job.append(
                            preview,
                            0.0,
                            egui::TextFormat {
                                font_id: font,
                                color: colors::TEXT_MUTED(),
                                ..Default::default()
                            },
                        );
                        if ui.selectable_label(false, job).clicked_by(egui::PointerButton::Primary) {
                            jump = Some((path.clone(), *line));
                        }
                    }
                });
                if let Some((path, line)) = jump {
                    self.open_file(path);
                    if let Some(f) = self.groups[self.focused].active_file() {
                        let rope = f.buffer.rope().clone();
                        let byte = rope.line_to_byte(line.min(rope.len_lines().saturating_sub(1)));
                        f.view.jump_to(byte, &rope);
                    }
                }
                (None, None)
            }
        }
    }

    /// Color-coded, column-aligned problems list: severity | line | source | message, then the
    /// PROJECT · Power of Ten scope with exact rule citations.
    fn problems_ui(&mut self, ui: &mut egui::Ui) -> (Option<usize>, Option<(PathBuf, usize)>) {
        let mut jump = None;
        let mut project_jump = None;
        let mut fix_req: Option<(PathBuf, std::ops::Range<usize>)> = None;
        // (path, flagged range, message, task) — AI actions chosen from a diagnostic row.
        let mut ai_req: Option<(PathBuf, std::ops::Range<usize>, String, ai_actions::AiTaskKind)> = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            let g = &self.groups[self.focused];
            if let Some(f) = g.files.get(g.active) {
                let rope = f.buffer.rope();
                let diags = self.diags.merged(&f.path);
                if !diags.is_empty() {
                    // Fast triage: the whole list (or one row via right-click) is one click
                    // away from a bug report / AI prompt / teammate ping.
                    if crate::style::tool_button(ui, "Copy all", false)
                        .on_hover_text("Copy every problem in this file as file:line: message lines")
                        .clicked_by(egui::PointerButton::Primary)
                    {
                        let rel = f.path.strip_prefix(&self.workspace.root).unwrap_or(&f.path);
                        let all: String = diags
                            .iter()
                            .filter(|d| d.severity != 4)
                            .map(|d| {
                                let line = rope.byte_to_line(d.range.start.min(rope.len_bytes()));
                                format!("{}:{}: {}", rel.display(), line + 1, d.message)
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        ui.output_mut(|o| o.copied_text = all);
                    }
                    egui::Grid::new("problems-grid").num_columns(4).spacing([10.0, 2.0]).show(
                        ui,
                        |ui| {
                            for d in &diags {
                                if d.severity == 4 {
                                    continue; // hints hidden by default
                                }
                                let (icon, color) = match d.severity {
                                    1 => ("✕", colors::ERROR()),
                                    2 => ("⚠", colors::WARN()),
                                    5 => ("☢", colors::ACCENT()), // NASA layer
                                    6 => ("🛡", egui::Color32::from_rgb(180, 142, 173)), // guarded recursion
                                    7 => ("⚒", egui::Color32::from_rgb(191, 115, 100)), // tooling recursion
                                    _ => ("ℹ", colors::TEXT_FAINT()),
                                };
                                let line = rope.byte_to_line(d.range.start.min(rope.len_bytes()));
                                let (source, text) = split_source(&d.message);
                                ui.colored_label(color, icon);
                                ui.colored_label(
                                    colors::TEXT_FAINT(),
                                    egui::RichText::new(format!("{:>5}", line + 1)).monospace(),
                                );
                                ui.colored_label(
                                    colors::TEXT_FAINT(),
                                    egui::RichText::new(format!("{source:<14}"))
                                        .monospace()
                                        .size(11.0),
                                );
                                let row = ui.selectable_label(
                                    false,
                                    egui::RichText::new(text).size(12.0).color(color),
                                );
                                if row.clicked_by(egui::PointerButton::Primary) {
                                    jump = Some(d.range.start);
                                }
                                row.context_menu(|ui| {
                                    if ui.button("Copy").clicked_by(egui::PointerButton::Primary) {
                                        let rel = f.path.strip_prefix(&self.workspace.root).unwrap_or(&f.path);
                                        ui.output_mut(|o| {
                                            o.copied_text = format!("{}:{}: {}", rel.display(), line + 1, d.message)
                                        });
                                        ui.close_menu();
                                    }
                                    if ui.button("Copy message only").clicked_by(egui::PointerButton::Primary) {
                                        ui.output_mut(|o| o.copied_text = d.message.clone());
                                        ui.close_menu();
                                    }
                                    if ui.button("Quick fixes…").clicked_by(egui::PointerButton::Primary) {
                                        fix_req = Some((f.path.clone(), d.range.clone()));
                                        ui.close_menu();
                                    }
                                    if ui.button("AI: Show Hot Fix").clicked_by(egui::PointerButton::Primary) {
                                        ai_req = Some((
                                            f.path.clone(),
                                            d.range.clone(),
                                            d.message.clone(),
                                            ai_actions::AiTaskKind::FixDiagnostic,
                                        ));
                                        ui.close_menu();
                                    }
                                    if ui.button("AI: Explain").clicked_by(egui::PointerButton::Primary) {
                                        ai_req = Some((
                                            f.path.clone(),
                                            d.range.clone(),
                                            d.message.clone(),
                                            ai_actions::AiTaskKind::ExplainDiagnostic,
                                        ));
                                        ui.close_menu();
                                    }
                                });
                                ui.end_row();
                            }
                        },
                    );
                }
            }

            // ---- PROJECT scope: whole-program Rule-1 -------------------------------------------
            match &self.psi.state {
                PsiState::NotCProject => {}
                PsiState::Indexing => {
                    ui.add_space(6.0);
                    style::panel_header_inline(ui, match self.standards { Standards::JplPot => "Project · JPL Power of Ten", _ => "Project · GSFC 582 / cFS conventions" });
                    ui.colored_label(colors::TEXT_FAINT(), "indexing…");
                }
                PsiState::Ready { findings, files_indexed, elapsed_ms, .. } => {
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        style::panel_header_inline(ui, match self.standards { Standards::JplPot => "Project · JPL Power of Ten", _ => "Project · GSFC 582 / cFS conventions" });
                        ui.colored_label(
                            colors::TEXT_FAINT(),
                            egui::RichText::new(format!("{files_indexed} files · {elapsed_ms} ms"))
                                .size(11.0),
                        );
                    });
                    if findings.is_empty() {
                        ui.colored_label(
                            colors::MOSS(),
                            "rule 1 (no recursion): call graph acyclic ✓",
                        );
                    }
                    for (fi, finding) in findings.iter().enumerate() {
                        let (icon, color) = if finding.macro_textual {
                            ("⚠", colors::WARN())
                        } else if finding.guarded && self.standards != Standards::JplPot {
                            // Bounded by a recognized re-entry guard — informative, not alarming.
                            ("🛡", egui::Color32::from_rgb(180, 142, 173))
                        } else if finding.tooling {
                            // Host-side tooling: real recursion, softer stakes.
                            ("⚒", egui::Color32::from_rgb(191, 115, 100))
                        } else {
                            ("☢", colors::ERROR())
                        };
                        let chain: Vec<&str> =
                            finding.hops.iter().map(|h| h.func.as_str()).collect();
                        let title = format!(
                            "{icon} pot-1-recursion — {}-member cycle: {}",
                            finding.members.len(),
                            chain.join(" → "),
                        );
                        egui::CollapsingHeader::new(
                            egui::RichText::new(title).size(12.0).color(color),
                        )
                        .id_salt(("rule1", fi))
                        .show(ui, |ui| {
                            // EXACTLY what is violated — the verbatim rule.
                            ui.label(
                                egui::RichText::new(RULE1_TEXT)
                                    .size(11.5)
                                    .italics()
                                    .color(colors::TEXT_MUTED()),
                            );
                            if finding.macro_textual {
                                ui.colored_label(
                                    colors::TEXT_FAINT(),
                                    "all members are macros — likely a unioned #if configuration; \
                                     verify one build branch actually closes the loop",
                                );
                            }
                            ui.add_space(2.0);
                            for (hi, hop) in finding.hops.iter().enumerate() {
                                let next = &finding.hops[(hi + 1) % finding.hops.len()].func;
                                let file_disp = hop
                                    .file
                                    .strip_prefix(&self.workspace.root)
                                    .unwrap_or(&hop.file)
                                    .display();
                                let row = format!(
                                    "    {}:{}  {} → {next}",
                                    file_disp,
                                    hop.line + 1,
                                    hop.func,
                                );
                                if ui
                                    .selectable_label(
                                        false,
                                        egui::RichText::new(row)
                                            .size(12.0)
                                            .monospace()
                                            .color(colors::TEXT_MUTED()),
                                    )
                                    .clicked_by(egui::PointerButton::Primary)
                                {
                                    project_jump = Some((hop.file.clone(), hop.offset));
                                }
                            }
                        });
                    }
                }
            }
        });
        if let Some((path, range)) = fix_req {
            self.request_quick_fixes(path, range);
        }
        if let Some((path, range, message, kind)) = ai_req {
            self.submit_diagnostic_ai(path, range, message, kind, ui.ctx());
        }
        (jump, project_jump)
    }

    /// Open `path` and land on the LSP `target` position, recording the hop for Alt+Left.
    /// The call site is captured BEFORE opening (open_file moves focus); the target byte
    /// needs the destination's rope, so: open, resolve, jump, record.
    fn jump_to_lsp_location(&mut self, path: PathBuf, target: lsp_types::Position) {
        let enc = self.lsp_encoding(&path);
        let from = self.current_location();
        self.open_file(path.clone());
        let byte = self.groups[self.focused]
            .active_file()
            .map(|f| pos_to_byte(&f.buffer.rope().clone(), &target, enc))
            .unwrap_or(0);
        self.open_and_jump(path.clone(), byte);
        if let Some(from) = from {
            self.nav.record(from, nav::NavPoint::new(path, byte));
        }
    }

    /// Open the merge-conflict resolver on the active file, if it has conflict markers.
    fn open_conflict_resolver(&mut self) {
        let Some(f) = self.groups[self.focused].active_file() else { return };
        let path = f.path.clone();
        if conflicts::parse(&f.buffer.rope().to_string()).is_empty() {
            self.lsp_message = Some("no merge conflicts in this file".into());
            return;
        }
        self.conflict_file = Some(path);
    }

    /// The merge-conflict resolver overlay: walks the FIRST remaining conflict in the target
    /// file, showing ours/theirs and Accept buttons. Each choice replaces the conflict region
    /// (markers included) with the chosen text through the undo-safe editor path; re-parsing
    /// next frame surfaces the next conflict. Esc closes.
    fn conflict_resolver_ui(&mut self, ctx: &egui::Context) {
        let Some(path) = self.conflict_file.clone() else { return };
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.conflict_file = None;
            return;
        }
        // Parse from the LIVE buffer (offsets shift as conflicts are resolved).
        let Some(f) = self.groups.iter().flat_map(|g| g.files.iter()).find(|f| f.path == path && f.loaded)
        else {
            self.conflict_file = None;
            return;
        };
        let text = f.buffer.rope().to_string();
        let all = conflicts::parse(&text);
        let total = all.len();
        let Some(conflict) = all.into_iter().next() else {
            // All resolved.
            self.conflict_file = None;
            self.lsp_message = Some("all conflicts resolved".into());
            return;
        };
        // Jump the editor to this conflict.
        let start = conflict.start;
        if let Some(f) = self.find_file_mut(&path) {
            let rope = f.buffer.rope().clone();
            f.view.jump_to(start.min(rope.len_bytes()), &rope);
        }
        let mut choice: Option<conflicts::Side> = None;
        egui::Area::new("conflict-resolver".into())
            .anchor(egui::Align2::CENTER_TOP, [0.0, 60.0])
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .inner_margin(egui::Margin::same(style::sizes::OVERLAY_PAD))
                    .show(ui, |ui| {
                        ui.set_width(620.0);
                        ui.horizontal(|ui| {
                            style::panel_header_inline(ui, "Resolve merge conflict");
                            ui.colored_label(colors::TEXT_FAINT(), format!("{total} remaining"));
                        });
                        let preview = |ui: &mut egui::Ui, label: &str, body: &str, color: egui::Color32| {
                            ui.colored_label(color, label);
                            egui::Frame::none()
                                .fill(colors::BG_INPUT())
                                .rounding(egui::Rounding::same(4.0))
                                .inner_margin(egui::Margin::same(6.0))
                                .show(ui, |ui| {
                                    let shown: String = body.lines().take(8).collect::<Vec<_>>().join("\n");
                                    ui.add(egui::Label::new(
                                        egui::RichText::new(if shown.is_empty() { "(empty)" } else { &shown })
                                            .monospace()
                                            .size(12.0)
                                            .color(colors::TEXT_MUTED()),
                                    ).wrap());
                                });
                        };
                        preview(ui, "Ours (current branch)", &conflict.ours, colors::MOSS());
                        preview(ui, "Theirs (incoming)", &conflict.theirs, colors::ACCENT_HI());
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if ui.button("Accept Ours").clicked_by(egui::PointerButton::Primary) {
                                choice = Some(conflicts::Side::Ours);
                            }
                            if ui.button("Accept Theirs").clicked_by(egui::PointerButton::Primary) {
                                choice = Some(conflicts::Side::Theirs);
                            }
                            if ui.button("Accept Both").clicked_by(egui::PointerButton::Primary) {
                                choice = Some(conflicts::Side::Both);
                            }
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.button("Close  Esc").clicked_by(egui::PointerButton::Primary) {
                                    self.conflict_file = None;
                                }
                            });
                        });
                    });
            });
        if let Some(side) = choice {
            let replacement = conflict.resolved(side);
            let now = ctx.input(|i| i.time);
            if let Some(f) = self.find_file_mut(&path) {
                f.view.replace_range(&mut f.buffer, conflict.start..conflict.end, &replacement, now);
                f.dirty = true;
            }
        }
    }

    /// The multi-target definition picker: a small popup at the caret listing every site
    /// (`file:line`); ↑/↓ move, Enter/click jump, Esc dismisses.
    fn def_choices_ui(&mut self, ctx: &egui::Context) {
        let Some((pos, targets, selected)) = &mut self.def_choices else { return };
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.def_choices = None;
            return;
        }
        let n = targets.len();
        if ctx.input(|i| i.key_pressed(egui::Key::ArrowDown)) {
            *selected = (*selected + 1) % n;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
            *selected = (*selected + n - 1) % n;
        }
        // Tab accepts, like the completion popup — Enter would race the editor's newline
        // (the editor processes it earlier in the frame; see the popup's comment). The
        // suppress_nav_keys OR in drive_completion_popup keeps the editor's hands off
        // arrows/Tab/Esc while this is open.
        let mut chosen: Option<usize> = None;
        if ctx.input(|i| i.key_pressed(egui::Key::Tab)) {
            chosen = Some(*selected);
        }
        let root = self.workspace.root.clone();
        egui::Area::new("def-choices".into())
            .fixed_pos(*pos + egui::vec2(0.0, 6.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_max_width(520.0);
                    ui.colored_label(colors::TEXT_FAINT(), format!("{n} definitions — ↑↓, Tab jumps, Esc closes"));
                    for (i, (path, target)) in targets.iter().enumerate() {
                        let rel = path.strip_prefix(&root).unwrap_or(path);
                        let row = format!("{}:{}", rel.display(), target.line + 1);
                        if ui
                            .selectable_label(
                                i == *selected,
                                egui::RichText::new(row).monospace().size(12.0),
                            )
                            .clicked_by(egui::PointerButton::Primary)
                        {
                            chosen = Some(i);
                        }
                    }
                });
            });
        if let Some(i) = chosen {
            if let Some((_, mut targets, _)) = self.def_choices.take() {
                if i < targets.len() {
                    let (path, target) = targets.remove(i);
                    self.jump_to_lsp_location(path, target);
                }
            }
        }
    }

    /// Context auto-attached to freeform panel questions: the active file's selection when
    /// one exists (with an Origin so reply code blocks are Apply-able), else ±60 lines
    /// around the caret, plus the file's top error/warning messages. Cheap enough to
    /// rebuild every frame the AI tab is visible.
    fn current_ask_context(&self) -> ai_actions::AiContext {
        let g = &self.groups[self.focused];
        let Some(f) = g.files.get(g.active).filter(|f| f.loaded) else {
            return ai_actions::AiContext::default();
        };
        let rope = f.buffer.rope();
        let len = rope.len_bytes();
        let sel = f.view.selection_byte_range();
        let (code, origin) = if !sel.is_empty() && sel.end <= len {
            let origin = ai_actions::Origin {
                path: f.path.clone(),
                range: sel.clone(),
                generation: f.buffer.generation,
            };
            (rope.byte_slice(sel).to_string(), Some(origin))
        } else {
            let line = rope.byte_to_line(f.view.caret_byte().min(len));
            let ls = line.saturating_sub(60);
            let le = (line + 61).min(rope.len_lines());
            let sb = rope.line_to_byte(ls);
            let eb = if le >= rope.len_lines() { len } else { rope.line_to_byte(le) };
            (rope.byte_slice(sb..eb).to_string(), None)
        };
        let diags = self.diags.merged(&f.path);
        let joined: String = diags
            .iter()
            .filter(|d| d.severity <= 2)
            .take(5)
            .map(|d| d.message.replace('\n', " "))
            .collect::<Vec<_>>()
            .join("\n");
        ai_actions::AiContext {
            file_name: f.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            language: f.path.extension().and_then(|e| e.to_str()).unwrap_or("").to_string(),
            code,
            diagnostic: (!joined.is_empty()).then_some(joined),
            extra: String::new(),
            origin,
        }
    }

    /// Send a diagnostic to the AI panel: the flagged span expanded to whole lines (+2 lines
    /// of context each side) so the model sees the construct, with an Origin over that span —
    /// the hot-fix reply's code block Applies straight over the violation.
    fn submit_diagnostic_ai(
        &mut self,
        path: PathBuf,
        range: std::ops::Range<usize>,
        message: String,
        kind: ai_actions::AiTaskKind,
        ctx: &egui::Context,
    ) {
        let Some(f) = self.find_file_mut(&path) else { return };
        let rope = f.buffer.rope().clone();
        let len = rope.len_bytes();
        let ls = rope.byte_to_line(range.start.min(len)).saturating_sub(2);
        let le = (rope.byte_to_line(range.end.min(len)) + 3).min(rope.len_lines());
        let sb = rope.line_to_byte(ls);
        let eb = if le >= rope.len_lines() { len } else { rope.line_to_byte(le) };
        let code = rope.byte_slice(sb..eb).to_string();
        let origin = Some(ai_actions::Origin {
            path: path.clone(),
            range: sb..eb,
            generation: f.buffer.generation,
        });
        let lang = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_string();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        self.ai_panel.submit(
            kind,
            ai_actions::AiContext {
                file_name: name,
                language: lang,
                code,
                diagnostic: Some(message),
                origin,
                ..Default::default()
            },
            ctx,
        );
        self.pins_open = true;
        self.right_tab = RightTab::Ai;
    }

    /// Ask the file's language server for code actions covering `range`.
    fn request_quick_fixes(&mut self, path: PathBuf, range: std::ops::Range<usize>) {
        if let Some(f) =
            self.groups.iter().flat_map(|g| g.files.iter()).find(|f| f.path == path && f.loaded)
        {
            let gen = f.buffer.generation;
            let rope = f.buffer.rope().clone();
            self.fix_request_gen = Some(gen);
            self.fix_request_refactor = false;
            self.lsp.request_code_actions(&path, &rope, range, gen);
            self.lsp_message = Some("fetching quick fixes…".into());
        }
    }

    /// Is Change Signature offerable for the active file? C needs the PSI index; Rust needs
    /// only a language server, since rust-analyzer supplies the reference set.
    fn change_signature_available(&self) -> bool {
        let g = &self.groups[self.focused];
        match g.files.get(g.active).and_then(|f| f.lang) {
            Some(cauldron_editor::syntax::Lang::Rust) => true,
            Some(cauldron_editor::syntax::Lang::C) => self.psi.index().is_some(),
            _ => false,
        }
    }

    /// Ctrl+F6 — open Change Signature for the function at the caret.
    ///
    /// Two engines, because the languages need different machinery. C is driven by the PSI
    /// index, which knows every call site by name. Rust cannot be: `new`/`len`/`run` are reused
    /// across unrelated types and `x.foo(a)` needs type inference to resolve, so the reference
    /// set comes from rust-analyzer and only the SPANS are parsed locally.
    fn start_change_signature(&mut self) {
        let Some(f) = self.groups[self.focused].active_file() else { return };
        let (path, byte) = (f.path.clone(), f.view.caret_byte());
        let text = f.buffer.rope().to_string();
        let is_rust = matches!(f.lang, Some(cauldron_editor::syntax::Lang::Rust));
        if is_rust {
            self.start_change_signature_rust(path, byte, text);
            return;
        }
        let Some(index) = self.psi.index() else {
            self.lsp_message =
                Some("Change Signature needs the C index or a Rust language server".into());
            return;
        };
        let Some(name) = cauldron_psi::chsig::function_at(&index, &path, byte) else {
            self.lsp_message = Some("put the caret on a function or one of its calls".into());
            return;
        };
        // Seed from wherever the parameter list actually lives: the caret may be on a CALL, in
        // which case this file may hold no declaration of the function at all.
        let params = cauldron_psi::chsig::current_params(&index, &name, &text, &path)
            .or_else(|| {
                let d = *index.defs_by_name(&name).first().or(index.decls_by_name(&name).first())?;
                let decl_path = index.path(d.file)?.to_path_buf();
                let decl_text = self.text_for_refactor(&decl_path)?;
                cauldron_psi::chsig::current_params(&index, &name, &decl_text, &decl_path)
            })
            .unwrap_or_default();
        self.close_overlays();
        self.chsig = Some(chsig_ui::ChangeSigUi::new(chsig_ui::Engine::C, name, path, params));
    }

    /// Rust path: seed the dialog from the local parse, then ask rust-analyzer for every
    /// reference. The dialog opens immediately in a "finding references" state so the user is
    /// not left staring at nothing while r-a answers.
    fn start_change_signature_rust(&mut self, path: PathBuf, byte: usize, text: String) {
        // Resolve what the caret MEANS, not what encloses it: on `t.m(1)` the target is `m`, and
        // falling back to the enclosing function would silently refactor the wrong thing.
        let Some(name) = cauldron_psi::rustsig::target_name_at(&text, byte) else {
            self.lsp_message =
                Some("put the caret on a function, method, or one of its calls".into());
            return;
        };
        let Some(f) =
            self.groups.iter().flat_map(|g| g.files.iter()).find(|f| f.path == path && f.loaded)
        else {
            return;
        };
        let rope = f.buffer.rope().clone();
        // The declaration is usually in ANOTHER file, so the parameter rows cannot be seeded
        // here — they arrive with the reference set (see `chsig_take_references`). Seed locally
        // only when the caret is genuinely on the declaration itself.
        let params = cauldron_psi::rustsig::current_params(&text, byte)
            .filter(|(n, _)| *n == name)
            .map(|(_, p)| p)
            .unwrap_or_default();
        self.close_overlays();
        self.chsig = Some(chsig_ui::ChangeSigUi::new(
            chsig_ui::Engine::Rust(None),
            name,
            path.clone(),
            params,
        ));
        // A DISTINCT generation namespace: `usages_gen` and this both key References replies off
        // a bare u64, and both would otherwise be the same buffer generation — so Find Usages
        // while the dialog is open cross-wired the two. Buffer generations count up from 0 and
        // never reach this range.
        self.chsig_req_seq = self.chsig_req_seq.wrapping_add(1);
        let gen = CHSIG_GEN_BASE + self.chsig_req_seq;
        self.chsig_refs_gen = Some(gen);
        self.lsp.request_references(&path, &rope, byte, gen);
    }

    /// Turn rust-analyzer's reference Locations into byte offsets in each file's CURRENT text,
    /// then hand them to the open dialog. Positions are in the server's negotiated encoding, and
    /// a file that is open and dirty must be measured against the buffer, not the disk copy.
    fn chsig_take_references(&mut self, locations: &[lsp_types::Location]) {
        let mut refs: Vec<cauldron_psi::rustsig::Reference> = Vec::new();
        let mut texts: HashMap<PathBuf, String> = HashMap::new();
        for loc in locations {
            let Some(path) = cauldron_lsp::capabilities::uri_to_path(&loc.uri) else { continue };
            let enc = self.lsp_encoding(&path);
            let text = match texts.get(&path) {
                Some(t) => t.clone(),
                None => {
                    let Some(t) = self.text_for_refactor(&path) else { continue };
                    texts.insert(path.clone(), t.clone());
                    t
                }
            };
            let rope = Rope::from_str(&text);
            let offset = pos_to_byte(&rope, &loc.range.start, enc);
            refs.push(cauldron_psi::rustsig::Reference { path, offset });
        }
        // The declaration among the references is what supplies the parameter rows.
        let name = self.chsig.as_ref().map(|d| d.function.clone()).unwrap_or_default();
        let params = cauldron_psi::rustsig::params_from_references(&refs, &name, |p| {
            self.text_for_refactor(p)
        });
        if let Some(d) = &mut self.chsig {
            if let Some(params) = params {
                d.seed_params(params);
            }
            d.engine = chsig_ui::Engine::Rust(Some(refs));
            d.mark_dirty();
        }
    }

    /// Current text of `path` for refactoring:    /// Current text of `path` for refactoring: the live buffer when the file is open and loaded
    /// (the PSI index carries buffer-coordinate overlay facts for dirty files, so spans and text
    /// must come from the same place), otherwise disk.
    fn text_for_refactor(&self, path: &Path) -> Option<String> {
        self.groups
            .iter()
            .flat_map(|g| g.files.iter())
            .find(|f| f.path == path && f.loaded)
            .map(|f| f.buffer.rope().to_string())
            .or_else(|| std::fs::read_to_string(path).ok())
    }

    /// Draw the Change Signature dialog, refresh its preview, and apply on Refactor.
    fn change_signature_ui(&mut self, ctx: &egui::Context) {
        if self.chsig.is_none() {
            return;
        }
        // Recompute the preview before drawing, so the counts on screen match the current rows.
        if self.chsig.as_ref().is_some_and(chsig_ui::ChangeSigUi::needs_preview) {
            let change = self.chsig.as_ref().map(chsig_ui::ChangeSigUi::change);
            let engine = self.chsig.as_ref().map(|d| d.engine.clone());
            let from = self.chsig.as_ref().map(|d| d.path.clone());
            let preview = match (change, engine) {
                (Some(change), Some(chsig_ui::Engine::C)) => self.psi.index().map(|index| {
                    // Anchor on the invoking file so a `static` resolves to the function under
                    // the caret rather than a same-named one elsewhere.
                    cauldron_psi::chsig::plan_from(&index, &change, from.as_deref(), |p| {
                        self.text_for_refactor(p)
                    })
                    .map_err(|e| e.message())
                }),
                // Still waiting on rust-analyzer — leave the dialog in its pending state.
                (_, Some(chsig_ui::Engine::Rust(None))) => None,
                (Some(change), Some(chsig_ui::Engine::Rust(Some(refs)))) => Some(
                    cauldron_psi::rustsig::plan(&refs, &change, |p| self.text_for_refactor(p))
                        .map_err(|e| e.message()),
                ),
                _ => None,
            };
            if let (Some(preview), Some(ui)) = (preview, &mut self.chsig) {
                ui.set_preview(preview);
            }
        }
        let Some(dialog) = &mut self.chsig else { return };
        match dialog.ui(ctx) {
            chsig_ui::Action::None => {}
            chsig_ui::Action::Close => {
                self.chsig = None;
                self.chsig_refs_gen = None;
            }
            chsig_ui::Action::Apply(plan, change) => {
                self.chsig = None;
                self.chsig_refs_gen = None;
                self.apply_signature_plan(&plan, &change.function);
            }
        }
    }

    /// Apply a Change Signature plan through the same undo-safe path as any other refactor.
    fn apply_signature_plan(&mut self, plan: &cauldron_psi::chsig::Plan, name: &str) {
        // The plan's byte offsets belong to the index generation it was computed against. If the
        // index moved (a background reindex landed, or a buffer changed) they may now point at
        // different text, and applying them would corrupt files.
        if self.psi.index().is_some_and(|i| i.generation() != plan.generation) {
            self.lsp_message =
                Some("the index changed while the dialog was open — reopen Change Signature".into());
            return;
        }
        // 0.0 matches the other external-edit paths: a refactor never coalesces into a typing
        // group, so the timestamp is immaterial.
        let now = 0.0;
        let (mut files, mut edits) = (0usize, 0usize);
        let mut failures: Vec<String> = Vec::new();
        for fe in &plan.files {
            if fe.edits.is_empty() {
                continue;
            }
            files += 1;
            edits += fe.edits.len();
            if let Err(e) = self.apply_psi_edits(&fe.path, &fe.edits, now) {
                failures.push(e);
            }
        }
        if failures.is_empty() {
            self.lsp_message = Some(format!(
                "changed signature of `{name}` — {edits} edit(s) across {files} file(s)"
            ));
        } else {
            log::warn!("change signature failures: {}", failures.join("; "));
            self.lsp_message =
                Some(format!("change signature partly failed — {}", failures[0]));
        }
        self.refresh_nasa_squiggles();
    }

    /// Apply PSI byte edits (already DESCENDING) to one file — editor transaction when open,
    /// local-history-backed disk write otherwise. Mirrors [`Self::apply_text_edits`], but PSI
    /// edits are raw byte offsets and need no encoding conversion.
    fn apply_psi_edits(
        &mut self,
        path: &Path,
        edits: &[cauldron_psi::chsig::Edit],
        now: f64,
    ) -> Result<(), String> {
        let open = self
            .groups
            .iter_mut()
            .flat_map(|g| g.files.iter_mut())
            .find(|f| f.path == path && f.loaded);
        match open {
            Some(f) => {
                // Transaction wants ASCENDING disjoint changes; the plan is descending.
                let mut changes: Vec<cauldron_editor::buffer::Change> = edits
                    .iter()
                    .map(|e| cauldron_editor::buffer::Change {
                        start: e.range.start,
                        end: e.range.end,
                        text: e.text.clone(),
                    })
                    .collect();
                changes.sort_by_key(|c| c.start);
                let tx = cauldron_editor::Transaction { changes };
                f.view.apply_external(&mut f.buffer, &tx, now);
                f.dirty = true;
                Ok(())
            }
            None => {
                let text = std::fs::read_to_string(path)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                localhist::record(path, &text);
                let mut out = text;
                for e in edits {
                    if e.range.end > out.len() || !out.is_char_boundary(e.range.start) {
                        return Err(format!("{}: stale offsets, file changed", path.display()));
                    }
                    out.replace_range(e.range.clone(), &e.text);
                }
                std::fs::write(path, out).map_err(|e| format!("{}: {e}", path.display()))
            }
        }
    }

    /// Ask for REFACTORINGS only (Refactor This, Ctrl+Alt+Shift+T). Filtering server-side keeps
    /// the menu from filling with quick fixes for whatever diagnostics happen to overlap the
    /// caret — the thing that makes an unfiltered code-action menu useless for refactoring.
    fn request_refactorings(&mut self, path: PathBuf, range: std::ops::Range<usize>) {
        let Some(f) =
            self.groups.iter().flat_map(|g| g.files.iter()).find(|f| f.path == path && f.loaded)
        else {
            return;
        };
        let gen = f.buffer.generation;
        let rope = f.buffer.rope().clone();
        self.fix_request_gen = Some(gen);
        self.fix_request_refactor = true;
        self.lsp.request_code_actions_only(
            &path,
            &rope,
            range,
            &["refactor", "source"],
            gen,
        );
        self.lsp_message = Some("finding refactorings…".into());
    }

    /// Apply one chosen code action: WorkspaceEdit through the undo-safe editor path for open
    /// files (falls back to disk rewrite for others); bare commands go to executeCommand.
    fn apply_code_action(&mut self, path: &Path, action: &lsp_types::CodeActionOrCommand, now: f64) {
        match action {
            lsp_types::CodeActionOrCommand::Command(cmd) => {
                self.lsp.execute_command(path, cmd);
            }
            lsp_types::CodeActionOrCommand::CodeAction(a) => {
                // An action with neither edit nor command is DEFERRED, not empty: the server
                // parked the real work behind `data` and expects a codeAction/resolve round
                // trip. rust-analyzer ships every extract/inline refactor this way, so applying
                // one directly used to be a silent no-op.
                if a.edit.is_none() && a.command.is_none() && self.lsp.code_action_resolves(path) {
                    let gen = self
                        .groups
                        .iter()
                        .flat_map(|g| g.files.iter())
                        .find(|f| f.path == path && f.loaded)
                        .map_or(0, |f| f.buffer.generation);
                    self.action_resolve_pending = Some((path.to_path_buf(), gen, a.title.clone()));
                    self.lsp.resolve_code_action(path, a, gen);
                    self.lsp_message = Some(format!("{}…", a.title));
                    return;
                }
                if let Some(edit) = &a.edit {
                    self.apply_workspace_edit(edit, now);
                }
                if let Some(cmd) = &a.command {
                    self.lsp.execute_command(path, cmd);
                }
            }
        }
        self.lsp_message = None;
    }

    /// The position encoding for LSP coordinates targeting `path`. Falls through to the
    /// would-be owning server for files never didOpen'ed (rename WorkspaceEdits, first jump
    /// into an unopened file) — a blind UTF-16 default corrupted utf-8 positions there.
    fn lsp_encoding(&self, path: &Path) -> Encoding {
        self.lsp
            .encoding_for(path)
            .or_else(|| self.lsp.encoding_for_unopened(path))
            .unwrap_or(Encoding::Utf16)
    }

    fn apply_workspace_edit(&mut self, edit: &lsp_types::WorkspaceEdit, now: f64) {
        let ops = txsync::workspace_edit_to_ops(edit);
        let mut failures: Vec<String> = Vec::new();
        for op in ops {
            let res = match op {
                txsync::WorkspaceOp::Edit { path, edits } => self.apply_text_edits(&path, &edits, now),
                txsync::WorkspaceOp::Create { path, overwrite, ignore_if_exists } => {
                    self.create_file_op(&path, overwrite, ignore_if_exists)
                }
                txsync::WorkspaceOp::Rename { from, to, overwrite, ignore_if_exists } => {
                    self.rename_file_op(&from, &to, overwrite, ignore_if_exists)
                }
                txsync::WorkspaceOp::Delete { path, recursive, ignore_if_not_exists } => {
                    self.delete_file_op(&path, recursive, ignore_if_not_exists)
                }
            };
            if let Err(e) = res {
                failures.push(e);
            }
        }
        if !failures.is_empty() {
            // These used to be `let _ = fs::write(…)` — a half-applied refactor that reported
            // success. Surface it: a partially applied workspace edit is exactly when the user
            // most needs to know to check their tree.
            log::warn!("workspace edit had {} failure(s): {}", failures.len(), failures.join("; "));
            self.lsp_message = Some(format!(
                "refactor partly failed — {}",
                failures.first().cloned().unwrap_or_default()
            ));
        }
    }

    /// Text edits for one file: undo-safe editor transaction when the buffer is open and loaded,
    /// read-modify-write otherwise. `edits` must be DESCENDING by position (as
    /// [`txsync::workspace_edit_to_ops`] returns them).
    fn apply_text_edits(
        &mut self,
        file: &Path,
        edits: &[lsp_types::TextEdit],
        now: f64,
    ) -> Result<(), String> {
        let enc = self.lsp_encoding(file);
        // A lazy tab is NOT an open buffer: route its edits to the DISK branch (editing
        // the empty stub then saving would truncate the real file).
        let open = self
            .groups
            .iter_mut()
            .flat_map(|g| g.files.iter_mut())
            .find(|f| f.path == file && f.loaded);
        match open {
            Some(f) => {
                // Edits arrive DESCENDING by start → one back-to-front transaction. The
                // Transaction contract wants ASCENDING disjoint changes; reverse into order.
                let rope = f.buffer.rope().clone();
                let mut changes: Vec<cauldron_editor::buffer::Change> = edits
                    .iter()
                    .map(|te| {
                        let s = pos_to_byte(&rope, &te.range.start, enc);
                        let e = pos_to_byte(&rope, &te.range.end, enc).max(s);
                        cauldron_editor::buffer::Change {
                            start: s,
                            end: e,
                            text: te.new_text.clone(),
                        }
                    })
                    .collect();
                changes.reverse();
                changes.sort_by_key(|c| c.start);
                let tx = cauldron_editor::Transaction { changes };
                f.view.apply_external(&mut f.buffer, &tx, now);
                f.dirty = true;
                Ok(())
            }
            None => {
                // Not open: rope read-modify-write, edits already back-to-front. A closed file
                // has no undo stack, so snapshot into local history first — that is the only
                // way back if the refactor is wrong.
                let text = std::fs::read_to_string(file)
                    .map_err(|e| format!("{}: {e}", file.display()))?;
                localhist::record(file, &text);
                let mut rope = Rope::from_str(&text);
                for te in edits {
                    let s = pos_to_byte(&rope, &te.range.start, enc);
                    let e = pos_to_byte(&rope, &te.range.end, enc).max(s);
                    let (cs, ce) = (rope.byte_to_char(s), rope.byte_to_char(e));
                    rope.remove(cs..ce);
                    rope.insert(cs, &te.new_text);
                }
                std::fs::write(file, rope.to_string())
                    .map_err(|e| format!("{}: {e}", file.display()))
            }
        }
    }

    /// `CreateFile` resource op. Default LSP semantics: without `overwrite`, an existing file is
    /// left ALONE (and that is only an error when `ignoreIfExists` is also unset).
    fn create_file_op(
        &mut self,
        path: &Path,
        overwrite: bool,
        ignore_if_exists: bool,
    ) -> Result<(), String> {
        if path.exists() && !overwrite {
            return if ignore_if_exists {
                Ok(())
            } else {
                Err(format!("{} already exists", path.display()))
            };
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        std::fs::write(path, "").map_err(|e| format!("{}: {e}", path.display()))
    }

    /// `RenameFile` resource op — the file move behind Move refactorings. Open tabs follow the
    /// file, and the language server is told the doc moved, or it keeps diagnosing a ghost.
    fn rename_file_op(
        &mut self,
        from: &Path,
        to: &Path,
        overwrite: bool,
        ignore_if_exists: bool,
    ) -> Result<(), String> {
        if to.exists() && !overwrite {
            return if ignore_if_exists {
                Ok(())
            } else {
                Err(format!("{} already exists", to.display()))
            };
        }
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        std::fs::rename(from, to).map_err(|e| format!("{} → {}: {e}", from.display(), to.display()))?;
        self.retarget_open_tabs(from, to);
        Ok(())
    }

    /// `DeleteFile` resource op (Safe Delete). Closes any tab on the file first — a tab pointing
    /// at a deleted path saves it right back into existence.
    fn delete_file_op(
        &mut self,
        path: &Path,
        recursive: bool,
        ignore_if_not_exists: bool,
    ) -> Result<(), String> {
        if !path.exists() {
            return if ignore_if_not_exists {
                Ok(())
            } else {
                Err(format!("{} does not exist", path.display()))
            };
        }
        // Last-resort undo for a delete: stash the content in local history first.
        if path.is_file() {
            if let Ok(text) = std::fs::read_to_string(path) {
                localhist::record(path, &text);
            }
        }
        self.forget_open_tabs(path);
        let res = if path.is_dir() && recursive {
            std::fs::remove_dir_all(path)
        } else if path.is_dir() {
            std::fs::remove_dir(path)
        } else {
            std::fs::remove_file(path)
        };
        res.map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Point every open tab on `from` at `to` after a file move, and re-key the language server's
    /// document (close old uri, open new) so completions and diagnostics keep working.
    fn retarget_open_tabs(&mut self, from: &Path, to: &Path) {
        let mut moved_text: Option<String> = None;
        for g in &mut self.groups {
            for f in &mut g.files {
                if f.path == from {
                    f.path = to.to_path_buf();
                    f.lang = Lang::from_path(&to.to_string_lossy());
                    if f.loaded && moved_text.is_none() {
                        moved_text = Some(f.buffer.rope().to_string());
                    }
                }
            }
        }
        self.lsp.close_doc(from);
        if let Some(text) = moved_text {
            // Re-derive the language from the NEW path: a move can change the extension, and the
            // stale tab lang would route the doc to the wrong server.
            if let Some(lang) = Lang::from_path(&to.to_string_lossy()) {
                let root = self.workspace.root.clone();
                self.lsp.open_doc(lang, &root, to, &text);
            }
        }
    }

    /// Drop any tab (and language-server doc) pointing at a path that is about to be deleted.
    fn forget_open_tabs(&mut self, path: &Path) {
        for g in &mut self.groups {
            if let Some(i) = g.files.iter().position(|f| f.path == path) {
                g.files.remove(i);
                g.active = g.active.min(g.files.len().saturating_sub(1));
            }
        }
        self.lsp.close_doc(path);
    }

    /// The inline AI-edit modal: instruction box → background request → reply replaces the
    /// selection through [`Self::apply_ai_replacement`]. Esc closes (dropping the channel
    /// orphans any straggler reply harmlessly).
    fn ai_edit_ui(&mut self, ctx: &egui::Context) {
        let Some(edit) = &mut self.ai_edit else { return };
        // Drain the worker first: a landed reply applies and closes.
        if let Ok(reply) = edit.rx.try_recv() {
            edit.in_flight = false;
            match reply {
                Some(text) => {
                    let origin = edit.origin.clone();
                    let code = ai::unfence(&text).trim_end().to_string();
                    self.ai_edit = None;
                    let now = ctx.input(|i| i.time);
                    if !code.is_empty() {
                        self.apply_ai_replacement(&origin, &code, now);
                    }
                    return;
                }
                None => edit.error = Some("request failed — backend unreachable or no reply".into()),
            }
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.ai_edit = None;
            return;
        }
        let mut fire = false;
        egui::Area::new("ai-edit".into())
            .anchor(egui::Align2::CENTER_TOP, [0.0, 120.0])
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .inner_margin(egui::Margin::same(style::sizes::OVERLAY_PAD))
                    .show(ui, |ui| {
                        ui.set_width(520.0);
                        style::panel_header_inline(ui, "AI edit selection");
                        let lines = edit.code.lines().count().max(1);
                        ui.colored_label(
                            colors::TEXT_FAINT(),
                            format!("{} line{} selected", lines, if lines == 1 { "" } else { "s" }),
                        );
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut edit.instruction)
                                .id(egui::Id::new("ai-edit-instruction"))
                                .hint_text("e.g. add error handling / convert to iterator / rename x to count")
                                .desired_width(f32::INFINITY),
                        );
                        if edit.focus_pending {
                            resp.request_focus();
                            edit.focus_pending = false;
                        }
                        if resp.lost_focus()
                            && ui.input(|i| i.key_pressed(egui::Key::Enter))
                            && !edit.in_flight
                            && !edit.instruction.trim().is_empty()
                        {
                            fire = true;
                            resp.request_focus();
                        }
                        if edit.in_flight {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.colored_label(colors::TEXT_FAINT(), "rewriting…");
                            });
                        }
                        if let Some(err) = &edit.error {
                            ui.colored_label(colors::ERROR(), err);
                        }
                    });
            });
        if fire {
            edit.in_flight = true;
            edit.error = None;
            let prompt = format!(
                "Rewrite the following {} code according to the instruction. Reply with ONLY \
                 the replacement code — no explanation, no markdown fences. Preserve the \
                 surrounding indentation style.\n\nInstruction: {}\n\nCode:\n{}",
                edit.lang,
                edit.instruction.trim(),
                edit.code
            );
            let tx = edit.tx.clone();
            let ctx2 = ctx.clone();
            let spawned = std::thread::Builder::new().name("cauldron-ai-edit".into()).spawn(move || {
                let reply = ai::ask(ai::OAUTH_SYSTEM, &prompt, "claude-sonnet-5", 2000, None);
                let _ = tx.send(reply);
                ctx2.request_repaint();
            });
            if spawned.is_err() {
                edit.in_flight = false;
                edit.error = Some("could not start the request thread".into());
            }
        }
    }

    /// Replace the AI request's originating selection with `code`, undo-safely — but ONLY if
    /// the buffer hasn't changed since the request was made (generation match). A changed
    /// buffer falls back to inserting at the caret: silently splicing into moved offsets
    /// could stomp code the user just wrote.
    fn apply_ai_replacement(&mut self, origin: &ai_actions::Origin, code: &str, now: f64) {
        let open = self
            .groups
            .iter_mut()
            .flat_map(|g| g.files.iter_mut())
            .find(|f| f.path == origin.path && f.loaded);
        let Some(f) = open else {
            self.lsp_message = Some("apply target is no longer open — nothing changed".into());
            return;
        };
        if f.buffer.generation != origin.generation {
            f.view.paste_for_menu(&mut f.buffer, code, now);
            f.dirty = true;
            self.lsp_message = Some("buffer changed since the request — inserted at caret instead".into());
            return;
        }
        let end = origin.range.end.min(f.buffer.rope().len_bytes());
        let start = origin.range.start.min(end);
        let tx = cauldron_editor::Transaction {
            changes: vec![cauldron_editor::buffer::Change { start, end, text: code.to_string() }],
        };
        f.view.apply_external(&mut f.buffer, &tx, now);
        f.dirty = true;
    }

    /// WebStorm-style live preview of the active HTML file: (re)start the static server rooted
    /// at the project root, then open the file's URL in the system browser. The page auto-
    /// reloads when any file changes (CSS/JS/HTML edits show up on save — auto-save makes that
    /// ~1s after you stop typing).
    fn web_preview_active_file(&mut self) {
        let Some(f) = self.groups[self.focused].active_file() else {
            self.lsp_message = Some("open an .html file to preview".into());
            return;
        };
        let path = f.path.clone();
        let is_html = matches!(path.extension().and_then(|e| e.to_str()), Some("html" | "htm"));
        if !is_html {
            self.lsp_message = Some("Web preview needs an .html file in the editor".into());
            return;
        }
        let root = self.workspace.root.clone();
        // Reuse the server when it already serves this root; otherwise start fresh.
        if self.web_server.as_ref().map(|s| s.root() != root).unwrap_or(true) {
            self.web_server = webpreview::WebServer::start(&root);
        }
        let Some(server) = &self.web_server else {
            self.lsp_message = Some("could not start the preview server".into());
            return;
        };
        let rel = path.strip_prefix(&root).unwrap_or(&path);
        let url = format!("{}/{}", server.base_url(), rel.to_string_lossy());
        let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
        self.lsp_message = Some(format!("live preview at {url} (auto-reloads on save)"));
    }

    /// Run the project's web dev server (package.json `dev` script, falling back to `start`) in
    /// the terminal and surface a hint to open the browser. Detects the package manager from
    /// the lockfile.
    fn run_web_dev_server(&mut self, ctx: &egui::Context) {
        let root = self.workspace.root.clone();
        let pkg = root.join("package.json");
        let Ok(text) = std::fs::read_to_string(&pkg) else {
            self.lsp_message = Some("no package.json in this project".into());
            return;
        };
        let script = if text.contains("\"dev\"") {
            "dev"
        } else if text.contains("\"start\"") {
            "start"
        } else {
            self.lsp_message = Some("package.json has no dev/start script".into());
            return;
        };
        // Package manager from the lockfile (pnpm/yarn/bun/npm).
        let pm = if root.join("pnpm-lock.yaml").exists() {
            "pnpm"
        } else if root.join("yarn.lock").exists() {
            "yarn"
        } else if root.join("bun.lockb").exists() {
            "bun"
        } else {
            "npm"
        };
        let cmd = if pm == "npm" { format!("npm run {script}") } else { format!("{pm} {script}") };
        self.terminal.run_command(&cmd, &root, ctx);
        self.lsp_message = Some(format!("running `{cmd}` — open the printed localhost URL in your browser"));
    }

    /// Apply the active theme live: flip both palettes (app chrome + editor) and rebuild egui's
    /// Visuals so the whole UI re-paints this frame.
    fn apply_theme(&mut self, ctx: &egui::Context) {
        let theme = systheme::resolve(self.theme_choice);
        style::set_theme(theme);
        cauldron_editor::theme::set_light(style::is_light_theme());
        livewall_uikit::theme::apply(ctx);
        style::apply_ide_style(ctx);
    }

    /// Set a file's ENTIRE content: loaded buffers get one whole-buffer transaction through
    /// the undo-safe editor path (same as WorkspaceEdit's open branch — Ctrl+Z reverts it,
    /// the per-frame drain sends didChange); anything else is written to disk. No-op when
    /// the content already matches.
    fn replace_file_text(&mut self, path: &Path, text: String, now: f64) {
        let open = self
            .groups
            .iter_mut()
            .flat_map(|g| g.files.iter_mut())
            .find(|f| f.path == path && f.loaded);
        match open {
            Some(f) => {
                let rope = f.buffer.rope();
                if rope.to_string() == text {
                    return;
                }
                let end = rope.len_bytes();
                let tx = cauldron_editor::Transaction {
                    changes: vec![cauldron_editor::buffer::Change { start: 0, end, text }],
                };
                f.view.apply_external(&mut f.buffer, &tx, now);
                f.dirty = true;
            }
            None => {
                let _ = std::fs::write(path, text);
            }
        }
    }

    /// Close every keyboard-owning overlay: pickers, prompts, and the completion popup. Called
    /// before ANY overlay opens so exactly one owns the keyboard at a time — stacked overlays each
    /// read Enter/arrows as raw input, so one Enter used to fire in ALL of them at once (e.g.
    /// quick-open choosing a file WHILE the rename prompt behind it fired a workspace-wide LSP
    /// rename with whatever its box contained).
    fn close_overlays(&mut self) {
        self.palette.close();
        self.quickopen.close();
        self.openfolder.close();
        self.goto_symbol.close();
        self.search.close();
        self.rename = None;
        self.goto_line = None;
        self.prompt = None;
        self.prompt_focus_pending = false;
        self.bookmarks_open = false;
        self.file_symbols_open = false;
        self.file_symbols_query.clear();
        self.completion = None;
        self.recent_locations = None;
        self.def_choices = None;
    }

    /// The picker/overlay open helpers below all route through [`Self::close_overlays`] so the
    /// one being opened is the only keyboard owner.
    fn open_quickopen_files(&mut self) {
        self.close_overlays();
        self.quickopen.open(self.workspace.all_files(), &self.workspace.root);
    }

    fn open_project_picker(&mut self) {
        self.close_overlays();
        self.openfolder.open();
    }

    fn open_file_picker(&mut self) {
        self.close_overlays();
        let root = self.workspace.root.clone();
        self.openfolder.open_file_mode(&root);
    }

    fn open_new_project_picker(&mut self) {
        self.close_overlays();
        self.openfolder.open_new_project();
    }

    fn open_search(&mut self, seed: &str) {
        self.close_overlays();
        self.search.open(seed);
    }

    fn open_palette(&mut self) {
        self.close_overlays();
        self.palette.open();
    }

    fn open_goto_symbol(&mut self) {
        self.close_overlays();
        self.goto_symbol.open();
    }

    /// Show the side-by-side diff for `abs` in `mode` as the central-panel takeover. Dirty
    /// buffers are flushed first so the diff (and any hunk staged/reverted from it) reflects
    /// exactly what the editor shows. Outside-the-repo paths just open in the editor.
    fn open_diff_mode(&mut self, abs: &Path, mode: diffview::DiffMode) {
        self.flush_dirty_buffers();
        boot_trace::count("git-subprocess", 1);
        match diffview::open_mode(&self.workspace.root, abs, mode) {
            Some(v) => self.diff_view = Some(v),
            None => self.open_file(abs.to_path_buf()),
        }
    }

    /// Palette "Show Diff": prefer whichever comparison has content — unstaged edits first,
    /// then staged, else the (possibly empty) HEAD overview.
    fn open_diff(&mut self, abs: &Path) {
        self.flush_dirty_buffers();
        boot_trace::count("git-subprocess", 1);
        let root = self.workspace.root.clone();
        for mode in [
            diffview::DiffMode::Unstaged,
            diffview::DiffMode::Staged,
            diffview::DiffMode::Head,
        ] {
            if let Some(v) = diffview::open_mode(&root, abs, mode) {
                let empty = v.is_empty();
                self.diff_view = Some(v);
                if !empty || mode == diffview::DiffMode::Head {
                    return;
                }
            } else {
                self.open_file(abs.to_path_buf());
                return;
            }
        }
    }

    /// Open the name prompt (new file / new folder / rename path) as the ONLY overlay, grabbing
    /// focus on its first frame.
    fn open_prompt(&mut self, purpose: NamePrompt, seed: String) {
        self.close_overlays();
        self.prompt = Some((purpose, seed));
        self.prompt_focus_pending = true;
    }

    /// Shift+F11: open the bookmarks list as the only overlay.
    fn open_bookmarks(&mut self) {
        self.close_overlays();
        self.bookmarks_open = true;
        self.bookmarks_sel = 0;
        self.rebuild_bookmark_rows();
    }

    /// Snapshot the overlay's rows (path, line, preview) — previews come from open buffers or
    /// ONE disk read here, never per frame.
    fn rebuild_bookmark_rows(&mut self) {
        let mut rows: Vec<(PathBuf, u32, String)> = Vec::new();
        let mut sorted: Vec<(&PathBuf, &Vec<u32>)> = self.bookmarks.iter().collect();
        sorted.sort();
        for (path, lines) in sorted {
            let from_buffer = self
                .groups
                .iter()
                .flat_map(|g| g.files.iter())
                .find(|f| f.path == **path && f.loaded)
                .map(|f| f.buffer.rope().to_string());
            let text = from_buffer.or_else(|| std::fs::read_to_string(path).ok());
            for l in lines {
                let preview = text
                    .as_deref()
                    .and_then(|t| t.lines().nth(l.saturating_sub(1) as usize))
                    .unwrap_or("")
                    .trim()
                    .chars()
                    .take(80)
                    .collect();
                rows.push((path.clone(), *l, preview));
            }
        }
        self.bookmark_rows = rows;
    }

    /// Ctrl+F12: open the file-structure (go-to-symbol-in-file) popup as the only overlay.
    fn open_file_symbols(&mut self) {
        self.close_overlays();
        self.file_symbols_open = true;
        self.file_symbols_query.clear();
        self.file_symbols_sel = 0;
        self.prompt_focus_pending = true;
    }

    /// Ctrl+G: open the go-to-line prompt as the only overlay.
    fn open_goto_line(&mut self) {
        self.close_overlays();
        self.goto_line = Some(String::new());
        self.prompt_focus_pending = true;
    }

    /// Shift+F6: open the rename prompt seeded with the identifier under the caret.
    fn start_rename(&mut self) {
        if let Some(f) = self.groups[self.focused].active_file() {
            let (path, gen, byte) = (f.path.clone(), f.buffer.generation, f.view.caret_byte());
            let rope = f.buffer.rope().clone();
            let seed = ident_at(&rope, byte);
            self.close_overlays();
            self.rename = Some((seed, path, byte, gen));
            self.prompt_focus_pending = true;
        }
    }

    /// Alt+F7: find all references to the symbol under the caret. LSP is primary; when the
    /// file's language has NO live server (never registered, spawn failed, crashed out), fall
    /// back to the native PSI index instead — one path answers per invocation, never both.
    fn find_usages(&mut self) {
        let Some(f) = self.groups[self.focused].active_file() else { return };
        let (path, gen, byte) = (f.path.clone(), f.buffer.generation, f.view.caret_byte());
        let rope = f.buffer.rope().clone();
        if self.lsp.has_live_server(&path) {
            self.usages_gen = Some(gen);
            self.lsp.request_references(&path, &rope, byte, gen);
            self.lsp_message = Some("finding usages…".into());
            return;
        }
        let name = ident_at(&rope, byte);
        if name.is_empty() {
            self.lsp_message = Some("no symbol under the caret".into());
            return;
        }
        self.find_usages_from_index(&name);
    }

    /// Call hierarchy — who calls the symbol under the caret. LSP-only (needs a live server);
    /// results land in the Usages panel labeled as callers.
    fn call_hierarchy(&mut self) {
        let Some(f) = self.groups[self.focused].active_file() else { return };
        let (path, gen, byte) = (f.path.clone(), f.buffer.generation, f.view.caret_byte());
        if !self.lsp.has_live_server(&path) {
            self.lsp_message = Some("call hierarchy needs a language server for this file".into());
            return;
        }
        let rope = f.buffer.rope().clone();
        self.call_hierarchy_gen = Some(gen);
        self.lsp.request_call_hierarchy(&path, &rope, byte, gen);
        self.lsp_message = Some("finding callers…".into());
    }

    /// The native find-usages fallback: query the retained PSI index snapshot by name and fill
    /// the same Usages panel, labeled as index results. Cancels any pending LSP generation so a
    /// late server reply from an older invocation can't overwrite the index answer.
    fn find_usages_from_index(&mut self, name: &str) {
        let Some(index) = self.psi.index() else {
            self.lsp_message =
                Some("no language server for this file, and no PSI index to fall back on".into());
            return;
        };
        // The retained index holds BUFFER-coordinate overlay facts for dirty files (item 7):
        // hand the snapshot those live texts so lines/context resolve in the same coordinate
        // space instead of against the shorter disk text.
        let overlay: HashMap<PathBuf, String> = self
            .groups
            .iter()
            .flat_map(|g| g.files.iter())
            .filter(|f| f.dirty)
            .map(|f| (f.path.clone(), f.buffer.rope().to_string()))
            .collect();
        let snap = cauldron_psi::query::PsiSnapshot::with_overlay(index, overlay);
        let hits = snap.find_usages(name);
        self.usages_gen = None;
        self.lsp_message = None;
        self.usages.clear();
        self.usages_from_index = true;
        self.usages_are_callers = false;
        for u in hits {
            self.usages.push((u.path, u.line, u.context.chars().take(140).collect()));
        }
        if self.usages.is_empty() {
            self.lsp_message = Some(format!("no usages of '{name}' in the PSI index"));
        } else {
            self.bottom_open = true;
            self.bottom_tab = BottomTab::Usages;
        }
    }

    fn request_completion_at_caret(&mut self) {
        if let Some(f) = self.groups[self.focused].active_file() {
            let (path, gen, byte) = (f.path.clone(), f.buffer.generation, f.view.caret_byte());
            let rope = f.buffer.rope().clone();
            self.completion_gen = Some(gen);
            self.lsp.request_completion(&path, &rope, byte, gen);
        }
    }

    /// Keep the focused editor's inline-blame annotation current: pump the background service,
    /// request blame for the visible file when missing, and pin the annotation to the caret
    /// line. Only CLEAN buffers annotate — a dirty buffer's lines have shifted relative to the
    /// disk content git blamed, and wrong attribution is worse than none.
    fn drive_inline_blame(&mut self, ctx: &egui::Context) {
        self.blame.pump();
        // Only the FOCUSED pane's caret line annotates; every other open view must be cleared or
        // a split pane keeps painting its last note forever.
        let focused = self.focused;
        for (gi, g) in self.groups.iter_mut().enumerate() {
            for (fi, f) in g.files.iter_mut().enumerate() {
                if !(gi == focused && fi == g.active) {
                    f.view.set_inline_blame(None);
                }
            }
        }
        let root = self.workspace.root.clone();
        let Some(f) = self.groups[self.focused].active_file() else { return };
        if !self.inline_blame_enabled || !f.loaded || f.dirty || self.no_project {
            f.view.set_inline_blame(None);
            return;
        }
        let path = f.path.clone();
        let line = f.buffer.rope().byte_to_line(f.view.caret_byte());
        let note = match self.blame.cache.get(&path) {
            Some(blame::FileBlame::Ready(lines)) => lines.get(line).map(|b| {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                (line, blame::annotation(b, now))
            }),
            // In flight (or just invalidated): the previous note is stale against the new
            // content — show nothing rather than a wrong attribution.
            Some(blame::FileBlame::Pending(_)) => None,
            None => {
                self.blame.request(&root, &path, ctx);
                None
            }
        };
        if let Some(f) = self.groups[self.focused].active_file() {
            if f.view.set_inline_blame(note) {
                // This runs AFTER the editor painted; a changed note needs one more frame.
                ctx.request_repaint();
            }
        }
    }

    /// Draw + drive the completion popup: prefix filter, ↑/↓/Enter/Tab/Esc, click to accept.
    fn drive_completion_popup(&mut self, ctx: &egui::Context) {
        // The popup only lives while the EDITOR owns the keyboard. Anything else focused — the
        // command palette, pickers, the terminal, the find bar, git boxes, rename/goto-line
        // prompts, dbg-eval — means Tab/arrows are aimed THERE, and reading them here would
        // silently accept/steer a completion into the buffer behind the user's back (e.g. shell
        // tab-completion in the terminal inserting an LSP item into the source file).
        let editor_focused = self.groups[self.focused]
            .active_file()
            .map(|f| f.view.has_focus())
            .unwrap_or(false);
        if !editor_focused
            || self.palette.is_open()
            || self.quickopen.is_open()
            || self.openfolder.is_open()
        {
            self.completion = None;
            if let Some(f) = self.groups[self.focused].active_file() {
                f.view.suppress_nav_keys = false;
                f.view.suppress_enter = false;
            }
            return;
        }
        // The editor must not consume arrows/Tab/Esc while the popup is up. Enter is handled below
        // (suppress_enter) — only stolen once the user has navigated the list.
        let open = self.completion.is_some() || self.def_choices.is_some();
        // The recent-locations popup ALSO owns Enter (it jumps on Enter), unlike the
        // completion/def popups where Enter stays a newline — so it suppresses both.
        let recent_open = self.recent_locations.is_some();
        if let Some(f) = self.groups[self.focused].active_file() {
            f.view.suppress_nav_keys = open || recent_open;
            if recent_open {
                f.view.suppress_enter = true;
            } else if !open {
                f.view.suppress_enter = false; // no popup → Enter is always a newline
            }
        }
        let Some(mut c) = self.completion.take() else { return };
        // Live prefix from anchor..caret; popup dies if the caret left the word.
        let (prefix, alive) = {
            let g = &mut self.groups[self.focused];
            match g.files.get_mut(g.active) {
                Some(f) if f.path == c.path => {
                    let caret = f.view.caret_byte();
                    let rope = f.buffer.rope();
                    if caret < c.anchor || caret > rope.len_bytes() {
                        (String::new(), false)
                    } else {
                        (rope.byte_slice(c.anchor..caret).to_string(), true)
                    }
                }
                _ => (String::new(), false),
            }
        };
        if !alive || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            return; // dropped
        }
        let filtered: Vec<usize> = rank_completions(&c.items, &prefix);
        if filtered.is_empty() {
            return; // nothing matches the prefix any more
        }
        if ctx.input(|i| i.key_pressed(egui::Key::ArrowDown)) {
            c.selected = (c.selected + 1) % filtered.len();
            c.navigated = true;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
            c.selected = (c.selected + filtered.len() - 1) % filtered.len();
            c.navigated = true;
        }
        c.selected = c.selected.min(filtered.len() - 1);
        // Tell the editor to steal Enter ONLY now that the list is navigated — next frame's bare
        // Enter otherwise makes a newline. (Set every frame the popup lives so it tracks navigation.)
        if let Some(f) = self.groups[self.focused].active_file() {
            f.view.suppress_enter = c.navigated;
        }
        // Tab (or a click) accepts; Enter is ALWAYS a newline, never an accept. Accepting on Enter
        // meant racing the editor (which processes Enter a step earlier in the frame) via a
        // cross-frame suppress_enter flag — it ate newlines and, on a stale LSP range, dropped the
        // caret on a random line. Tab-to-accept is unambiguous and race-free.
        let accept = ctx.input(|i| i.key_pressed(egui::Key::Tab));
        let mut clicked: Option<usize> = None;

        egui::Area::new("completion".into())
            .fixed_pos(c.pos + egui::vec2(0.0, 4.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_min_width(320.0);
                    ui.set_max_width(560.0);
                    egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                        for (row, &idx) in filtered.iter().enumerate() {
                            let it = &c.items[idx];
                            let sel = row == c.selected;
                            let kind = completion_kind_glyph(it.kind);
                            let mut job = egui::text::LayoutJob::default();
                            let font = egui::TextStyle::Monospace.resolve(ui.style());
                            job.append(
                                &format!("{kind} {}", it.label),
                                0.0,
                                egui::TextFormat {
                                    font_id: font.clone(),
                                    color: if sel { colors::ACCENT_HI() } else { colors::TEXT() },
                                    ..Default::default()
                                },
                            );
                            if let Some(d) = &it.detail {
                                job.append(
                                    &format!("  {}", d.chars().take(48).collect::<String>()),
                                    0.0,
                                    egui::TextFormat {
                                        font_id: font,
                                        color: colors::TEXT_FAINT(),
                                        ..Default::default()
                                    },
                                );
                            }
                            if ui.selectable_label(sel, job).clicked_by(egui::PointerButton::Primary) {
                                clicked = Some(row);
                            }
                        }
                    });
                });
            });

        let chosen = if accept { Some(c.selected) } else { clicked };
        if let Some(row) = chosen {
            let idx = filtered[row];
            let item = c.items[idx].clone();
            self.accept_completion(&c, &item, ctx.input(|i| i.time));
        } else {
            self.completion = Some(c);
        }
    }

    fn accept_completion(&mut self, c: &CompletionUi, item: &lsp_types::CompletionItem, now: f64) {
        let enc = self.lsp_encoding(&c.path);
        let g = &mut self.groups[self.focused];
        let Some(f) = g.files.get_mut(g.active).filter(|f| f.path == c.path) else { return };
        let rope = f.buffer.rope().clone();
        let caret = f.view.caret_byte();
        // Replace range: the item's textEdit when present, else anchor..caret (typed prefix).
        let (range, text) = match &item.text_edit {
            Some(lsp_types::CompletionTextEdit::Edit(te)) => {
                let s = pos_to_byte(&rope, &te.range.start, enc);
                let e = pos_to_byte(&rope, &te.range.end, enc).max(s).max(caret);
                (s..e, te.new_text.clone())
            }
            Some(lsp_types::CompletionTextEdit::InsertAndReplace(te)) => {
                let s = pos_to_byte(&rope, &te.replace.start, enc);
                let e = pos_to_byte(&rope, &te.replace.end, enc).max(s).max(caret);
                (s..e, te.new_text.clone())
            }
            None => (
                c.anchor..caret,
                item.insert_text.clone().unwrap_or_else(|| item.label.clone()),
            ),
        };
        if item.insert_text_format == Some(lsp_types::InsertTextFormat::SNIPPET) {
            // Full expansion: placeholders become a Tab-traversal session in the view.
            f.view.insert_snippet(&mut f.buffer, range, &text, now);
        } else {
            // Plain items pass through the legacy stripper as belt-and-braces (some servers
            // mislabel and ship a stray `$0` in PlainText items).
            let text = strip_snippet_markers(&text);
            f.view.replace_range(&mut f.buffer, range, &text, now);
        }
        f.dirty = true;
        // AUTO-IMPORTS: additionalTextEdits present → apply now (they sit above the completion
        // point, so post-accept application is safe — the standard client behavior). Absent →
        // resolve round-trip: r-a / ts-server / pyright only compute imports lazily.
        let gen_after = f.buffer.generation;
        match &item.additional_text_edits {
            Some(extra) if !extra.is_empty() => {
                let path = c.path.clone();
                self.apply_additional_edits(&path, extra, now);
            }
            _ => {
                self.resolve_pending = Some((c.path.clone(), gen_after));
                self.lsp.resolve_completion(&c.path, item, gen_after);
            }
        }
    }

    /// Apply a resolve's additionalTextEdits (auto-import lines) to an open file, undo-safe.
    fn apply_additional_edits(
        &mut self,
        path: &Path,
        edits: &[lsp_types::TextEdit],
        now: f64,
    ) {
        let enc = self.lsp_encoding(path);
        let Some(f) = self
            .groups
            .iter_mut()
            .flat_map(|g| g.files.iter_mut())
            .find(|f| f.path == path && f.loaded)
        else {
            return;
        };
        let rope = f.buffer.rope().clone();
        let mut changes: Vec<cauldron_editor::buffer::Change> = edits
            .iter()
            .map(|te| {
                let s = pos_to_byte(&rope, &te.range.start, enc);
                let e = pos_to_byte(&rope, &te.range.end, enc).max(s);
                cauldron_editor::buffer::Change { start: s, end: e, text: te.new_text.clone() }
            })
            .collect();
        changes.sort_by_key(|c| c.start);
        let tx = cauldron_editor::Transaction { changes };
        f.view.apply_external(&mut f.buffer, &tx, now);
        f.dirty = true;
    }

    fn handle_tree_action(&mut self, action: TreeAction) {
        match action {
            TreeAction::Open(abs) => self.open_file(abs),
            TreeAction::NewFile(dir) => {
                self.open_prompt(NamePrompt::NewFile { dir_rel: dir }, String::new());
            }
            TreeAction::NewFolder(dir) => {
                self.open_prompt(NamePrompt::NewFolder { dir_rel: dir }, String::new());
            }
            TreeAction::Rename(rel) => {
                let seed =
                    rel.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                self.open_prompt(NamePrompt::Rename { rel }, seed);
            }
            TreeAction::Delete(rel) => {
                let abs = self.workspace.root.join(&rel);
                let result = if abs.is_dir() {
                    std::fs::remove_dir_all(&abs)
                } else {
                    std::fs::remove_file(&abs)
                };
                match result {
                    Ok(()) => {
                        for gi in 0..self.groups.len() {
                            while let Some(i) = self
                                .groups
                                .get(gi)
                                .and_then(|g| g.files.iter().position(|f| f.path.starts_with(&abs)))
                            {
                                self.close_tab(gi, i);
                            }
                        }
                        self.ws_refresh_pending = true;
                    }
                    Err(e) => self.error = Some(format!("delete failed: {e}")),
                }
            }
        }
    }

    /// The inline name prompt (new file / new folder / rename).
    fn prompt_ui(&mut self, ctx: &egui::Context) {
        let Some((purpose, mut buf)) = self.prompt.take() else { return };
        // One-shot: grab keyboard focus only on the frame the prompt opened, so clicking back
        // into the editor is respected instead of the overlay re-stealing focus every frame.
        let grab_focus = std::mem::take(&mut self.prompt_focus_pending);
        let mut keep = true;
        let mut submit = false;
        let title = match &purpose {
            NamePrompt::NewFile { dir_rel } => format!("New file in {}/", show_rel(dir_rel)),
            NamePrompt::NewFolder { dir_rel } => format!("New folder in {}/", show_rel(dir_rel)),
            NamePrompt::Rename { rel } => format!("Rename {}", rel.display()),
        };
        egui::Area::new("nameprompt".into())
            .anchor(egui::Align2::CENTER_TOP, [0.0, 120.0])
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .inner_margin(egui::Margin::same(style::sizes::OVERLAY_PAD))
                    .show(ui, |ui| {
                        ui.set_width(420.0);
                        style::panel_header_inline(ui, &title);
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut buf)
                                .hint_text("name")
                                .desired_width(f32::INFINITY)
                                .font(egui::TextStyle::Monospace),
                        );
                        if grab_focus {
                            resp.request_focus();
                        }
                        if ui.input(|i| i.key_pressed(egui::Key::Enter)) && !buf.trim().is_empty() {
                            submit = true;
                            keep = false;
                        }
                        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                            keep = false;
                        }
                    });
            });
        if submit {
            let name = buf.trim().to_string();
            let root = self.workspace.root.clone();
            match &purpose {
                NamePrompt::NewFile { dir_rel } => {
                    let abs = root.join(dir_rel).join(&name);
                    if let Some(parent) = abs.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    // create_new: never TRUNCATE an existing file to zero bytes just because its
                    // name was typed into the New File prompt (atomic, unlike an exists() probe).
                    match std::fs::OpenOptions::new().write(true).create_new(true).open(&abs) {
                        Ok(_) => {
                            self.ws_refresh_pending = true;
                            self.open_file(abs);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                            self.error = Some(format!("new file failed: {name} already exists"));
                            self.open_file(abs);
                        }
                        Err(e) => self.error = Some(format!("new file failed: {e}")),
                    }
                }
                NamePrompt::NewFolder { dir_rel } => {
                    let abs = root.join(dir_rel).join(&name);
                    match std::fs::create_dir_all(&abs) {
                        Ok(()) => self.ws_refresh_pending = true,
                        Err(e) => self.error = Some(format!("new folder failed: {e}")),
                    }
                }
                NamePrompt::Rename { rel } => {
                    let from = root.join(rel);
                    let to = from.with_file_name(&name);
                    // rename(2) silently REPLACES an existing target — refuse instead.
                    // (symlink_metadata so even a dangling symlink counts as occupied.)
                    if to != from && to.symlink_metadata().is_ok() {
                        self.error = Some(format!("rename failed: {name} already exists"));
                        return;
                    }
                    // An open dirty buffer is flushed first: the close+reopen below would
                    // otherwise silently discard its unsaved edits.
                    if !self.save_dirty(Some(&[from.clone()])) {
                        return; // save_dirty already surfaced the error; nothing was renamed
                    }
                    match std::fs::rename(&from, &to) {
                        Ok(()) => {
                            for gi in 0..self.groups.len() {
                                if let Some(i) =
                                    self.groups[gi].files.iter().position(|f| f.path == from)
                                {
                                    self.close_tab(gi, i);
                                    self.open_file(to.clone());
                                }
                            }
                            self.ws_refresh_pending = true;
                        }
                        Err(e) => self.error = Some(format!("rename failed: {e}")),
                    }
                }
            }
        } else if keep {
            self.prompt = Some((purpose, buf));
        }
    }

    /// The unsaved-changes modal: Save & Close / Discard / Cancel for a parked tab close or a
    /// vetoed window close. Enter takes the safe default (save), Esc cancels. The editor's keys
    /// are stolen while this is up (see the `modal_keys_stolen` OR in bookmarks_overlay_ui).
    fn close_confirm_ui(&mut self, ctx: &egui::Context) {
        if !self.exit_confirm {
            return;
        }
        // The files whose save FAILED — auto-save on close couldn't write them (read-only,
        // full disk). Names shown so "Close anyway" is an informed click.
        let dirty_names: Vec<String> = self
            .groups
            .iter()
            .flat_map(|grp| grp.files.iter())
            .filter(|f| f.dirty && f.loaded)
            .map(|f| f.path.file_name().unwrap_or_default().to_string_lossy().into_owned())
            .collect();
        // A retry elsewhere cleared them: nothing left to warn about — let the close proceed.
        if dirty_names.is_empty() {
            self.exit_confirm = false;
            self.exit_approved = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        let mut choice: Option<bool> = None; // Some(true) = retry save, Some(false) = discard
        let mut keep = true;
        egui::Area::new("closeconfirm".into())
            .anchor(egui::Align2::CENTER_TOP, [0.0, 120.0])
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .inner_margin(egui::Margin::same(style::sizes::OVERLAY_PAD))
                    .show(ui, |ui| {
                        ui.set_width(420.0);
                        style::panel_header_inline(ui, "Could not save on exit");
                        let shown = dirty_names.iter().take(6).cloned().collect::<Vec<_>>();
                        let mut list = shown.join(", ");
                        if dirty_names.len() > shown.len() {
                            list.push_str(&format!(" (+{} more)", dirty_names.len() - shown.len()));
                        }
                        ui.label(
                            egui::RichText::new(list).monospace().color(style::colors::TEXT_FAINT()),
                        );
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if ui
                                .button("Retry Save & Close    Enter")
                                .clicked_by(egui::PointerButton::Primary)
                                || ui.input(|i| i.key_pressed(egui::Key::Enter))
                            {
                                choice = Some(true);
                                keep = false;
                            }
                            if ui
                                .button("Close Without Saving")
                                .clicked_by(egui::PointerButton::Primary)
                            {
                                choice = Some(false);
                                keep = false;
                            }
                            if ui.button("Cancel    Esc").clicked_by(egui::PointerButton::Primary)
                                || ui.input(|i| i.key_pressed(egui::Key::Escape))
                            {
                                keep = false;
                            }
                        });
                    });
            });
        match choice {
            // Retry: a still-failing save leaves the window OPEN (self.error explains).
            Some(save_first) => {
                if !save_first || self.flush_dirty_buffers() {
                    self.exit_confirm = false;
                    self.exit_approved = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
            None if keep => {} // still showing
            None => self.exit_confirm = false, // cancelled
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Frame 1 entry/exit + frame 2 entry (~first pixel on Wayland) + boot summary; the
        // guard's Drop catches every return path. No-op without CAULDRON_BOOT_TRACE=1.
        let _boot_frame = boot_trace::frame_guard();
        // PoT Rule-5 dogfood: the two indices everything below trusts (`groups[self.focused]`,
        // `files[g.active]`) are asserted-and-clamped once per frame. Every historical
        // index-panic crash in this file was one of these going stale after a close/collapse
        // path missed a decrement — the debug assert catches the NEXT such bug in dev, the
        // clamp keeps a release build alive instead of aborting the whole IDE.
        debug_assert!(self.focused < self.groups.len(), "stale focused group index");
        if self.focused >= self.groups.len() {
            self.focused = self.groups.len().saturating_sub(1);
        }
        for g in &mut self.groups {
            debug_assert!(g.files.is_empty() || g.active < g.files.len(), "stale active tab index");
            if g.active >= g.files.len() {
                g.active = g.files.len().saturating_sub(1);
            }
        }
        // Window close: SAVE everything and let it close (auto-save policy — no prompt).
        // Only when a write FAILS does the close get vetoed and the modal offer the choice
        // (retry save / close anyway / cancel). on_exit never prompts — by the time it runs
        // the window is already gone.
        if ctx.input(|i| i.viewport().close_requested()) && !self.exit_approved {
            if self.flush_dirty_buffers() {
                self.exit_approved = true;
            } else {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.exit_confirm = true;
            }
        }
        // Lazy session tabs (item 4): whatever became ACTIVE since last frame hydrates before
        // anything reads buffers this frame. For loaded tabs this is a bool check per group.
        let mut gi = 0;
        while gi < self.groups.len() {
            self.ensure_active_loaded(gi);
            gi += 1;
        }
        // --- pumps first: this frame paints fresh state ------------------------------------------
        // Every PTY drains every frame, visible or not — see TerminalPane::pump_all.
        self.terminal.pump_all(ctx);
        let (events, wake) = self.lsp.pump();
        if let Some(d) = wake {
            ctx.request_repaint_after(d);
        }
        let mut squiggle_paths: Vec<PathBuf> = Vec::new();
        for (_, ev) in events {
            if let Some(p) = self.handle_lsp_event(ev) {
                squiggle_paths.push(p);
            }
        }
        let psi_was_indexing = matches!(self.psi.state, PsiState::Indexing);
        self.psi.pump();
        if psi_was_indexing && matches!(self.psi.state, PsiState::Ready { .. }) {
            self.refresh_nasa_squiggles();
        }
        // AUTOSAVE the session: on_exit does not run when the process is killed (SIGTERM /
        // compositor force-close), so periodic saving is what makes "reopen where you left off"
        // actually reliable.
        if self.last_session_save.elapsed().as_secs() >= 20 {
            self.last_session_save = std::time::Instant::now();
            self.save_session();
            self.save_settings();
        }
        // Every view is born at EditorView::new's own 14.0 default, and the font was only ever
        // pushed at the zoom/slider sites — so a file opened AFTER a zoom (or restored at boot, or
        // hydrated from a lazy tab) rendered at 14 regardless of the real size. One sweep per
        // frame keeps every view at the current font, whatever created it and whenever.
        for g in &mut self.groups {
            for f in &mut g.files {
                if f.view.font_size() != self.editor_font {
                    f.view.set_font_size(self.editor_font);
                }
            }
        }
        if self.restore_terminal_pending {
            self.restore_terminal_pending = false;
            if !self.terminal.open {
                let root = self.terminal_root();
                self.terminal.toggle(ctx, &root);
            }
        }
        // --- file watcher + async workspace refresh -----------------------------------------------
        // Swap in any finished background re-walk BEFORE consumers read all_files this frame.
        if self.workspace.poll_refresh() {
            // A NEW universe just landed — the moment deferred index work becomes correct.
            if self.rescan_after_refresh {
                // The watcher's FullRescan plan waited for this swap: rebuilding earlier would
                // have consumed the pre-event universe (checkout's new files missing) with
                // nothing to re-kick, freezing PSI/symbols on stale contents indefinitely.
                self.rescan_after_refresh = false;
                self.psi_scan_pending = true;
                self.symbols_rebuild_pending = true;
            }
            if !self.fs_pending_universe.is_empty() {
                // Retry paths that raced the async re-walk (just-created files): members now
                // rejoin the incremental lanes; still-absent paths are out-of-universe for
                // real (gitignored / outside the root) and are dropped here.
                let parked = std::mem::take(&mut self.fs_pending_universe);
                let members: Vec<PathBuf> =
                    parked.into_iter().filter(|p| self.workspace.contains(p)).collect();
                if !members.is_empty() {
                    self.route_incremental(members, ctx);
                }
            }
        }
        if self.watcher.is_none() && !self.no_project {
            // Lazily (re)created here: needs a ctx, and open_folder drops it on project switch.
            self.watcher = Some(watcher::FsWatcher::start(
                &self.workspace.root,
                self.workspace.excludes(),
                ctx,
            ));
        }
        if let Some(plan) = self.watcher.as_mut().and_then(|w| w.poll()) {
            // Something changed on disk (external edit, git checkout, codegen, our own save):
            // the tree always re-walks; the plan picks the index strategy. Any fs change also
            // arms an open-buffer reload check (a no-op for our own saves — buffer==disk).
            self.ws_refresh_pending = true;
            self.watcher_fired = true;
            match plan {
                watcher::Plan::FullRescan => {
                    // DEFERRED to the poll_refresh swap above: the rebuilds must consume the
                    // post-event universe this same plan is about to produce.
                    self.rescan_after_refresh = true;
                }
                watcher::Plan::Incremental(files) => {
                    // Only canonical-universe members may enter the incremental lanes (the
                    // watcher's root-gitignore filter is looser than the walk's — nested
                    // .gitignores are not consulted there). Non-members park until the swap.
                    let (members, parked): (Vec<PathBuf>, Vec<PathBuf>) =
                        files.into_iter().partition(|f| self.workspace.contains(f));
                    for p in parked {
                        if !self.fs_pending_universe.contains(&p) {
                            self.fs_pending_universe.push(p);
                        }
                    }
                    if !members.is_empty() {
                        self.route_incremental(members, ctx);
                    }
                }
            }
        }
        if self.watcher_fired {
            self.watcher_fired = false;
            self.reload_externally_changed_buffers(ctx);
        }
        if self.ws_refresh_pending {
            self.ws_refresh_pending = false;
            self.workspace.refresh_async(ctx);
        }
        if self.psi_scan_pending {
            self.psi_scan_pending = false;
            // The full rescan supersedes any queued single-file updates.
            self.psi_saved_files.clear();
            let root = self.workspace.root.clone();
            // ONE file universe: PSI consumes the workspace walk, never re-walks itself.
            let files = self.workspace.all_files().to_vec();
            let has_c = self.workspace.has_c_sources() && self.standards != Standards::Off;
            self.psi.rescan(&root, &files, has_c, ctx);
        } else if !self.psi_saved_files.is_empty() {
            // Saved C files take the incremental lane (single-file replace_file_facts on the
            // resident indexer); if the service has no index to patch, fall back to a full scan
            // on the next frame.
            let root = self.workspace.root.clone();
            let excludes = self.workspace.excludes().to_vec();
            for path in std::mem::take(&mut self.psi_saved_files) {
                if !self.workspace.contains(&path) {
                    // Not in the canonical universe: a just-created file racing the async
                    // re-walk (retried when the swap lands) or genuinely out-of-universe
                    // (gitignored — the index must never admit what the full scan excludes).
                    if !self.fs_pending_universe.contains(&path) {
                        self.fs_pending_universe.push(path);
                    }
                    continue;
                }
                // Dirty open buffer = the disk change is EXTERNAL to the editor's view — the
                // worker keeps the live overlay authoritative instead of clobbering it.
                let external = self.buffer_is_dirty(&path);
                if !self.psi.file_saved(&root, &path, &excludes, external, ctx) {
                    self.psi_scan_pending = true;
                    break;
                }
            }
        }
        // --- dirty-buffer PSI overlay (item 7) -----------------------------------------------------
        // Closes first: a queued close cancels its pending overlay tick above, and the worker
        // (one serialized queue) processes it after any full scan queued this frame — order-safe.
        if !self.psi_closed_files.is_empty() {
            let root = self.workspace.root.clone();
            let excludes = self.workspace.excludes().to_vec();
            for path in std::mem::take(&mut self.psi_closed_files) {
                self.psi_overlay_pending.remove(&path);
                self.psi.buffer_closed(&root, &path, &excludes, ctx);
            }
        }
        // Buffers quiet past the debounce ship their LIVE text to the worker (facts are
        // re-collected THERE, never on this thread); squiggles then update without saving.
        if !self.psi_overlay_pending.is_empty() {
            let ready: Vec<PathBuf> = self
                .psi_overlay_pending
                .iter()
                .filter(|(_, t)| t.elapsed() >= PSI_OVERLAY_DEBOUNCE)
                .map(|(p, _)| p.clone())
                .collect();
            if !ready.is_empty() {
                let root = self.workspace.root.clone();
                let excludes = self.workspace.excludes().to_vec();
                for path in ready {
                    self.psi_overlay_pending.remove(&path);
                    if !self.workspace.contains(&path) {
                        // Out-of-universe (gitignored) or racing the async re-walk: the full
                        // scan would drop this file, so its overlay must not enter the index
                        // either. The next keystroke re-arms the debounce — by then a racing
                        // just-created file's re-walk has landed.
                        continue;
                    }
                    // The buffer may have closed since the tick was armed — nothing to ship
                    // (close_tab already queued the overlay drop).
                    let Some(text) = self.buffer_text(&path) else { continue };
                    if !self.psi.buffer_edited(&root, &path, &excludes, text, ctx) {
                        // No index to shadow yet: degrade to the full-rescan lane.
                        self.psi_scan_pending = true;
                    }
                }
            }
            // Debounce expiry must produce a frame even when the user stops typing entirely.
            if let Some(rest) = self
                .psi_overlay_pending
                .values()
                .map(|t| PSI_OVERLAY_DEBOUNCE.saturating_sub(t.elapsed()))
                .min()
            {
                ctx.request_repaint_after(rest + std::time::Duration::from_millis(10));
            }
        }

        *POINTER_POS.lock().unwrap() =
            ctx.input(|i| i.pointer.hover_pos()).map(|p| (p.x, p.y));

        // --- chrome ---------------------------------------------------------------------------------
        livewall_uikit::chrome::title_bar(ctx, &self.title());
        self.menu_bar(ctx);

        // --- global keys ------------------------------------------------------------------------------
        // While the SHELL has the keyboard, none of these chords fire: readline owns
        // Ctrl+W/P/N/E/O/S/G/J/B (delete-word, history, search…), the same keystroke ALSO
        // reaches the PTY (raw ctx.input reads bypass egui focus — see the Ctrl+Tab comment),
        // and a shell session was getting tabs closed and overlays popped by ordinary typing.
        // Only Alt+F12 (toggle the terminal itself) stays global.
        let shell = self.terminal.shell_focused();
        if !shell
            && ctx.input(|i| i.modifiers.command && i.modifiers.alt && i.key_pressed(egui::Key::L))
        {
            self.reformat_file(ctx);
        }
        if !shell
            && ctx.input(|i| i.modifiers.command && i.modifiers.alt && i.key_pressed(egui::Key::B))
        {
            self.run_command(palette::Command::GoToImplementation, ctx);
        }
        if !shell
            && ctx.input(|i| i.modifiers.command && i.modifiers.alt && i.key_pressed(egui::Key::H))
        {
            self.call_hierarchy();
        }
        // Double-Shift → Search Everywhere. Any non-shift modifier or key during the hold
        // makes it a chord, not a tap (Shift+F6, shifted typing, Ctrl+Shift+F never count).
        {
            let (shift_down, other, now) = ctx.input(|i| {
                let other = !i.keys_down.is_empty()
                    || i.modifiers.command
                    || i.modifiers.alt
                    || i.modifiers.ctrl;
                (i.modifiers.shift, other, i.time)
            });
            if self.double_shift.update(shift_down, other, now)
                && !shell
                && !self.everywhere.is_open()
                && !self.quickopen.is_open()
                && !self.palette.is_open()
            {
                self.everywhere.open(self.workspace.all_files(), &self.workspace.root);
            }
        }
        let cmd = |k: egui::Key| {
            !shell && ctx.input(|i| i.modifiers.command && !i.modifiers.shift && i.key_pressed(k))
        };
        let cmd_shift = |k: egui::Key| {
            !shell && ctx.input(|i| i.modifiers.command && i.modifiers.shift && i.key_pressed(k))
        };
        if cmd_shift(egui::Key::O) {
            if self.openfolder.is_open() {
                self.openfolder.close();
            } else {
                self.open_project_picker();
            }
        }
        if cmd_shift(egui::Key::N) {
            self.open_new_project_picker();
        }
        if cmd_shift(egui::Key::F) {
            self.open_search("");
        }
        if cmd_shift(egui::Key::P) {
            if self.palette.is_open() {
                self.palette.close();
            } else {
                self.open_palette();
            }
        }
        if cmd(egui::Key::E) {
            self.open_recent_files();
        }
        if cmd_shift(egui::Key::E) {
            self.open_recent_locations();
        }
        if cmd(egui::Key::O) {
            self.open_file_picker();
        }
        if cmd(egui::Key::N) {
            self.open_prompt(NamePrompt::NewFile { dir_rel: PathBuf::new() }, String::new());
        }
        if cmd(egui::Key::P) {
            if self.quickopen.is_open() {
                self.quickopen.close();
            } else {
                self.open_quickopen_files();
            }
        }
        if cmd(egui::Key::S) {
            self.save();
        }
        if cmd(egui::Key::J) {
            if self.bottom_open && self.bottom_tab == BottomTab::Problems {
                self.bottom_open = false;
            } else {
                self.bottom_open = true;
                self.bottom_tab = BottomTab::Problems;
            }
        }
        if cmd(egui::Key::W) {
            let (g, a) = (self.focused, self.groups[self.focused].active);
            self.request_close_tab(g, a);
        }
        // Ctrl+Tab cycles editor file tabs — but NOT while you're typing in the shell, which owns
        // every Tab it can see (completion / backtab). egui's focus lock can't defend this one: it
        // guards widget focus traversal, while this reads raw input straight off the context, so the
        // terminal has to be checked explicitly or Ctrl+Tab would both cycle files AND reach the PTY.
        if cmd(egui::Key::Tab)
            && !self.terminal.shell_focused()
            && self.groups[self.focused].files.len() > 1
        {
            let g = &mut self.groups[self.focused];
            g.active = (g.active + 1) % g.files.len();
        }
        // Ctrl+\ splits; Ctrl+Shift+\ is the editor's jump-to-matching-bracket, so exclude shift
        // here or both fire on the same chord.
        if !shell
            && ctx.input(|i| {
                i.modifiers.command && !i.modifiers.shift && i.key_pressed(egui::Key::Backslash)
            })
        {
            self.split_move_right();
        }
        // Alt+Left / Alt+Right: back/forward through the jump list. Suppressed while a TEXT FIELD
        // has focus (commit box, rename, goto-line, palette) — there egui treats Alt+Arrow as
        // word-motion, and firing nav too would yank the editor out from under the typist. The
        // custom editor widget is NOT a text field, so nav still works there. Also not while a
        // shell owns the keys.
        // egui's wants_keyboard_input() is true for ANY focused widget — including the custom
        // editor — so it alone suppresses editor-centric shortcuts exactly when they matter.
        // The editor gets an explicit pass; shells and real text fields still gate.
        let editor_focused = self
            .groups
            .get(self.focused)
            .and_then(|g| g.files.get(g.active))
            .map(|f| f.view.has_focus())
            .unwrap_or(false);
        if !self.terminal.shell_focused() && (editor_focused || !ctx.wants_keyboard_input()) {
            if ctx.input(|i| i.modifiers.alt && !i.modifiers.command && i.key_pressed(egui::Key::ArrowLeft)) {
                self.nav_back();
            }
            if ctx.input(|i| i.modifiers.alt && !i.modifiers.command && i.key_pressed(egui::Key::ArrowRight)) {
                self.nav_forward();
            }
        }
        // Alt+digit is readline's digit-argument and Alt+P a common zsh history search —
        // these panel/pin chords stay out of a focused shell too.
        if !shell && ctx.input(|i| i.modifiers.alt && i.key_pressed(egui::Key::Num1)) {
            self.project_open = !self.project_open;
        }
        if !shell && ctx.input(|i| i.modifiers.alt && i.key_pressed(egui::Key::Num7)) {
            if self.pins_open && self.right_tab == RightTab::Structure {
                self.pins_open = false;
            } else {
                self.pins_open = true;
                self.right_tab = RightTab::Structure;
            }
        }
        if !shell && ctx.input(|i| i.modifiers.alt && i.key_pressed(egui::Key::P)) {
            if let Some(f) = self.groups[self.focused].active_file() {
                let p = f.path.clone();
                if !self.pins.contains(&p) {
                    self.pins.push(p);
                    self.pins_open = true;
                }
            }
        }
        if ctx.input(|i| i.modifiers.alt && i.key_pressed(egui::Key::F12)) {
            let root = self.terminal_root();
            self.terminal.toggle(ctx, &root);
        }
        // Ctrl+Shift+F10 = run the focused FILE; plain Shift+F10 = run the project's selected
        // config. The `!command` guard is what keeps them apart: without it the project-run arm
        // also matches the Ctrl chord and both would fire on the same keystroke.
        if ctx.input(|i| i.modifiers.command && i.modifiers.shift && i.key_pressed(egui::Key::F10))
        {
            self.run_current_file(ctx);
        } else if ctx
            .input(|i| i.modifiers.shift && !i.modifiers.command && i.key_pressed(egui::Key::F10))
        {
            self.run_project(ctx, true);
        }
        if cmd(egui::Key::F9) {
            self.run_project(ctx, false);
        }
        // Editor zoom (Ctrl+± / Ctrl+0) — EDITOR-ONLY; the whole-UI zoom lives in Settings.
        let mut font_delta = 0.0f32;
        if cmd(egui::Key::Equals) || cmd(egui::Key::Plus) {
            font_delta += 1.0;
        }
        if cmd(egui::Key::Minus) {
            font_delta -= 1.0;
        }
        if cmd(egui::Key::Num0) {
            self.editor_font = 14.0;
            font_delta = f32::EPSILON; // trigger the apply below
        }
        if cmd(egui::Key::G) {
            self.open_goto_line();
        }
        // Bookmark/symbol keys fire while the EDITOR owns the keyboard (their primary use) but
        // never while a shell or a real text field does (see editor_focused above the nav block).
        if !self.terminal.shell_focused() && (editor_focused || !ctx.wants_keyboard_input()) {
            if ctx.input(|i| !i.modifiers.shift && i.key_pressed(egui::Key::F11)) {
                self.toggle_bookmark();
            }
            if ctx.input(|i| i.modifiers.shift && i.key_pressed(egui::Key::F11)) {
                self.open_bookmarks();
            }
            if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::F12)) {
                self.open_file_symbols();
            }
        }
        if !shell && ctx.input(|i| i.modifiers.shift && i.key_pressed(egui::Key::F6)) {
            self.start_rename();
        }
        // Ctrl+F6 — Change Signature (JetBrains' binding).
        if !shell && ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::F6)) {
            self.start_change_signature();
        }
        if !shell && ctx.input(|i| i.modifiers.alt && i.key_pressed(egui::Key::F7)) {
            self.find_usages();
        }
        // Ctrl+Alt+Shift+T — Refactor This.
        if !shell
            && ctx.input(|i| {
                i.modifiers.command
                    && i.modifiers.alt
                    && i.modifiers.shift
                    && i.key_pressed(egui::Key::T)
            })
        {
            if let Some(f) = self.groups[self.focused].active_file() {
                let path = f.path.clone();
                // Unlike a quick fix, a refactoring is usually ABOUT the selection (extract
                // function/variable). Collapse to the caret only when nothing is selected.
                let range = f.view.selection_byte_range();
                let range = if range.is_empty() {
                    let b = f.view.caret_byte();
                    b..b
                } else {
                    range
                };
                self.request_refactorings(path, range);
            }
        }
        if !shell && ctx.input(|i| i.modifiers.alt && i.key_pressed(egui::Key::Enter)) {
            if let Some(f) = self.groups[self.focused].active_file() {
                let (p, b) = (f.path.clone(), f.view.caret_byte());
                self.request_quick_fixes(p, b..b);
            }
        }
        if cmd(egui::Key::B) {
            if let Some(f) = self.groups[self.focused].active_file() {
                let (path, gen, byte) = (f.path.clone(), f.buffer.generation, f.view.caret_byte());
                let rope = f.buffer.rope().clone();
                self.lsp.request_definition(&path, &rope, byte, gen);
            }
        }
        if !shell && ctx.input(|i| i.key_pressed(egui::Key::Space) && i.modifiers.ctrl) {
            self.request_completion_at_caret();
        }
        // Ctrl+scroll = editor zoom (JetBrains muscle memory).
        let ctrl_scroll = ctx.input(|i| {
            if i.modifiers.command {
                i.raw_scroll_delta.y
            } else {
                0.0
            }
        });
        if ctrl_scroll > 0.5 {
            font_delta += 0.5;
        } else if ctrl_scroll < -0.5 {
            font_delta -= 0.5;
        }
        if font_delta != 0.0 {
            self.editor_font = (self.editor_font + font_delta).clamp(8.0, 40.0);
            for g in &mut self.groups {
                for f in &mut g.files {
                    f.view.set_font_size(self.editor_font);
                }
            }
        }

        // --- status bar (added first → bottom-most) ---------------------------------------------------
        egui::TopBottomPanel::bottom("status").exact_height(style::sizes::STATUS_H).show(ctx, |ui| {
            ui.horizontal_centered(|ui| {
                ui.spacing_mut().item_spacing.x = 16.0;
                let g = &self.groups[self.focused];
                if let Some(f) = g.files.get(g.active) {
                    ui.colored_label(
                        colors::TEXT_MUTED(),
                        egui::RichText::new(format!("{} lines", f.buffer.rope().len_lines()))
                            .size(style::sizes::FONT_STATUS),
                    );
                    let (err, warn) = self.diags.counts(&f.path);
                    if err > 0
                        && style::status_chip(ui, &format!("{err} ✕"), colors::ERROR()).clicked_by(egui::PointerButton::Primary)
                    {
                        self.bottom_open = true;
                        self.bottom_tab = BottomTab::Problems;
                    }
                    if warn > 0
                        && style::status_chip(ui, &format!("{warn} ⚠"), colors::WARN()).clicked_by(egui::PointerButton::Primary)
                    {
                        self.bottom_open = true;
                        self.bottom_tab = BottomTab::Problems;
                    }
                }
                let lsp_line = self.lsp.status_line();
                if !lsp_line.is_empty() {
                    ui.colored_label(
                        colors::TEXT_FAINT(),
                        egui::RichText::new(lsp_line).size(style::sizes::FONT_STATUS),
                    );
                }
                if let Some(psi_line) = self.psi.status() {
                    ui.colored_label(
                        colors::TEXT_FAINT(),
                        egui::RichText::new(psi_line).size(style::sizes::FONT_STATUS),
                    );
                }
                if let Some(m) = self.bg_message.lock().unwrap().clone() {
                    ui.colored_label(
                        colors::TEXT_FAINT(),
                        egui::RichText::new(m).size(style::sizes::FONT_STATUS),
                    );
                }
                if let Some(m) = &self.lsp_message {
                    ui.colored_label(
                        colors::TEXT_FAINT(),
                        egui::RichText::new(m).size(style::sizes::FONT_STATUS),
                    );
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // The Claude usage meter: 5h + 7d windows as % of plan budget, bottom right.
                    let _ = self.usage.poll();
                    let label =
                        self.usage.status_line().unwrap_or_else(|| "claude: —".to_string());
                    let resp = ui.colored_label(
                        ORANGE,
                        egui::RichText::new(label).size(style::sizes::FONT_STATUS),
                    );
                    if let Some(detail) = self.usage.detail_line() {
                        resp.on_hover_text(detail);
                    }
                });
            });
        });

        // --- bottom tool-window switcher ------------------------------------------------------------
        egui::TopBottomPanel::bottom("toolbar").exact_height(style::sizes::TOOLBAR_H).show(ctx, |ui| {
            ui.horizontal_centered(|ui| {
                ui.add_space(6.0);
                ui.spacing_mut().item_spacing.x = 4.0;
                if style::tool_button(ui, "Terminal", self.terminal.open).clicked_by(egui::PointerButton::Primary) {
                    let root = self.terminal_root();
                    self.terminal.toggle(ctx, &root);
                }
                if style::tool_button(
                    ui,
                    "Output",
                    self.bottom_open && self.bottom_tab == BottomTab::Output,
                )
                .clicked_by(egui::PointerButton::Primary)
                {
                    if self.bottom_open && self.bottom_tab == BottomTab::Output {
                        self.bottom_open = false;
                    } else {
                        self.bottom_open = true;
                        self.bottom_tab = BottomTab::Output;
                    }
                }
                if style::tool_button(
                    ui,
                    "Problems",
                    self.bottom_open && self.bottom_tab == BottomTab::Problems,
                )
                .clicked_by(egui::PointerButton::Primary)
                {
                    if self.bottom_open && self.bottom_tab == BottomTab::Problems {
                        self.bottom_open = false;
                    } else {
                        self.bottom_open = true;
                        self.bottom_tab = BottomTab::Problems;
                    }
                }
                {
                    let local = self.ai_settings.provider == settings::AiProvider::Ollama;
                    let tip = match (self.ai.available, local) {
                        (true, false) => "Claude inline completions: ghost text after a pause; Tab accepts, Esc dismisses. Uses your Claude Code sign-in.",
                        (true, true) => "Local inline completions (Ollama): ghost text after a pause; Tab accepts, Esc dismisses. Fully offline.",
                        (false, false) => "Sign in to Claude Code (~/.claude) to enable inline AI completions.",
                        (false, true) => "Ollama server not reachable — start it with `ollama serve` (see Settings ▸ AI).",
                    };
                    if style::tool_button(ui, "AI", self.ai.enabled && self.ai.available)
                        .on_hover_text(tip)
                        .clicked_by(egui::PointerButton::Primary)
                        && self.ai.available
                    {
                        self.ai.enabled = !self.ai.enabled;
                    }
                }
                // Inline blame chip, next to AI: same one-click toggle the palette command
                // and Settings checkbox flip; persisted.
                if style::tool_button(ui, "⎇ blame", self.inline_blame_enabled)
                    .on_hover_text("Inline git blame on the caret line (author · commit). Click to toggle.")
                    .clicked_by(egui::PointerButton::Primary)
                {
                    self.inline_blame_enabled = !self.inline_blame_enabled;
                    if !self.inline_blame_enabled {
                        if let Some(f) = self.groups[self.focused].active_file() {
                            f.view.set_inline_blame(None);
                        }
                    }
                    self.save_settings();
                }
                let std_label = match self.standards {
                    Standards::Off => "STD off",
                    Standards::Gsfc => "GSFC 582",
                    Standards::JplPot => "JPL PoT",
                };
                if style::tool_button(ui, std_label, self.standards != Standards::Off).on_hover_text(
                    "Coding-standards tier (click to cycle): GSFC 582 / cFS conventions = what \
                     cFE reviewers actually gate (clang-format check, recursion as advice) → \
                     JPL Power of Ten = strict Rule-1 errors → off.",
                ).clicked_by(egui::PointerButton::Primary)
                {
                    self.standards = match self.standards {
                        Standards::Off => Standards::Gsfc,
                        Standards::Gsfc => Standards::JplPot,
                        Standards::JplPot => Standards::Off,
                    };
                    if self.standards != Standards::Off {
                        self.psi_scan_pending = true;
                        self.refresh_nasa_squiggles();
                    } else {
                        // Strip the NASA layer everywhere, immediately. disable() also makes
                        // the worker forget retained facts + overlays: edits made while off
                        // are never shipped, so kept overlays would replay stale text later.
                        self.psi.disable();
                        self.psi_saved_files.clear();
                        self.psi_overlay_pending.clear();
                        self.psi_closed_files.clear();
                        let paths: Vec<PathBuf> = self
                            .groups
                            .iter()
                            .flat_map(|g| g.files.iter().map(|f| f.path.clone()))
                            .collect();
                        for p in paths {
                            self.diags.replace(&p, 2, Vec::new());
                            let merged = self.diags.merged(&p);
                            if let Some(f) = self.find_file_mut(&p) {
                                f.view.set_diagnostics(merged);
                            }
                        }
                    }
                }
                if style::tool_button(ui, "Git", self.bottom_open && self.bottom_tab == BottomTab::Git)
                    .clicked_by(egui::PointerButton::Primary)
                {
                    if self.bottom_open && self.bottom_tab == BottomTab::Git {
                        self.bottom_open = false;
                    } else {
                        self.bottom_open = true;
                        self.bottom_tab = BottomTab::Git;
                        let root = self.workspace.root.clone();
                        self.git_panel.refresh(&root, ctx);
                    }
                }
            });
        });

        // --- the DOCK: terminal LEFT · Output/Problems tabbed RIGHT ---------------------------------
        let mut jump: Option<usize> = None;
        let mut project_jump: Option<(PathBuf, usize)> = None;
        if self.terminal.open || self.bottom_open {
            egui::TopBottomPanel::bottom("dock")
                .resizable(true)
                .default_height(450.0)
                .min_height(100.0)
                .show(ctx, |ui| {
                    style::hairline(ui); // crisp top edge between editor and dock
                    let both = self.terminal.open && self.bottom_open;
                    if both {
                        // terminal_root(), NOT workspace.root: in no-project mode the latter is a
                        // sentinel that never exists, and TerminalPane LATCHES whatever it is
                        // handed (terminal.rs: `if self.root.is_empty()`) — so the "+ new shell"
                        // button would then spawn at a nonexistent cwd and simply fail.
                        let root = self.terminal_root();
                        ui.columns(2, |cols| {
                            self.terminal.ui_embedded(&mut cols[0], &root);
                            let (j, pj) = self.right_slot_ui(&mut cols[1]);
                            jump = j;
                            project_jump = pj;
                        });
                    } else if self.terminal.open {
                        let root = self.terminal_root();
                        self.terminal.ui_embedded(ui, &root);
                    } else {
                        let (j, pj) = self.right_slot_ui(ui);
                        jump = j;
                        project_jump = pj;
                    }
                });
        }

        // --- left rail: symmetric spacer to the right rail so the frame reads balanced
        // (tool icons can move in later). -----------------------------------------------------------
        egui::SidePanel::left("left-rail")
            .exact_width(34.0)
            .resizable(false)
            .show_separator_line(true)
            .show(ctx, |_ui| {});
        // --- right icon rail (JetBrains tool-window bar): one icon per right tab; click opens /
        // switches / collapses. The panel itself gets a single clean title. ------------------------
        egui::SidePanel::right("right-rail")
            .exact_width(34.0)
            .resizable(false)
            .show_separator_line(true)
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.vertical_centered(|ui| {
                    ui.spacing_mut().item_spacing.y = 8.0;
                    for (tab, icon, tip) in [
                        (RightTab::Pinned, icons::ToolIcon::Pin, "Pinned files (Alt+P)"),
                        (RightTab::Structure, icons::ToolIcon::Structure, "Structure (Alt+7)"),
                        (RightTab::History, icons::ToolIcon::History, "Local History"),
                        (RightTab::Ai, icons::ToolIcon::Sparkle, "AI assistant"),
                    ] {
                        let active = self.pins_open && self.right_tab == tab;
                        if icons::tool_icon_toggle(ui, icon, active, tip).clicked_by(egui::PointerButton::Primary) {
                            if active {
                                self.pins_open = false;
                            } else {
                                self.right_tab = tab;
                                self.pins_open = true;
                            }
                        }
                    }
                });
            });
        if self.pins_open {
            egui::SidePanel::right("pins").resizable(true).width_range(160.0..=460.0).default_width(335.0).show(ctx, |ui| {
                ui.horizontal(|ui| {
                    style::panel_header_inline(ui, match self.right_tab {
                        RightTab::Pinned => "Pinned",
                        RightTab::Structure => "Structure",
                        RightTab::History => "Local History",
                        RightTab::Ai => "AI Assistant",
                        RightTab::Preview => "Markdown Preview",
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("✕").clicked_by(egui::PointerButton::Primary) {
                            self.pins_open = false;
                        }
                    });
                });
                style::hairline(ui);
                ui.add_space(2.0);
                match self.right_tab {
                    RightTab::History => {
                        let cur = self
                            .groups
                            .get_mut(self.focused)
                            .and_then(|g| g.active_file())
                            .map(|f| (f.path.clone(), f.buffer.rope().to_string()));
                        if let Some((path, text)) = cur {
                            if let Some(restored) = self.history_ui.ui(ui, &text, &path) {
                                let now = ctx.input(|i| i.time);
                                let mut sync: Option<(ropey::Rope, ropey::Rope, cauldron_editor::Transaction)> = None;
                                if let Some(f) = self.find_file_mut(&path) {
                                    let pre = f.buffer.rope().clone();
                                    let tx = cauldron_editor::Transaction::replace(0, pre.len_bytes(), restored);
                                    f.view.apply_external(&mut f.buffer, &tx, now);
                                    let post = f.buffer.rope().clone();
                                    f.dirty = true;
                                    sync = Some((pre, post, tx));
                                }
                                if let Some((pre, post, tx)) = sync {
                                    self.lsp.did_change(&path, &pre, &post, &tx);
                                }
                            }
                        } else {
                            ui.colored_label(colors::TEXT_FAINT(), "no file open");
                        }
                        return; // closure ends: history tab rendered
                    }
                    RightTab::Ai => {
                        self.ai_panel.ask_context = self.current_ask_context();
                        if let Some(action) = self.ai_panel.ui(ui) {
                            let now = ctx.input(|i| i.time);
                            match action {
                                ai_actions::PanelAction::Insert(code) => {
                                    if let Some(f) = self.groups[self.focused].active_file() {
                                        f.view.paste_for_menu(&mut f.buffer, &code, now);
                                        f.dirty = true;
                                    }
                                }
                                ai_actions::PanelAction::Apply(code, origin) => {
                                    self.apply_ai_replacement(&origin, &code, now);
                                }
                            }
                        }
                        return;
                    }
                    RightTab::Preview => {
                        // Live markdown preview of the active .md buffer (or a hint otherwise).
                        let md = self
                            .groups
                            .get(self.focused)
                            .and_then(|g| g.files.get(g.active))
                            .filter(|f| f.loaded && matches!(f.path.extension().and_then(|e| e.to_str()), Some("md" | "markdown")))
                            .map(|f| f.buffer.rope().to_string());
                        match md {
                            Some(src) => mdpreview::ui(ui, &src),
                            None => {
                                ui.colored_label(colors::TEXT_FAINT(), "Open a .md file to preview it here.");
                            }
                        }
                        return;
                    }
                    RightTab::Pinned | RightTab::Structure => {}
                }
                if self.right_tab == RightTab::Structure {
                    let mut jump_pos: Option<lsp_types::Position> = None;
                    if self.outline.is_empty() {
                        ui.colored_label(
                            colors::TEXT_FAINT(),
                            "no symbols (file has no LSP, or still indexing)",
                        );
                    }
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        for (depth, glyph, name, detail, pos) in &self.outline {
                            let mut job = egui::text::LayoutJob::default();
                            let font = egui::TextStyle::Monospace.resolve(ui.style());
                            job.append(
                                &format!("{}{glyph} ", "  ".repeat(*depth)),
                                0.0,
                                egui::TextFormat {
                                    font_id: font.clone(),
                                    color: colors::AMBER(),
                                    ..Default::default()
                                },
                            );
                            job.append(
                                name,
                                0.0,
                                egui::TextFormat {
                                    font_id: font.clone(),
                                    color: colors::TEXT(),
                                    ..Default::default()
                                },
                            );
                            if !detail.is_empty() {
                                job.append(
                                    &format!("  {}", detail.chars().take(28).collect::<String>()),
                                    0.0,
                                    egui::TextFormat {
                                        font_id: font,
                                        color: colors::TEXT_FAINT(),
                                        ..Default::default()
                                    },
                                );
                            }
                            if ui.selectable_label(false, job).clicked_by(egui::PointerButton::Primary) {
                                jump_pos = Some(*pos);
                            }
                        }
                    });
                    if let Some(pos) = jump_pos {
                        let g = &mut self.groups[self.focused];
                        if let Some(f) = g.files.get_mut(g.active) {
                            // (self.lsp directly: can't call lsp_encoding(&self) under the
                            // self.groups borrow; the active file is always didOpen'ed.)
                            let enc =
                                self.lsp.encoding_for(&f.path).unwrap_or(Encoding::Utf16);
                            let rope = f.buffer.rope().clone();
                            let byte = pos_to_byte(&rope, &pos, enc);
                            f.view.jump_to(byte, &rope);
                        }
                    }
                    return;
                }
                let mut open: Option<PathBuf> = None;
                let mut unpin: Option<usize> = None;
                for (i, p) in self.pins.iter().enumerate() {
                    ui.horizontal(|ui| {
                        let (icon_rect, _) = ui
                            .allocate_exact_size(egui::Vec2::splat(14.0), egui::Sense::hover());
                        icons::file_icon(ui, icon_rect, p);
                        let name = p
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        if ui.selectable_label(false, name).clicked_by(egui::PointerButton::Primary) {
                            open = Some(p.clone());
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button(egui::RichText::new("✕").size(9.0)).clicked_by(egui::PointerButton::Primary) {
                                unpin = Some(i);
                            }
                        });
                    });
                }
                if let Some(i) = unpin {
                    self.pins.remove(i);
                }
                if let Some(p) = open {
                    self.open_file(p);
                }
            });
        }

        // --- Project tree -----------------------------------------------------------------------------------
        let mut tree_action: Option<TreeAction> = None;
        if self.project_open {
            // Files with error diagnostics squiggle in the tree — and so do their ancestor dirs.
            let problems = self.diags.error_paths();
            self.workspace.set_problem_files(problems.iter());
            egui::SidePanel::left("project").resizable(true).default_width(270.0).show(ctx, |ui| {
                let name = self.workspace.name.clone();
                style::panel_header(ui, &name);
                egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                    tree_action = self.workspace.tree_ui(ui);
                });
            });
        }

        // --- central: the editor groups (tabs render here — RIGHT of the tree) -----------------------
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(colors::BG_EDITOR()))
            .show(ctx, |ui| {
                // Diff takeover: the central area shows the side-by-side diff instead of the
                // editor. Esc closes it — unless a picker overlay is stacked above (one Escape
                // must not rip through both layers).
                if let Some(dv) = &mut self.diff_view {
                    let allow_esc = !self.palette.is_open()
                        && !self.quickopen.is_open()
                        && !self.openfolder.is_open()
                        && !self.goto_symbol.is_open()
                        && !self.search.is_open()
                        && !self.bookmarks_open
                        && !self.file_symbols_open
                        && self.prompt.is_none()
                        && self.goto_line.is_none()
                        && self.rename.is_none();
                    match dv.ui(ui, allow_esc) {
                        Some(diffview::DiffAction::Close) => {
                            self.diff_view = None;
                            // Hand the keyboard straight back to the editor — otherwise typing
                            // is dead until a click.
                            if let Some(f) = self.groups[self.focused].active_file() {
                                f.view.grab_focus();
                            }
                        }
                        Some(diffview::DiffAction::OpenInEditor(path)) => {
                            self.diff_view = None;
                            self.open_file(path);
                            if let Some(f) = self.groups[self.focused].active_file() {
                                f.view.grab_focus();
                            }
                        }
                        Some(diffview::DiffAction::SwitchMode(mode)) => {
                            let path = dv.path.clone();
                            self.open_diff_mode(&path, mode);
                        }
                        Some(diffview::DiffAction::Hunk(idx, op)) => {
                            let (path, mode) = (dv.path.clone(), dv.mode());
                            let root = self.workspace.root.clone();
                            let applied = {
                                // dv still borrows self.diff_view; re-borrow immutably for apply.
                                let view = self.diff_view.as_ref().expect("diff view present");
                                diffview::apply_hunk(&root, view, idx, op)
                            };
                            match applied {
                                Ok(()) => {
                                    // Rebuild the same mode (remaining hunks), then refresh
                                    // everything downstream of the tree/index change.
                                    self.diff_view =
                                        diffview::open_mode(&root, &path, mode);
                                    self.git_panel.refresh(&root, ctx);
                                    self.refresh_gutter(&path);
                                    if op == diffview::HunkOp::Revert {
                                        // The worktree file changed under any open buffer.
                                        self.reload_externally_changed_buffers(ctx);
                                    }
                                }
                                Err(e) => {
                                    self.lsp_message = Some(format!("git apply failed: {e}"));
                                }
                            }
                        }
                        None => {}
                    }
                    return;
                }
                if let Some(err) = &self.error {
                    ui.centered_and_justified(|ui| {
                        ui.colored_label(colors::ERROR(), err);
                    });
                    return;
                }
                let n = self.groups.len();
                if n == 1 {
                    let r = ui.max_rect();
                    self.pane_spans = vec![(r.left(), r.right())];
                    self.group_ui(ui, 0);
                } else {
                    ui.columns(n, |cols| {
                        // Capture each pane's horizontal span BEFORE rendering, so a tab drop this
                        // frame can hit-test against every pane.
                        self.pane_spans =
                            cols.iter().map(|c| (c.max_rect().left(), c.max_rect().right())).collect();
                        for (gi, col) in cols.iter_mut().enumerate() {
                            self.group_ui(col, gi);
                        }
                    });
                }
                // Apply tab closes AFTER every pane rendered — never mid-columns (would shrink
                // self.groups and crash a later column).
                self.apply_pending_tab_closes();
                // A tab released outside its own strip resolves to a move / split here, once every
                // pane's span is known for this frame.
                self.finish_tab_drag(ui);
            });

        // --- problems jumps ---------------------------------------------------------------------------
        if let Some(byte) = jump {
            if let Some(f) = self.groups[self.focused].active_file() {
                let rope = f.buffer.rope().clone();
                f.view.jump_to(byte, &rope);
            }
        }
        if let Some((path, offset)) = project_jump {
            self.open_file(path);
            if let Some(f) = self.groups[self.focused].active_file() {
                let rope = f.buffer.rope().clone();
                f.view.jump_to(offset, &rope);
            }
        }

        // --- route edits from EVERY loaded buffer to the LSP ----------------------------------
        // Every loaded tab, not just each group's active one: apply_workspace_edit (rename,
        // code actions, server applyEdit) and completion additional-edits write into BACKGROUND
        // tabs via apply_external, and an undrained edit there means the server never gets its
        // didChange — the next edit against that file is computed on stale text and lands at
        // wrong offsets. take_edits on an untouched tab is a free empty-Vec check.
        // System-theme sync: while the choice is System (RuneOS only), poll the OS every ~3s
        // and re-apply if the light/dark preference flipped. gdbus subprocess at 1/3 Hz is
        // negligible; the poll is skipped entirely for every non-System choice.
        if self.theme_choice == settings::ThemeChoice::System {
            let now = ctx.input(|i| i.time);
            if now - self.system_theme_poll >= 3.0 {
                self.system_theme_poll = now;
                if systheme::resolve(settings::ThemeChoice::System) != style::active_theme() {
                    self.apply_theme(ctx);
                }
                ctx.request_repaint_after(std::time::Duration::from_secs(3));
            }
        }
        // Idle auto-save: fires 1s after the last edit (armed below). A failed write surfaces
        // in the status line and retries on the next edit — never in a hot loop.
        if self.autosave_deadline.is_some_and(|t| ctx.input(|i| i.time) >= t) {
            self.autosave_deadline = None;
            self.save_dirty(None);
        }
        // Build-before-debug: the parked launch fires when OUR cargo build finishes. A
        // different run replacing it in the runner (title mismatch) cancels the park.
        if let Some(bin) = self.debug_pending_build.clone() {
            if !self.runner.title.starts_with("cargo build") {
                self.debug_pending_build = None;
            } else if !self.runner.running {
                self.debug_pending_build = None;
                if self.runner.exit == Some(0) {
                    self.dbg_say("build OK — launching debugger");
                    self.launch_lldb(bin);
                } else {
                    self.dbg_say("build failed — fix the errors, then Debug again");
                }
            }
        }
        let mut edited: Vec<EditedBatch> = Vec::new();
        let mut edited_chars: Vec<(char, PathBuf)> = Vec::new();
        let focused = self.focused;
        for (gidx, g) in self.groups.iter_mut().enumerate() {
            let active = g.active;
            for (fidx, f) in g.files.iter_mut().enumerate() {
                if !f.loaded {
                    continue;
                }
                let edits = f.view.take_edits();
                if !edits.is_empty() {
                    f.dirty = true;
                    // Item 7: C edits (re)arm the dirty-buffer overlay debounce — after the
                    // quiet window the live text ships to the PSI worker, so NASA squiggles
                    // update without saving. Standards off = no NASA layer to feed.
                    if self.standards != Standards::Off
                        && matches!(f.lang, Some(Lang::C) | Some(Lang::Cpp))
                    {
                        self.psi_overlay_pending
                            .insert(f.path.clone(), std::time::Instant::now());
                    }
                    // Only the focused editor's edits can be TYPED chars — background-tab
                    // edits must never auto-trigger completion at the focused caret.
                    let typed_here = gidx == focused && fidx == active;
                    edited.push((f.path.clone(), edits, f.buffer.rope().clone(), typed_here));
                }
            }
        }
        if !edited.is_empty() {
            // Auto-save: (re)arm the idle deadline on every edit — the save fires once the
            // user pauses for a second (JetBrains model; the close/exit paths also save).
            self.autosave_deadline = Some(ctx.input(|i| i.time) + 1.0);
            ctx.request_repaint_after(std::time::Duration::from_millis(1050));
        }
        for (path, edits, post, typed_here) in edited {
            // Breakpoints ride their code: shift stored lines through each transaction so the
            // dot, the session file, and the adapter all keep pointing at the marked STATEMENT
            // after lines are inserted/deleted above it.
            for (pre, tx) in edits.iter() {
                self.remap_breakpoints(&path, pre, tx);
                self.remap_bookmarks(&path, pre, tx);
            }
            for (i, (pre, tx)) in edits.iter().enumerate() {
                if typed_here && tx.changes.len() == 1 && tx.changes[0].text.chars().count() == 1 {
                    if let Some(c) = tx.changes[0].text.chars().next() {
                        edited_chars.push((c, path.clone()));
                    }
                }
                let post_i: Rope =
                    edits.get(i + 1).map(|(p, _)| p.clone()).unwrap_or_else(|| post.clone());
                self.lsp.did_change(&path, pre, &post_i, tx);
                self.diags.map_through(&path, tx);
            }
            squiggle_paths.push(path);
        }
        for path in squiggle_paths {
            let merged = self.diags.merged(&path);
            if let Some(f) = self.find_file_mut(&path) {
                f.view.set_diagnostics(merged);
            }
        }

        // --- goto-definition via Ctrl+Click ---------------------------------------------------
        if let Some(f) = self.groups[self.focused].active_file() {
            if let Some(byte) = f.view.take_ctrl_click() {
                let (path, gen) = (f.path.clone(), f.buffer.generation);
                let rope = f.buffer.rope().clone();
                self.lsp.request_definition(&path, &rope, byte, gen);
            }
        }
        // --- background run-config detection (boot-wave item 2) ------------------------------
        // The merge is APPEND-only (kind+program dedup): user edits are never clobbered and a
        // selection the user already made never moves or flashes. Persist only when new
        // suggestions actually landed.
        if self.run_cfgs.poll_detect() && !self.no_project {
            self.run_cfgs.save(&self.workspace.root);
        }
        // --- symbol index + goto-symbol overlay (Ctrl+Alt+N) --------------------------------------
        // Rebuilds are EVENT-driven (build-on-open, project switch, watcher bursts) via the
        // pending flag — the old rebuild-only-when-empty kick is retired; per-file updates go
        // through `refresh_files` in the watcher drain.
        self.symbols.poll();
        if take_symbol_rebuild_kick(&mut self.symbols_rebuild_pending, self.symbols.is_building())
        {
            // ONE file universe: the index consumes the workspace walk (excludes pre-applied).
            let files = self.workspace.all_files().to_vec();
            self.symbols.rebuild(&files, ctx);
        }
        if ctx.input(|i| i.modifiers.command && i.modifiers.alt && i.key_pressed(egui::Key::N)) {
            self.open_goto_symbol();
        }
        // C tier (cauldron#2 item 8): while the overlay is up, supersede the regex rows for C
        // files with PSI truth — stub-exact names/kinds/lines from the retained index, files
        // never opened included, current through the save/overlay/watcher invalidation lanes.
        // Synced lazily HERE so index churn while the overlay is closed costs nothing; while
        // Indexing the last synced tier is kept (it re-syncs the frame the result lands).
        if self.goto_symbol.is_open() {
            match &self.psi.state {
                PsiState::Ready { index, .. } => {
                    let index = std::sync::Arc::clone(index);
                    self.symbols.sync_psi(&index);
                }
                PsiState::NotCProject => self.symbols.clear_psi(),
                PsiState::Indexing => {}
            }
        }
        // LSP tier (cauldron#2 item 9): while the overlay is up, fan its query out as
        // workspace/symbol to every INDEXED live server (quiescence-gated in the manager — a
        // half-indexed rust-analyzer is skipped, never stalled on). Answers accumulate into
        // the SymbolIndex LSP tier and merge into the same list: PSI wins for C, LSP for its
        // languages, regex fills gaps, deduped by (path, line). Empty query = local tiers only
        // (an unfiltered workspace/symbol dump of a big project helps nobody).
        if self.goto_symbol.is_open() {
            let q = self.goto_symbol.query_text().trim().to_string();
            if self.ws_symbols_sent.as_deref() != Some(q.as_str()) {
                self.ws_symbols_sent = Some(q.clone());
                self.symbols.clear_lsp();
                if !q.is_empty() {
                    self.ws_symbols_gen = self.ws_symbols_gen.wrapping_add(1);
                    self.lsp.request_workspace_symbols(&q, self.ws_symbols_gen);
                }
            }
        } else if self.ws_symbols_sent.take().is_some() {
            // Overlay closed: the tier answered a query that no longer exists.
            self.symbols.clear_lsp();
        }
        if let Some((path, line)) = self.goto_symbol.ui(ctx, &self.symbols) {
            self.open_file(path.clone());
            self.goto_file_line(&path, line);
        }
        // --- debugger: pump events, route gutter clicks to breakpoints, shortcuts ----------------
        self.pump_debugger();
        {
            // Keep each visible file's gutter test-markers current (cheap linear rescan, only
            // when the buffer generation moved), and collect gutter clicks of both kinds.
            let mut toggles: Vec<(PathBuf, usize)> = Vec::new();
            let mut test_runs: Vec<(PathBuf, usize)> = Vec::new();
            let scan_ok = self.last_test_scan.elapsed().as_millis() >= 300;
            for g in &mut self.groups {
                if let Some(f) = g.files.get_mut(g.active) {
                    // Only extensions the scanner understands pay the rope.to_string() — and a
                    // typing burst is throttled (the ▶ set changing 300ms late is invisible).
                    let ext = f.path.extension().and_then(|e| e.to_str()).unwrap_or("");
                    if f.loaded && matches!(ext, "rs" | "py" | "cs") && scan_ok {
                        let ext = ext.to_string();
                        let generation = f.buffer.generation;
                        let stale = self
                            .test_decls
                            .get(&f.path)
                            .map(|(g0, _)| *g0 != generation)
                            .unwrap_or(true);
                        if stale {
                            self.last_test_scan = std::time::Instant::now();
                            let decls =
                                testrun::find_test_decls(&f.buffer.rope().to_string(), &ext);
                            f.view.test_lines = decls.iter().map(|(l, _)| *l).collect();
                            self.test_decls.insert(f.path.clone(), (generation, decls));
                        }
                    }
                }
                for f in &mut g.files {
                    if let Some(line) = f.view.take_gutter_click() {
                        toggles.push((f.path.clone(), line));
                    }
                    if let Some(line) = f.view.take_test_click() {
                        test_runs.push((f.path.clone(), line));
                    }
                }
            }
            for (path, line) in test_runs {
                let name = self
                    .test_decls
                    .get(&path)
                    .and_then(|(_, decls)| decls.iter().find(|(l, _)| *l == line))
                    .map(|(_, n)| n.clone());
                if let Some(name) = name {
                    // Tests run against DISK — save first, same contract as Run.
                    self.flush_dirty_buffers();
                    let root = self.workspace.root.clone();
                    let rel = path.strip_prefix(&root).unwrap_or(&path).to_path_buf();
                    self.testrun.run_named(&root, &rel, &name, ctx);
                    self.bottom_open = true;
                    self.bottom_tab = BottomTab::Tests;
                }
            }
            for (path, line0) in toggles {
                let bps = self.breakpoints.entry(path.clone()).or_default();
                let l1 = line0 as u32 + 1;
                match bps.binary_search_by_key(&l1, |(l, _)| *l) {
                    Ok(i) => {
                        bps.remove(i);
                    }
                    Err(i) => bps.insert(i, (l1, None)),
                }
                if bps.is_empty() {
                    self.breakpoints.remove(&path);
                }
                self.sync_breakpoints(&path);
            }
        }
        if self.dap.is_running() {
            let (f7, f8, f9, shift) = ctx.input(|i| {
                (
                    i.key_pressed(egui::Key::F7),
                    i.key_pressed(egui::Key::F8),
                    i.key_pressed(egui::Key::F9),
                    i.modifiers.shift,
                )
            });
            if self.dap.is_stopped() {
                if f9 {
                    self.dap.continue_run();
                }
                if f8 && !shift {
                    self.dap.next();
                }
                if f8 && shift {
                    self.dap.step_out();
                }
                if f7 {
                    self.dap.step_in();
                }
            }
        } else if ctx.input(|i| i.key_pressed(egui::Key::F9) && i.modifiers.shift) {
            self.start_debug(ctx);
        }
        // --- AI ghost completions ------------------------------------------------------------------
        // Gated on AI actually being on: with it off (the common case) this block did the single
        // biggest per-keystroke cost in the editor — `rope().to_string()` serialized the WHOLE file
        // to a fresh String every frame, defeating the lazy `FnOnce` that `ai.tick` already takes.
        // Now the per-frame cost is an O(1) rope Arc-clone; the full serialization happens ONLY
        // inside the closure, i.e. only when tick's debounce actually fires a request.
        if self.ai.enabled && self.ai.available {
            let focused_file = self
                .groups
                .get_mut(self.focused)
                .and_then(|g| g.active_file())
                .map(|f| {
                    (
                        f.view.ghost_anchor(&f.buffer).map(|(b, gen)| (f.path.clone(), b, gen)),
                        !f.view.ghost_visible(),
                        f.buffer.rope().clone(), // O(1) — NOT to_string()
                    )
                });
            let (anchor, no_ghost, rope) = match focused_file {
                Some((a, ng, r)) => (a, ng, Some(r)),
                None => (None, false, None),
            };
            // Only ask while nothing is already showing and no completion popup is up.
            let quiet = no_ghost && self.completion.is_none();
            let byte = anchor.as_ref().map(|(_, b, _)| *b).unwrap_or(0);
            if quiet {
                if let Some(rope) = rope {
                    self.ai.tick(anchor, move || rope.to_string(), byte, ctx);
                }
            }
            for ghost in self.ai.pump() {
                if let Some(f) = self.find_file_mut(&ghost.path) {
                    if f.view.ghost_anchor(&f.buffer) == Some((ghost.byte, ghost.generation)) {
                        f.view.set_ghost(ghost.byte, ghost.generation, ghost.text);
                    }
                }
            }
        }
        // --- C dependency auto-resolution: kick once when the workspace shows C files ------------
        if !self.c_deps_kicked
            && self
                .groups
                .iter()
                .flat_map(|g| g.files.iter())
                .any(|f| matches!(f.lang, Some(Lang::C) | Some(Lang::Cpp)))
        {
            self.c_deps_kicked = true;
            deps::resolve_c_deps(
                self.workspace.root.clone(),
                // ONE file universe: header dirs come from the workspace walk.
                self.workspace.all_files().to_vec(),
                Arc::clone(&self.bg_message),
                Arc::clone(&self.clangd_restart),
                ctx.clone(),
            );
        }
        // --- deps resolver finished: bounce clangd onto the new compile DB ------------------------
        if self.clangd_restart.swap(false, std::sync::atomic::Ordering::SeqCst) {
            self.lsp.restart_kind(cauldron_lsp::ServerKind::Clangd);
        }
        // --- editor right-click menu -------------------------------------------------------------
        for g in &mut self.groups {
            if let Some(f) = g.files.get_mut(g.active) {
                if let Some(byte) = f.view.take_context_click() {
                    if let Some(pos) = ctx_pointer_pos() {
                        self.editor_menu = Some((pos, f.path.clone(), byte));
                        // The type-info hover must yield to the menu immediately.
                        self.hover_popup = None;
                        self.hover_wait = None;
                    }
                }
            }
        }
        if let Some((pos, path, byte)) = self.editor_menu.clone() {
            let mut close = false;
            egui::Area::new("editor-menu".into())
                .fixed_pos(pos)
                .order(egui::Order::Foreground)
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.set_min_width(230.0);
                        let now = ui.input(|i| i.time);
                        // Full-width rows with the theme's subtle hover lift (BG_RAISED).
                        let item = |ui: &mut egui::Ui, label: &str| {
                            ui.set_min_width(230.0);
                            ui.add_sized(
                                [ui.available_width().max(230.0), 22.0],
                                egui::SelectableLabel::new(
                                    false,
                                    egui::RichText::new(label).size(13.0),
                                ),
                            )
                            .clicked_by(egui::PointerButton::Primary)
                        };
                        if item(ui, "Go to Definition        Ctrl+B") {
                            if let Some(f) = self.find_file_mut(&path) {
                                let gen = f.buffer.generation;
                                let rope = f.buffer.rope().clone();
                                self.lsp.request_definition(&path, &rope, byte, gen);
                            }
                            close = true;
                        }
                        if item(ui, "Find Usages             Alt+F7") {
                            self.find_usages();
                            close = true;
                        }
                        if item(ui, "Rename Symbol…          Shift+F6") {
                            self.start_rename();
                            close = true;
                        }
                        if item(ui, "Quick Fix…              Alt+Enter") {
                            self.request_quick_fixes(path.clone(), byte..byte);
                            close = true;
                        }
                        ui.separator();
                        if item(ui, "Cut                     Ctrl+X") {
                            if let Some(f) = self.find_file_mut(&path) {
                                let text = f.view.copy_text_for_menu(&f.buffer);
                                ui.ctx().copy_text(text);
                                f.view.cut_selection_for_menu(&mut f.buffer, now);
                                f.dirty = true;
                            }
                            close = true;
                        }
                        if item(ui, "Copy                    Ctrl+C") {
                            if let Some(f) = self.find_file_mut(&path) {
                                let text = f.view.copy_text_for_menu(&f.buffer);
                                ui.ctx().copy_text(text);
                            }
                            close = true;
                        }
                        if item(ui, "Paste                   Ctrl+V") {
                            let text = cider::widget::read_clipboard().unwrap_or_default();
                            if let Some(f) = self.find_file_mut(&path) {
                                f.view.paste_for_menu(&mut f.buffer, &text, now);
                                if !text.is_empty() {
                                    f.dirty = true;
                                }
                            }
                            close = true;
                        }
                        if item(ui, "Select All              Ctrl+A") {
                            if let Some(f) = self.find_file_mut(&path) {
                                f.view.select_all_for_menu(&f.buffer);
                            }
                            close = true;
                        }
                        ui.separator();
                        if item(ui, "AI: Explain Selection") {
                            if let Some(f) = self.find_file_mut(&path) {
                                let code = f.view.copy_text_for_menu(&f.buffer);
                                let lang = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_string();
                                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
                                let sel = f.view.selection_byte_range();
                                let origin = (!sel.is_empty()).then(|| ai_actions::Origin {
                                    path: path.clone(),
                                    range: sel,
                                    generation: f.buffer.generation,
                                });
                                self.ai_panel.submit(
                                    ai_actions::AiTaskKind::ExplainSelection,
                                    ai_actions::AiContext { file_name: name, language: lang, code, origin, ..Default::default() },
                                    ui.ctx(),
                                );
                                self.pins_open = true;
                                self.right_tab = RightTab::Ai;
                            }
                            close = true;
                        }
                        if item(ui, "AI: Edit Selection…") {
                            if let Some(f) = self.find_file_mut(&path) {
                                let sel = f.view.selection_byte_range();
                                if !sel.is_empty() {
                                    let rope = f.buffer.rope();
                                    let code = rope
                                        .byte_slice(sel.start.min(rope.len_bytes())..sel.end.min(rope.len_bytes()))
                                        .to_string();
                                    let lang = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_string();
                                    let (tx, rx) = std::sync::mpsc::channel();
                                    self.ai_edit = Some(AiEdit {
                                        origin: ai_actions::Origin {
                                            path: path.clone(),
                                            range: sel,
                                            generation: f.buffer.generation,
                                        },
                                        code,
                                        lang,
                                        instruction: String::new(),
                                        in_flight: false,
                                        focus_pending: true,
                                        error: None,
                                        rx,
                                        tx,
                                    });
                                } else {
                                    self.lsp_message = Some("select the code to edit first".into());
                                }
                            }
                            close = true;
                        }
                        if item(ui, "AI: Write Unit Test") {
                            if let Some(f) = self.find_file_mut(&path) {
                                let code = f.view.copy_text_for_menu(&f.buffer);
                                let lang = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_string();
                                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
                                self.ai_panel.submit(
                                    ai_actions::AiTaskKind::WriteUnitTest,
                                    ai_actions::AiContext { file_name: name, language: lang, code, ..Default::default() },
                                    ui.ctx(),
                                );
                                self.pins_open = true;
                                self.right_tab = RightTab::Ai;
                            }
                            close = true;
                        }
                        ui.separator();
                        if item(ui, "Pin File                Alt+P") {
                            if !self.pins.contains(&path) {
                                self.pins.push(path.clone());
                                self.pins_open = true;
                            }
                            close = true;
                        }
                    });
                });
            // Any click elsewhere or Esc closes.
            if close
                || ctx.input(|i| i.key_pressed(egui::Key::Escape))
                || ctx.input(|i| {
                    i.pointer.any_pressed()
                        && i.pointer
                            .interact_pos()
                            .map(|p| (p - pos).length() > 260.0)
                            .unwrap_or(false)
                })
            {
                self.editor_menu = None;
            }
        }
        // --- completion auto-trigger: fires on word-char / member-access typing ----------------
        if let Some((ch, _path)) = last_typed_char(&edited_chars) {
            if ch.is_alphanumeric() || ch == '_' || ch == '.' || ch == ':' || ch == '>' {
                self.request_completion_at_caret();
            } else {
                self.completion = None;
            }
            match ch {
                '(' | ',' => {
                    if let Some(f) = self.groups[self.focused].active_file() {
                        let (p, gen, byte) =
                            (f.path.clone(), f.buffer.generation, f.view.caret_byte());
                        let rope = f.buffer.rope().clone();
                        self.sig_gen = Some(gen);
                        self.lsp.request_signature_help(&p, &rope, byte, gen);
                    }
                }
                ')' | '\n' => self.sig_help = None,
                _ => {}
            }
        }
        self.drive_inline_blame(ctx);
        self.coverage.pump();
        if let Some(done) = self.coverage.finished.take() {
            match done {
                Ok((covered, total)) => {
                    let pct = if total > 0 { covered * 100 / total } else { 0 };
                    self.lsp_message =
                        Some(format!("coverage: {covered}/{total} lines ({pct}%)"));
                    self.push_coverage_marks();
                }
                Err(e) => self.lsp_message = Some(e),
            }
        }
        self.drive_completion_popup(ctx);

        // Structure panel data: (re)request when the active file or its content changed.
        if (self.pins_open && self.right_tab == RightTab::Structure) || self.file_symbols_open {
            let g = &self.groups[self.focused];
            if let Some(f) = g.files.get(g.active) {
                let key = (f.path.clone(), f.buffer.generation);
                if self.outline_for.as_ref() != Some(&key)
                    && self.outline_requested.as_ref() != Some(&key)
                {
                    self.outline_requested = Some(key.clone());
                    self.lsp.request_document_symbols(&key.0, key.1);
                }
            }
        }

        // Inlay hints: (re)request for the active file whenever its content generation moves
        // (same requested/for guard pair the Structure panel uses).
        if self.inlay_hints_on {
            let g = &self.groups[self.focused];
            if let Some(f) = g.files.get(g.active) {
                if f.loaded {
                    let key = (f.path.clone(), f.buffer.generation);
                    if self.inlay_for.as_ref() != Some(&key)
                        && self.inlay_requested.as_ref() != Some(&key)
                    {
                        self.inlay_requested = Some(key.clone());
                        let rope = f.buffer.rope().clone();
                        self.lsp.request_inlay_hints(&key.0, &rope, key.1);
                    }
                }
            }
        }

        // Signature help popup (renders above the caret line).
        if let Some((pos, help)) = &self.sig_help {
            if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.sig_help = None;
            } else {
                let active_sig = help.active_signature.unwrap_or(0) as usize;
                let sig = &help.signatures[active_sig.min(help.signatures.len() - 1)];
                let active_param = sig
                    .active_parameter
                    .or(help.active_parameter)
                    .unwrap_or(0) as usize;
                let label = sig.label.clone();
                // Active parameter range within the label (offsets form preferred).
                let param_range = sig.parameters.as_ref().and_then(|ps| {
                    ps.get(active_param).and_then(|p| match &p.label {
                        // Offsets are UTF-16 CODE UNITS per the LSP spec — using them as byte
                        // indices panicked the slice below on any non-ASCII in the signature.
                        lsp_types::ParameterLabel::LabelOffsets([a, b]) => {
                            let s = utf16_off_to_byte(&label, *a as usize);
                            let e = utf16_off_to_byte(&label, *b as usize);
                            Some(s..e.max(s))
                        }
                        lsp_types::ParameterLabel::Simple(s) => {
                            label.find(s.as_str()).map(|i| i..i + s.len())
                        }
                    })
                });
                egui::Area::new("sighelp".into())
                    .fixed_pos(*pos - egui::vec2(0.0, 46.0))
                    .order(egui::Order::Tooltip)
                    .show(ctx, |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.set_max_width(640.0);
                            let mut job = egui::text::LayoutJob::default();
                            let font = egui::TextStyle::Monospace.resolve(ui.style());
                            match &param_range {
                                Some(r) if r.end <= label.len() => {
                                    let mk = |_t: &str, c: egui::Color32, u: bool| egui::TextFormat {
                                        font_id: font.clone(),
                                        color: c,
                                        underline: if u {
                                            egui::Stroke::new(1.0, colors::ACCENT())
                                        } else {
                                            egui::Stroke::NONE
                                        },
                                        ..Default::default()
                                    };
                                    job.append(&label[..r.start], 0.0, mk("", colors::TEXT_MUTED(), false));
                                    job.append(
                                        &label[r.clone()],
                                        0.0,
                                        mk("", colors::ACCENT_HI(), true),
                                    );
                                    job.append(&label[r.end..], 0.0, mk("", colors::TEXT_MUTED(), false));
                                }
                                _ => job.append(
                                    &label,
                                    0.0,
                                    egui::TextFormat {
                                        font_id: font.clone(),
                                        color: colors::TEXT_MUTED(),
                                        ..Default::default()
                                    },
                                ),
                            }
                            ui.label(job);
                            if help.signatures.len() > 1 {
                                ui.colored_label(
                                    colors::TEXT_FAINT(),
                                    format!("{} of {} overloads", active_sig + 1, help.signatures.len()),
                                );
                            }
                        });
                    });
            }
        }

        // --- LSP hover ("code lens on hover"): request after the pointer rests on a symbol ----
        let now_t = ctx.input(|i| i.time);
        let hovered = if self.editor_menu.is_some() || self.fix_menu.is_some() {
            None // a context/fix menu owns the pointer — no hover requests underneath it
        } else {
            self.groups.get_mut(self.focused).and_then(|g| {
            let path = g.files.get(g.active).map(|f| f.path.clone());
            g.active_file().and_then(|f| f.view.hovered_byte()).zip(path)
            })
        };
        match hovered {
            Some((byte, path)) => {
                let stale = match &self.hover_wait {
                    Some((p, b, _, _)) => *p != path || b.abs_diff(byte) > 2,
                    None => true,
                };
                if stale {
                    self.hover_wait = Some((path, byte, now_t, false));
                    self.hover_popup = None;
                } else if let Some((p, b, since, requested)) = &mut self.hover_wait {
                    if !*requested && now_t - *since > 0.45 {
                        *requested = true;
                        let (p, b) = (p.clone(), *b);
                        if let Some(f) =
                            self.groups
                                .iter()
                                .flat_map(|g| g.files.iter())
                                .find(|f| f.path == p && f.loaded)
                        {
                            let gen = f.buffer.generation;
                            let rope = f.buffer.rope().clone();
                            self.lsp.request_hover(&p, &rope, b, gen);
                        }
                    }
                }
            }
            None => {
                self.hover_wait = None;
                self.hover_popup = None;
            }
        }
        if let Some((pos, path, actions)) = self.fix_menu.clone() {
            let mut close_menu = false;
            egui::Area::new("quickfixes".into())
                .fixed_pos(pos)
                .order(egui::Order::Foreground)
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.set_min_width(260.0);
                        style::panel_header_inline(ui, self.fix_menu_title);
                        // Rename is ours, not the server's — it has a dedicated inline editor and
                        // its own shortcut, so it never shows up among code actions. In a
                        // Refactor This menu its absence would be conspicuous.
                        if self.fix_menu_title == "Refactor This" {
                            if ui
                                .selectable_label(
                                    false,
                                    egui::RichText::new("Rename…                Shift+F6").size(12.5),
                                )
                                .clicked_by(egui::PointerButton::Primary)
                            {
                                self.start_rename();
                                close_menu = true;
                            }
                            // Change Signature is ours (PSI-driven) — no C language server
                            // implements it, so it never appears among the server's actions.
                            if self.change_signature_available()
                                && ui
                                    .selectable_label(
                                        false,
                                        egui::RichText::new("Change Signature…      Ctrl+F6")
                                            .size(12.5),
                                    )
                                    .clicked_by(egui::PointerButton::Primary)
                            {
                                self.start_change_signature();
                                close_menu = true;
                            }
                            ui.separator();
                        }
                        let mut group_shown = "";
                        for (i, action) in actions.iter().enumerate() {
                            let group = action_group(action);
                            if group != group_shown {
                                if !group_shown.is_empty() {
                                    ui.add_space(2.0);
                                }
                                group_shown = group;
                                ui.label(
                                    egui::RichText::new(group)
                                        .size(10.5)
                                        .color(style::colors::TEXT_MUTED()),
                                );
                            }
                            let (title, preferred) = match action {
                                lsp_types::CodeActionOrCommand::Command(c) => (c.title.clone(), false),
                                lsp_types::CodeActionOrCommand::CodeAction(a) => {
                                    (a.title.clone(), a.is_preferred.unwrap_or(false))
                                }
                            };
                            let label = if preferred { format!("★ {title}") } else { title };
                            if ui
                                .selectable_label(false, egui::RichText::new(label).size(12.5))
                                .clicked_by(egui::PointerButton::Primary)
                            {
                                let now = ctx.input(|inp| inp.time);
                                self.apply_code_action(&path, &actions[i].clone(), now);
                                close_menu = true;
                            }
                        }
                    });
                });
            if close_menu || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.fix_menu = None;
            }
        }
        // Any click dismisses the hover — and the popup is click-THROUGH (interactable false):
        // it floats right next to the pointer, so an interactive popup swallows the very click
        // that was meant to place the caret underneath it.
        if ctx.input(|i| i.pointer.any_pressed()) {
            self.hover_popup = None;
        }
        if let Some((pos, text)) = &self.hover_popup {
            egui::Area::new("lsp-hover".into())
                .fixed_pos(*pos + egui::vec2(12.0, 14.0))
                .order(egui::Order::Tooltip)
                .interactable(false)
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.set_max_width(560.0);
                        ui.label(
                            egui::RichText::new(text).size(12.0).monospace().color(colors::TEXT()),
                        );
                    });
                });
        }

        // --- goto line (Ctrl+G) -----------------------------------------------------------------
        self.bookmarks_overlay_ui(ctx);
        self.file_symbols_overlay_ui(ctx);
        if let Some(mut buf) = self.goto_line.take() {
            // One-shot focus grab on the open frame only (see prompt_focus_pending).
            let grab_focus = std::mem::take(&mut self.prompt_focus_pending);
            let mut keep = true;
            egui::Area::new("gotoline".into())
                .anchor(egui::Align2::CENTER_TOP, [0.0, 120.0])
                .order(egui::Order::Foreground)
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style())
                        .inner_margin(egui::Margin::same(style::sizes::OVERLAY_PAD))
                        .show(ui, |ui| {
                            ui.set_width(260.0);
                            style::panel_header_inline(ui, "Go to Line");
                            let resp = ui.add(
                                egui::TextEdit::singleline(&mut buf)
                                    .hint_text("line[:column]")
                                    .desired_width(f32::INFINITY)
                                    .font(egui::TextStyle::Monospace),
                            );
                            if grab_focus {
                                resp.request_focus();
                            }
                            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                                keep = false;
                            }
                            if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                keep = false;
                                let mut it = buf.split(':');
                                let line = it
                                    .next()
                                    .and_then(|t| t.trim().parse::<usize>().ok())
                                    .unwrap_or(1)
                                    .saturating_sub(1);
                                let col = it
                                    .next()
                                    .and_then(|t| t.trim().parse::<usize>().ok())
                                    .unwrap_or(1)
                                    .saturating_sub(1);
                                // Resolve the byte against the current file, then navigate_to so
                                // Go-to-Line is on the back/forward list too.
                                let dest = self.groups[self.focused].active_file().map(|f| {
                                    let rope = f.buffer.rope();
                                    let l = line.min(rope.len_lines().saturating_sub(1));
                                    let start = rope.line_to_byte(l);
                                    let max_col = rope.line(l).len_bytes();
                                    (f.path.clone(), start + col.min(max_col))
                                });
                                if let Some((path, byte)) = dest {
                                    self.navigate_to(path, byte);
                                }
                            }
                        });
                });
            if keep {
                self.goto_line = Some(buf);
            }
        }

        // --- rename overlay (Shift+F6) -----------------------------------------------------------
        if let Some((mut buf, path, byte, gen)) = self.rename.take() {
            // One-shot focus grab on the open frame only (see prompt_focus_pending).
            let grab_focus = std::mem::take(&mut self.prompt_focus_pending);
            let mut keep = true;
            egui::Area::new("rename".into())
                .anchor(egui::Align2::CENTER_TOP, [0.0, 120.0])
                .order(egui::Order::Foreground)
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style())
                        .inner_margin(egui::Margin::same(style::sizes::OVERLAY_PAD))
                        .show(ui, |ui| {
                            ui.set_width(320.0);
                            style::panel_header_inline(ui, "Rename Symbol");
                            let resp = ui.add(
                                egui::TextEdit::singleline(&mut buf)
                                    .desired_width(f32::INFINITY)
                                    .font(egui::TextStyle::Monospace),
                            );
                            if grab_focus {
                                resp.request_focus();
                            }
                            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                                keep = false;
                            }
                            if ui.input(|i| i.key_pressed(egui::Key::Enter))
                                && !buf.trim().is_empty()
                            {
                                keep = false;
                                if let Some(f) = self
                                    .groups
                                    .iter()
                                    .flat_map(|g| g.files.iter())
                                    .find(|f| f.path == path && f.loaded)
                                {
                                    let rope = f.buffer.rope().clone();
                                    self.lsp.request_rename(&path, &rope, byte, buf.trim(), gen);
                                    self.lsp_message = Some("renaming…".into());
                                }
                            }
                        });
                });
            if keep {
                self.rename = Some((buf, path, byte, gen));
            }
        }

        // --- overlays ---------------------------------------------------------------------------------------
        self.prompt_ui(ctx);
        self.close_confirm_ui(ctx);
        self.ai_edit_ui(ctx);
        self.change_signature_ui(ctx);
        self.def_choices_ui(ctx);
        self.conflict_resolver_ui(ctx);
        self.recent_locations_ui(ctx);
        match self.openfolder.ui(ctx) {
            Some(PickAction::OpenProject(dir)) => self.open_folder(dir),
            Some(PickAction::OpenFile(file)) => self.open_file(file),
            None => {}
        }
        let picked = self.quickopen.ui(ctx);
        if let Some(path) = picked {
            self.open_file(path);
        }
        // Command palette: draw, and run whatever action was chosen.
        if let Some(cmd) = self.palette.ui(ctx) {
            self.run_command(cmd, ctx);
        }
        // Search Everywhere (double-Shift): files open, symbols jump, actions run.
        if let Some(hit) = self.everywhere.ui(ctx, &self.symbols) {
            match hit {
                everywhere::Hit::File(p) => self.open_file(p),
                everywhere::Hit::Symbol(p, line) => {
                    self.open_file(p.clone());
                    self.goto_file_line(&p, line);
                }
                everywhere::Hit::Command(c) => self.run_command(c, ctx),
            }
        }
        if let Some((path, line)) = {
            let files = self.workspace.all_files().to_vec();
            let root = self.workspace.root.clone();
            // Dirty-buffer overlay: unsaved editor contents shadow disk for Find in Files, so
            // hits and line numbers match what the user sees. LAZY — the panel materializes it
            // only on the frames a search actually (re)launches, never per repaint (flattening
            // every dirty rope at repaint rate stalled the paint path on big files).
            let groups = &self.groups;
            let overlay = || -> search::DirtyOverlay {
                std::sync::Arc::new(
                    groups
                        .iter()
                        .flat_map(|g| g.files.iter())
                        .filter(|f| f.dirty)
                        .map(|f| (f.path.clone(), f.buffer.rope().to_string()))
                        .collect(),
                )
            };
            self.search.ui_with_symbols(ctx, &files, &root, Some(&self.symbols), &overlay)
        } {
            self.open_file(path);
            if let Some(f) = self.groups[self.focused].active_file() {
                let rope = f.buffer.rope().clone();
                let byte = rope.line_to_byte(line.min(rope.len_lines().saturating_sub(1)));
                f.view.jump_to(byte, &rope);
            }
        }
        // Replace in Files: apply the confirmed plan — open buffers through the undo-safe
        // editor path (Ctrl+Z reverts, didChange flows via the per-frame drain), the rest
        // straight to disk.
        if let Some(plan) = self.search.take_confirmed_replacements() {
            let now = ctx.input(|i| i.time);
            let mut applied = 0usize;
            for (path, text) in plan {
                self.replace_file_text(&path, text, now);
                applied += 1;
            }
            self.lsp_message = Some(format!("replaced in {applied} file(s)"));
        }
        if self.settings_open {
            let mut open = self.settings_open;
            egui::Window::new("Settings")
                .open(&mut open)
                .resizable(true)
                .collapsible(false)
                .default_size([640.0, 420.0])
                .min_size([540.0, 340.0])
                .show(ctx, |ui| {
                    egui::SidePanel::left(ui.id().with("settings-nav"))
                        .exact_width(150.0)
                        .resizable(false)
                        .show_separator_line(true)
                        .show_inside(ui, |ui| {
                            ui.add_space(8.0);
                            ui.spacing_mut().item_spacing.y = 6.0;
                            for (tab, label) in [
                                (SettingsTab::Appearance, "Appearance"),
                                (SettingsTab::Editor, "Editor"),
                                (SettingsTab::Ai, "AI"),
                                (SettingsTab::Standards, "Standards"),
                                (SettingsTab::About, "About"),
                            ] {
                                let sel = self.settings_tab == tab;
                                if ui
                                    .add_sized(
                                        [ui.available_width(), 26.0],
                                        egui::SelectableLabel::new(sel, egui::RichText::new(label).size(13.0)),
                                    )
                                    .clicked_by(egui::PointerButton::Primary)
                                {
                                    self.settings_tab = tab;
                                }
                            }
                        });
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        // Breathing room between the nav rail and the content pane.
                        egui::Frame::none()
                            .inner_margin(egui::Margin { left: 16.0, right: 10.0, top: 10.0, bottom: 10.0 })
                            .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 12.0;
                        match self.settings_tab {
                            SettingsTab::Appearance => {
                                style::panel_header_inline(ui, "Appearance");
                                ui.horizontal(|ui| {
                                    ui.label("Global UI zoom");
                                    if ui.button("−").clicked_by(egui::PointerButton::Primary) {
                                        zoom(ctx, -0.1);
                                    }
                                    ui.label(format!("{:.0}%", ctx.zoom_factor() * 100.0));
                                    if ui.button("+").clicked_by(egui::PointerButton::Primary) {
                                        zoom(ctx, 0.1);
                                    }
                                    if ui.button("reset").clicked_by(egui::PointerButton::Primary) {
                                        ctx.set_zoom_factor(1.0);
                                    }
                                });
                                ui.colored_label(
                                    colors::TEXT_FAINT(),
                                    "Scales every panel and font. Editor-only zoom is Ctrl+± / Ctrl+scroll.",
                                );
                                ui.separator();
                                ui.checkbox(&mut self.project_open, "Project panel (Alt+1)");
                                ui.checkbox(&mut self.pins_open, "Right tool panel");
                            }
                            SettingsTab::Editor => {
                                style::panel_header_inline(ui, "Appearance");
                                let before = self.theme_choice;
                                ui.horizontal_wrapped(|ui| {
                                    ui.label("Theme");
                                    ui.radio_value(&mut self.theme_choice, settings::ThemeChoice::Dark, "Dark");
                                    ui.radio_value(&mut self.theme_choice, settings::ThemeChoice::Light, "Light");
                                    ui.radio_value(&mut self.theme_choice, settings::ThemeChoice::Midnight, "Midnight");
                                    ui.radio_value(&mut self.theme_choice, settings::ThemeChoice::Amber, "Amber");
                                    if systheme::is_runeos() {
                                        ui.radio_value(&mut self.theme_choice, settings::ThemeChoice::System, "System");
                                    }
                                });
                                if self.theme_choice == settings::ThemeChoice::System {
                                    ui.colored_label(
                                        colors::TEXT_FAINT(),
                                        "Follows the RuneOS light/dark setting automatically.",
                                    );
                                }
                                if before != self.theme_choice {
                                    self.apply_theme(ctx);
                                    self.save_settings();
                                }
                                ui.add_space(8.0);
                                style::hairline(ui);
                                ui.add_space(8.0);
                                style::panel_header_inline(ui, "Editor");
                                ui.horizontal(|ui| {
                                    ui.label("Font size");
                                    let mut f = self.editor_font;
                                    if ui.add(egui::Slider::new(&mut f, 8.0..=40.0).suffix(" px")).changed() {
                                        self.editor_font = f;
                                        for g in &mut self.groups {
                                            for file in &mut g.files {
                                                file.view.set_font_size(f);
                                            }
                                        }
                                    }
                                });
                                ui.colored_label(
                                    colors::TEXT_FAINT(),
                                    "Ctrl+scroll or Ctrl+± zooms live; Ctrl+0 resets to 14 px.",
                                );
                                ui.add_space(12.0);
                                style::hairline(ui);
                                ui.add_space(8.0);
                                {
                                    let mut on = self.inline_blame_enabled;
                                    if ui
                                        .checkbox(&mut on, "Inline git blame (author · commit on the caret line)")
                                        .on_hover_text("Also in the command palette: Toggle Inline Blame")
                                        .changed()
                                    {
                                        self.inline_blame_enabled = on;
                                        if !on {
                                            if let Some(f) = self.groups[self.focused].active_file() {
                                                f.view.set_inline_blame(None);
                                            }
                                        }
                                        self.save_settings();
                                    }
                                }
                                if ui
                                    .checkbox(&mut self.inlay_hints_on, "Inlay hints (types, parameter names)")
                                    .on_hover_text("Language-server hints painted after each line's code")
                                    .changed()
                                {
                                    if !self.inlay_hints_on {
                                        for g in &mut self.groups {
                                            for file in &mut g.files {
                                                file.view.set_inlay_hints(Vec::new());
                                            }
                                        }
                                    }
                                    self.inlay_for = None;
                                    self.inlay_requested = None;
                                    self.save_settings();
                                }
                                if ui
                                    .checkbox(
                                        &mut self.auto_deps,
                                        "Auto-install dependencies on open",
                                    )
                                    .on_hover_text(
                                        "Resolve cargo / npm / NuGet / pip / … in the background \
                                         when a project opens. Runs package-manager install hooks \
                                         from the opened tree. Run ▸ Install dependencies triggers \
                                         it by hand.",
                                    )
                                    .changed()
                                {
                                    self.save_settings();
                                }
                            }
                            SettingsTab::Ai => {
                                style::panel_header_inline(ui, "AI");
                                let mut on = self.ai.enabled;
                                if ui
                                    .add_enabled(self.ai.available, egui::Checkbox::new(&mut on, "Inline completions (ghost text)"))
                                    .changed()
                                {
                                    self.ai.enabled = on;
                                }
                                ui.separator();
                                ui.colored_label(colors::TEXT_FAINT(), "Provider");
                                let before = self.ai_settings.clone();
                                ui.radio_value(
                                    &mut self.ai_settings.provider,
                                    settings::AiProvider::Claude,
                                    "Claude (cloud) — Claude Code sign-in, counts against plan limits",
                                );
                                ui.radio_value(
                                    &mut self.ai_settings.provider,
                                    settings::AiProvider::Ollama,
                                    "Local (Ollama) — fully offline, no sign-in",
                                );
                                match self.ai_settings.provider {
                                    settings::AiProvider::Claude => {
                                        egui::Grid::new("ai-models").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
                                            ui.colored_label(colors::TEXT_FAINT(), "Ghost completions");
                                            ui.label("Claude Haiku (fast tier)");
                                            ui.end_row();
                                            ui.colored_label(colors::TEXT_FAINT(), "Assistant panel");
                                            ui.label("Claude Sonnet (quality tier)");
                                            ui.end_row();
                                        });
                                        if !self.ai.available {
                                            ui.colored_label(
                                                colors::WARN(),
                                                "No Claude sign-in found (~/.claude/.credentials.json). Sign into Claude Code to enable.",
                                            );
                                        }
                                    }
                                    settings::AiProvider::Ollama => {
                                        egui::Grid::new("ai-ollama").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
                                            ui.colored_label(colors::TEXT_FAINT(), "Server");
                                            ui.add(
                                                egui::TextEdit::singleline(&mut self.ai_settings.ollama_url)
                                                    .id(egui::Id::new("ai-ollama-url"))
                                                    .desired_width(240.0),
                                            );
                                            ui.end_row();
                                            ui.colored_label(colors::TEXT_FAINT(), "Ghost completions");
                                            ui.add(
                                                egui::TextEdit::singleline(&mut self.ai_settings.ollama_fim_model)
                                                    .id(egui::Id::new("ai-ollama-fim"))
                                                    .desired_width(240.0),
                                            );
                                            ui.end_row();
                                            ui.colored_label(colors::TEXT_FAINT(), "Assistant panel");
                                            ui.add(
                                                egui::TextEdit::singleline(&mut self.ai_settings.ollama_chat_model)
                                                    .id(egui::Id::new("ai-ollama-chat"))
                                                    .desired_width(240.0),
                                            );
                                            ui.end_row();
                                        });
                                        ui.colored_label(
                                            colors::TEXT_FAINT(),
                                            "Ghost model needs fill-in-the-middle (a *base* model: qwen2.5-coder:1.5b-base, \
                                             starcoder2, codellama:code). Pull with `ollama pull <model>`.",
                                        );
                                        if !self.ai.available {
                                            ui.colored_label(
                                                colors::WARN(),
                                                "Ollama server not reachable — start it with `ollama serve`.",
                                            );
                                        }
                                    }
                                }
                                if before != self.ai_settings {
                                    ai::set_config(&self.ai_settings);
                                    // Re-probe only on provider flips: the URL probe can block
                                    // up to 2s on an unreachable remote host, which per-keystroke
                                    // (this branch fires on every typed char) would freeze the UI.
                                    // URL edits get picked up by the button below.
                                    if before.provider != self.ai_settings.provider {
                                        self.ai.refresh_available();
                                        self.ai_panel.available = self.ai.available;
                                    }
                                    self.save_settings();
                                }
                                if ui.button("Test connection").clicked_by(egui::PointerButton::Primary) {
                                    self.ai.refresh_available();
                                    self.ai_panel.available = self.ai.available;
                                }
                            }
                            SettingsTab::Standards => {
                                style::panel_header_inline(ui, "Coding standards");
                                let before = self.standards;
                                ui.radio_value(&mut self.standards, Standards::Off, "Off — LSP diagnostics only");
                                ui.radio_value(
                                    &mut self.standards,
                                    Standards::Gsfc,
                                    "GSFC 582 / cFS conventions — what cFE reviewers gate",
                                );
                                ui.radio_value(
                                    &mut self.standards,
                                    Standards::JplPot,
                                    "JPL Power of Ten — strict Rule-1 errors",
                                );
                                if before != self.standards {
                                    if self.standards != Standards::Off {
                                        self.psi_scan_pending = true;
                                    }
                                    self.refresh_nasa_squiggles();
                                }
                                ui.separator();
                                ui.colored_label(
                                    colors::TEXT_FAINT(),
                                    "Recursion cycles bounded by a recognized re-entry guard show in orchid \
                                     (🛡) instead of alarm colors. Format drift is checked only on files \
                                     you have changed — mirroring cFE CI.",
                                );
                            }
                            SettingsTab::About => {
                                style::panel_header_inline(ui, "Cauldron");
                                ui.label(format!("version {}", env!("CARGO_PKG_VERSION")));
                                ui.colored_label(colors::TEXT_FAINT(), "Rune-native IDE — egui + wgpu, no web runtime.");
                                ui.separator();
                                egui::Grid::new("about-paths").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
                                    ui.colored_label(colors::TEXT_FAINT(), "Claude budgets");
                                    ui.label("~/.config/cauldron/claude-budget.toml");
                                    ui.end_row();
                                    ui.colored_label(colors::TEXT_FAINT(), "Run configs");
                                    ui.label("<workspace>/.cauldron/runconfigs.json");
                                    ui.end_row();
                                    ui.colored_label(colors::TEXT_FAINT(), "Local history");
                                    ui.label("~/.local/share/cauldron/history/");
                                    ui.end_row();
                                    ui.colored_label(colors::TEXT_FAINT(), "Terminal theme");
                                    ui.label("cider's own config");
                                    ui.end_row();
                                });
                            }
                        }
                            });
                    });  
                });
            self.settings_open = open;
        }
        if let Some(action) = tree_action {
            self.handle_tree_action(action);
        }
    }

    fn on_exit(&mut self) {
        self.save_session();
        self.save_settings();
        self.runner.stop();
        self.lsp.shutdown_all();
    }
}

/// The empty-group state: a quiet occult/space touch + the keys that matter.
fn empty_state(ui: &mut egui::Ui) {
    let rect = ui.available_rect_before_wrap();
    let p = ui.painter();
    // A faint constellation: fixed pseudo-stars (deterministic — no RNG needed).
    let stars = [
        (0.18, 0.22, 1.4),
        (0.32, 0.11, 0.9),
        (0.47, 0.31, 1.1),
        (0.63, 0.18, 0.8),
        (0.74, 0.36, 1.5),
        (0.85, 0.14, 0.9),
        (0.26, 0.55, 1.0),
        (0.55, 0.62, 1.3),
        (0.71, 0.71, 0.8),
        (0.41, 0.79, 1.1),
        (0.87, 0.63, 1.0),
        (0.14, 0.76, 0.9),
    ];
    for (fx, fy, r) in stars {
        let pos =
            egui::Pos2::new(rect.min.x + rect.width() * fx, rect.min.y + rect.height() * fy);
        p.circle_filled(pos, r, egui::Color32::from_rgba_premultiplied(28, 26, 24, 28));
    }
    ui.centered_and_justified(|ui| {
        ui.vertical_centered(|ui| {
            ui.label(
                egui::RichText::new("CAULDRON")
                    .size(22.0)
                    .color(colors::TEXT_FAINT())
                    .extra_letter_spacing(4.0),
            );
            ui.colored_label(colors::TEXT_FAINT(), "mission ready");
            ui.add_space(12.0);
            for line in [
                "Ctrl+P  go to file        Ctrl+O  open file",
                "Ctrl+N  new file          Ctrl+Shift+N  new project",
                "Alt+F12 terminal          Ctrl+J  problems",
                "Ctrl+\\  split right       Alt+1  project panel",
            ] {
                ui.label(
                    egui::RichText::new(line).monospace().size(11.5).color(colors::TEXT_FAINT()),
                );
            }
        });
    });
}

fn show_rel(rel: &Path) -> String {
    if rel.as_os_str().is_empty() {
        ".".into()
    } else {
        rel.display().to_string()
    }
}

/// Split the "[source] message" convention into aligned columns.
fn split_source(msg: &str) -> (String, String) {
    if let Some(rest) = msg.strip_prefix('[') {
        if let Some(end) = rest.find("] ") {
            return (rest[..end].to_string(), rest[end + 2..].to_string());
        }
    }
    (String::new(), msg.to_string())
}

/// Last known pointer position — captured via a thread-local set each frame would be overkill;
/// egui exposes it through the context the popup is drawn with, so we stash it here instead.
static POINTER_POS: Mutex<Option<(f32, f32)>> = Mutex::new(None);

/// Which heading a code action sits under in the menu, from its `CodeActionKind`. LSP kinds are
/// dotted and open-ended (`refactor.extract.function`), so match by prefix and let anything
/// unrecognized fall into "Other".
fn action_group(action: &lsp_types::CodeActionOrCommand) -> &'static str {
    let kind = match action {
        // A bare Command carries no kind at all — servers send these for the odd action that
        // predates code-action literals.
        lsp_types::CodeActionOrCommand::Command(_) => return "Other",
        lsp_types::CodeActionOrCommand::CodeAction(a) => {
            a.kind.as_ref().map(|k| k.as_str().to_string()).unwrap_or_default()
        }
    };
    match () {
        _ if kind.starts_with("refactor.extract") => "Extract",
        _ if kind.starts_with("refactor.inline") => "Inline",
        _ if kind.starts_with("refactor.move") => "Move",
        _ if kind.starts_with("refactor.rewrite") => "Rewrite",
        _ if kind.starts_with("refactor") => "Refactor",
        _ if kind.starts_with("source") => "Source",
        _ if kind.starts_with("quickfix") => "Quick Fix",
        _ => "Other",
    }
}

/// Rank for [`action_group`] headings, so groups appear in a stable, useful order rather than
/// whatever sequence the server emitted.
fn action_group_rank(group: &str) -> u8 {
    match group {
        "Extract" => 0,
        "Inline" => 1,
        "Move" => 2,
        "Rewrite" => 3,
        "Refactor" => 4,
        "Quick Fix" => 5,
        "Source" => 6,
        _ => 7,
    }
}

/// Group actions by kind for display. Sort is STABLE, so within a group the server's own
/// ordering (which encodes its relevance ranking) is preserved.
fn sort_actions_by_kind(
    mut actions: Vec<lsp_types::CodeActionOrCommand>,
) -> Vec<lsp_types::CodeActionOrCommand> {
    actions.sort_by_key(|a| action_group_rank(action_group(a)));
    actions
}

fn ctx_pointer_pos() -> Option<egui::Pos2> {
    POINTER_POS.lock().unwrap().map(|(x, y)| egui::Pos2::new(x, y))
}

/// The per-frame drain gate for the EVENT-driven symbol-index rebuild (recon B2): fires — and
/// disarms `pending` — exactly once per arm, and never while a build stream is inflight (the
/// arm survives until the stream finishes, then fires once). Arms are explicit events only:
/// build-on-open (App construction), `open_folder`, and the watcher's FullRescan/fallback lanes.
///
/// REGRESSION GUARD: the retired kick (`symbols.is_empty() && !is_building()`) respawned a full
/// rebuild thread EVERY FRAME forever on any legitimately-zero-symbol project — emptiness is a
/// valid steady state and must never re-arm anything. Nothing here (or in any arm site) may
/// consult `is_empty()`.
fn take_symbol_rebuild_kick(pending: &mut bool, building: bool) -> bool {
    let fire = *pending && !building;
    if fire {
        *pending = false;
    }
    fire
}

#[cfg(test)]
mod debug_value_tests {
    use super::*;

    /// Whole-word matching for inline debug values: `x` must not match inside `max` or `xs`.
    #[test]
    fn word_appears_respects_identifier_boundaries() {
        assert!(word_appears("let x = 1;", "x"));
        assert!(word_appears("return x + y;", "x"));
        assert!(!word_appears("let max = 1;", "x"));
        assert!(!word_appears("let xs = [];", "x"));
        assert!(word_appears("count += 1", "count"));
        assert!(!word_appears("counter = 0", "count"));
        assert!(word_appears("obj.field", "field"));
        assert!(!word_appears("", "x"));
    }

    #[test]
    fn truncate_val_shortens_long_and_multiline() {
        assert_eq!(truncate_val("42"), "42");
        assert_eq!(truncate_val("line1\nline2"), "line1…");
        assert!(truncate_val(&"a".repeat(100)).ends_with('…'));
    }
}

#[cfg(test)]
mod completion_rank_tests {
    use super::*;

    fn item(label: &str, sort: Option<&str>) -> lsp_types::CompletionItem {
        lsp_types::CompletionItem {
            label: label.to_string(),
            sort_text: sort.map(str::to_string),
            ..Default::default()
        }
    }

    /// Exact beats prefix beats fuzzy; non-matches drop; server order no longer wins.
    #[test]
    fn ranks_match_quality_over_server_order() {
        let items = vec![
            item("read_to_string", None), // prefix of "read"? yes → prefix tier
            item("BufReader", None),      // fuzzy only ("read" ⊄ prefix; r-e-a-d in order? B-u-f-R-e-a-d → yes)
            item("write", None),          // no match → dropped
            item("read", None),           // exact → first despite arriving after
        ];
        let ranked = rank_completions(&items, "read");
        assert_eq!(ranked[0], 3, "exact match first");
        assert_eq!(ranked[1], 0, "prefix second");
        assert_eq!(ranked[2], 1, "fuzzy last");
        assert_eq!(ranked.len(), 3, "non-match dropped");
    }

    /// UTF-16 offsets land on byte boundaries even through multibyte chars, clamped at end.
    #[test]
    fn utf16_offset_conversion() {
        // "fn é(α: β)" — é is 1 UTF-16 unit / 2 bytes; α,β likewise.
        let s = "fn é(α: β)";
        assert_eq!(utf16_off_to_byte(s, 0), 0);
        assert_eq!(utf16_off_to_byte(s, 3), 3); // start of é
        assert_eq!(utf16_off_to_byte(s, 4), 5); // after é (2 bytes)
        assert_eq!(utf16_off_to_byte(s, 5), 6); // start of α
        assert_eq!(utf16_off_to_byte(s, 999), s.len()); // clamp
        // Surrogate pair: 𝕏 is 2 UTF-16 units / 4 bytes.
        let t = "a𝕏b";
        assert_eq!(utf16_off_to_byte(t, 1), 1);
        assert_eq!(utf16_off_to_byte(t, 3), 5); // past both surrogate halves
    }

    /// Empty prefix: everything shows, ordered by the server's sortText relevance.
    #[test]
    fn empty_prefix_uses_sort_text() {
        let items = vec![item("zeta", Some("b")), item("alpha", Some("c")), item("local", Some("a"))];
        let ranked = rank_completions(&items, "");
        assert_eq!(ranked, vec![2, 0, 1]);
    }

    /// Case-sensitive exact/prefix outranks the insensitive tier; fuzzy respects order.
    #[test]
    fn case_and_subsequence_tiers() {
        assert_eq!(completion_match_score("Read", "Read", "read"), Some(100));
        assert_eq!(completion_match_score("read", "Read", "read"), Some(90));
        assert_eq!(completion_match_score("Reader", "Read", "read"), Some(80));
        assert_eq!(completion_match_score("reader", "Read", "read"), Some(70));
        assert_eq!(completion_match_score("read_dir", "rdir", "rdir"), Some(40));
        assert_eq!(completion_match_score("read_dir", "rdd", "rdd"), Some(40), "r-d-d in order");
        assert_eq!(completion_match_score("read_dir", "xyz", "xyz"), None);
        assert_eq!(completion_match_score("read_dir", "ird", "ird"), None, "out of order");
    }
}

#[cfg(test)]
mod symbol_rekick_tests {
    use super::*;
    use crate::symbols::SymbolIndex;
    use std::time::{Duration, Instant};

    /// Pure gate semantics: an arm is HELD (not consumed) while a build stream is inflight,
    /// fires exactly once when the stream is idle, and never fires unarmed.
    #[test]
    fn kick_gate_fires_exactly_once_per_arm_and_holds_while_building() {
        let mut pending = false;
        assert!(!take_symbol_rebuild_kick(&mut pending, false), "fired without an arm");

        pending = true;
        assert!(!take_symbol_rebuild_kick(&mut pending, true), "fired mid-build");
        assert!(pending, "arm was consumed while the build was inflight");

        assert!(take_symbol_rebuild_kick(&mut pending, false), "armed + idle must fire");
        assert!(!pending, "firing must disarm");
        assert!(!take_symbol_rebuild_kick(&mut pending, false), "second fire from one arm");
    }

    /// Recon B2 regression: the retired gate (`symbols.is_empty() && !is_building()`) respawned
    /// a full rebuild thread EVERY FRAME forever on a legitimately-zero-symbol project. Drive
    /// the production drain (poll + take_symbol_rebuild_kick + rebuild) through hundreds of
    /// simulated frames against a real zero-symbol universe: exactly ONE build per arm.
    #[test]
    fn zero_symbol_project_rebuilds_once_per_arm_not_per_frame() {
        let dir = std::env::temp_dir().join(format!("cauldron-rekick-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A real universe member that legitimately yields zero symbols.
        std::fs::write(dir.join("empty.rs"), "// nothing declared here\n").unwrap();
        let universe = vec![dir.join("empty.rs")];

        let ctx = egui::Context::default();
        let mut idx = SymbolIndex::default();
        let mut pending = true; // the build-on-open arm (App construction)
        let mut builds = 0usize;

        // One "frame" of the update() drain; returns whether a build fired.
        let frame = |pending: &mut bool, idx: &mut SymbolIndex, builds: &mut usize| {
            idx.poll();
            if take_symbol_rebuild_kick(pending, idx.is_building()) {
                *builds += 1;
                idx.rebuild(&universe, &ctx);
            }
        };

        // Frames until the first (and only) build's stream completes.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            frame(&mut pending, &mut idx, &mut builds);
            if builds >= 1 && !idx.is_building() {
                break;
            }
            assert!(Instant::now() < deadline, "zero-symbol build never completed");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(builds, 1);
        assert!(idx.is_empty(), "the fixture project must really have zero symbols");

        // THE BUG: emptiness is a valid steady state — hundreds of idle frames, zero respawns.
        for _ in 0..300 {
            frame(&mut pending, &mut idx, &mut builds);
        }
        assert_eq!(builds, 1, "empty index re-kicked a rebuild (recon B2 regression)");

        // An explicit invalidation (open_folder / watcher FullRescan) arms once → ONE more.
        pending = true;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            frame(&mut pending, &mut idx, &mut builds);
            if builds >= 2 && !idx.is_building() {
                break;
            }
            assert!(Instant::now() < deadline, "re-armed build never completed");
            std::thread::sleep(Duration::from_millis(2));
        }
        for _ in 0..300 {
            frame(&mut pending, &mut idx, &mut builds);
        }
        assert_eq!(builds, 2, "one invalidation must produce exactly one build");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod split_tests {
    use super::*;

    #[test]
    fn drop_in_the_body_maps_position_to_move_or_split() {
        // Three panes spanning 0..300. `false` = pointer in the editor BODY (not the strip).
        let spans = [(0.0, 100.0), (100.0, 200.0), (200.0, 300.0)];
        // Middle of a pane → move into it.
        assert_eq!(resolve_drop(&spans, 50.0, false), Some((DropKind::Move, 0)));
        assert_eq!(resolve_drop(&spans, 250.0, false), Some((DropKind::Move, 2)));
        // Outer quarters → split that side of the pane.
        assert_eq!(resolve_drop(&spans, 10.0, false), Some((DropKind::SplitLeft, 0)));
        assert_eq!(resolve_drop(&spans, 95.0, false), Some((DropKind::SplitRight, 0)));
        assert_eq!(resolve_drop(&spans, 205.0, false), Some((DropKind::SplitLeft, 2)));
        // Past the far edges → split at that end.
        assert_eq!(resolve_drop(&spans, -20.0, false), Some((DropKind::SplitLeft, 0)));
        assert_eq!(resolve_drop(&spans, 400.0, false), Some((DropKind::SplitRight, 2)));
        assert_eq!(resolve_drop(&[], 50.0, false), None);
    }

    #[test]
    fn drop_over_the_tab_strip_is_always_a_move_never_a_split() {
        // `true` = pointer over the tab strip → reorder/move, so even the outer quarters and past
        // the far edges resolve to Move (into the nearest pane), never a split. This is the fix for
        // "reordering an edge tab spawns a split".
        let spans = [(0.0, 100.0), (100.0, 200.0)];
        assert_eq!(resolve_drop(&spans, 5.0, true), Some((DropKind::Move, 0)), "left quarter, strip");
        assert_eq!(resolve_drop(&spans, 98.0, true), Some((DropKind::Move, 0)), "right quarter, strip");
        assert_eq!(resolve_drop(&spans, 150.0, true), Some((DropKind::Move, 1)), "other pane strip");
        assert_eq!(resolve_drop(&spans, -30.0, true), Some((DropKind::Move, 0)), "past left → pane 0");
        assert_eq!(resolve_drop(&spans, 500.0, true), Some((DropKind::Move, 1)), "past right → last");
    }

    #[test]
    fn split_opens_new_panes_until_the_cap_then_shuffles() {
        // From one pane, split opens a second to the right, then a third.
        assert_eq!(split_destination(1, 0), (true, 1), "1 pane → open pane 1");
        assert_eq!(split_destination(2, 0), (true, 1), "2 panes, from 0 → open at 1");
        assert_eq!(split_destination(2, 1), (true, 2), "2 panes, from 1 → open at 2");
        // At the cap (MAX_GROUPS), no new pane — the tab moves to the next pane, wrapping.
        assert_eq!(split_destination(MAX_GROUPS, 0), (false, 1));
        assert_eq!(split_destination(MAX_GROUPS, 1), (false, 2));
        assert_eq!(
            split_destination(MAX_GROUPS, MAX_GROUPS - 1),
            (false, 0),
            "last pane wraps to the first"
        );
    }
}

#[cfg(test)]
mod shell_cwd_tests {
    use super::*;

    /// A shell opens at the project root, or at `~` — never anywhere else, and never at a path
    /// that does not exist (which would not degrade, it would fail the PTY spawn outright).
    #[test]
    fn shell_opens_at_project_root_or_home_and_never_at_a_missing_dir() {
        let tmp = std::env::temp_dir().join(format!("cauldron-shellcwd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let proj = tmp.join("proj");
        let home = tmp.join("home");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        // The no-project sentinel: an absolute path that by design is never created.
        let sentinel = tmp.join("never-created");
        assert!(!sentinel.exists(), "sentinel must not exist for this test to mean anything");

        // A real project: the shell opens THERE.
        assert_eq!(shell_cwd(false, &proj, Some(&home)), proj);
        // No-project mode: `~`, never the sentinel root it carries.
        assert_eq!(shell_cwd(true, &sentinel, Some(&home)), home);
        // Even flagged as a project, a root that isn't a real dir (renamed/deleted under a live
        // session) must fall back to `~` rather than spawn at a path that isn't there.
        assert_eq!(shell_cwd(false, &sentinel, Some(&home)), home);
        // A file is not a directory.
        let file = tmp.join("a-file");
        std::fs::write(&file, "x").unwrap();
        assert_eq!(shell_cwd(false, &file, Some(&home)), home);
        // No usable $HOME at all: `/`, the one directory guaranteed to exist.
        assert_eq!(shell_cwd(true, &sentinel, None), PathBuf::from("/"));
        assert_eq!(shell_cwd(true, &sentinel, Some(&sentinel)), PathBuf::from("/"), "missing home");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}

#[cfg(test)]
mod lazy_restore_tests {
    use super::*;

    /// The batched restore diff splits per `+++ b/<path>` header and parses each file's hunks
    /// with the same rules as the single-file path; deleted files (`+++ /dev/null`) and noise
    /// lines contribute nothing.
    #[test]
    fn batched_diff_splits_per_file() {
        let diff = "\
diff --git a/src/main.rs b/src/main.rs
index 1111111..2222222 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -10,0 +11,2 @@ fn main() {
+let a = 1;
+let b = 2;
@@ -5,1 +7,1 @@ adversarial content
-- x
+++ b/evil
@@ -20,3 +23,0 @@ fn other() {
-gone
-gone
-gone
diff --git a/docs/read me.md b/docs/read me.md\t
index 3333333..4444444 100644
--- a/docs/read me.md\t
+++ b/docs/read me.md\t
@@ -1,1 +1,1 @@
-old
+new
diff --git a/dead.txt b/dead.txt
deleted file mode 100644
index 5555555..0000000
--- a/dead.txt
+++ /dev/null
@@ -1,4 +0,0 @@
-x
";
        let per_file = parse_diff_batch(diff);
        // main.rs: lines 11-12 added, line 7 modified (the adversarial hunk's real edit),
        // deletion boundary below new line 23 → 22. The added CONTENT line `+++ b/evil` does
        // not follow a `---` header, so it must not hijack the per-file split.
        assert_eq!(
            per_file.get("src/main.rs"),
            Some(&vec![(10, 0), (11, 0), (6, 1), (22, 2)]),
            "hunks must parse exactly like the single-file path"
        );
        assert!(!per_file.contains_key("evil"), "content lines must never open a new file");
        // Trailing-tab convention for paths with spaces is stripped.
        assert_eq!(per_file.get("docs/read me.md"), Some(&vec![(0, 1)]));
        // The deleted file's hunk must NOT leak into any entry.
        assert!(!per_file.contains_key("dead.txt"));
        assert!(!per_file.contains_key("/dev/null"));
        assert_eq!(per_file.len(), 2);
        // Batch splitting and the single-file parser agree on the same content.
        let single = "@@ -10,0 +11,2 @@\n@@ -20,3 +23,0 @@\n";
        assert_eq!(parse_diff_gutter(single), vec![(10, 0), (11, 0), (22, 2)]);
    }

    /// An untracked / outside-repo file simply never appears in the diff: the batch result
    /// holds no entry, and the caller's unwrap_or_default gives it EMPTY marks (the exact
    /// behavior of the old per-file subprocess on a failed/empty diff).
    #[test]
    fn batched_diff_absent_file_means_empty_marks() {
        let per_file = parse_diff_batch("");
        assert!(per_file.get("not/in/diff.rs").is_none());
    }

    /// The `prev_was_old_header` guard alone was hijackable: a DELETED content line whose
    /// content starts with `-- ` renders `--- …` and arms the flag, and an ADDED line whose
    /// content starts with `++ b/…` renders `+++ b/…` right after it (a Lua/SQL comment edit,
    /// a checked-in .patch). Hunk-length body consumption makes the pair structurally opaque.
    #[test]
    fn batched_diff_hunk_body_cannot_forge_file_headers() {
        let diff = "\
diff --git a/notes.lua b/notes.lua
index 1111111..2222222 100644
--- a/notes.lua
+++ b/notes.lua
@@ -5,1 +5,1 @@
--- old comment
+++ b/phantom
@@ -30,0 +31,1 @@
+more
";
        let per_file = parse_diff_batch(diff);
        assert!(
            !per_file.contains_key("phantom"),
            "forged header inside a hunk body must not open a file: {per_file:?}"
        );
        // BOTH hunks stay attributed to the real file (the old code lost every hunk after
        // the hijack point to the phantom).
        assert_eq!(per_file.get("notes.lua"), Some(&vec![(4, 1), (30, 0)]));
        assert_eq!(per_file.len(), 1);

        // `\ No newline at end of file` markers are not counted in hunk lengths — the body
        // skip must not let one eat a real content line's slot.
        let diff = "\
--- a/x.txt
+++ b/x.txt
@@ -3,1 +3,1 @@
-old
\\ No newline at end of file
+new
\\ No newline at end of file
--- a/y.txt
+++ b/y.txt
@@ -1,0 +2,1 @@
+added
";
        let per_file = parse_diff_batch(diff);
        assert_eq!(per_file.get("x.txt"), Some(&vec![(2, 1)]));
        assert_eq!(per_file.get("y.txt"), Some(&vec![(1, 0)]), "next file header still found");

        // Malformed hunk headers parse to no marks and arm no skip (graceful degradation).
        assert!(hunk_body_lines("@@ garbage @@").is_none());
        assert_eq!(hunk_body_lines("@@ -5,2 +7,3 @@ ctx"), Some(5));
        assert_eq!(hunk_body_lines("@@ -5 +7 @@"), Some(2), "count omitted = 1");
    }

    /// End-to-end against REAL git: a project rooted in a SUBDIRECTORY of its repo (the
    /// monorepo-crate case) must key the batch diff by workspace-root-relative paths, and
    /// user gitconfig (`diff.noprefix`, `diff.mnemonicPrefix`) must not zero the split.
    /// Without `--relative`, git emits `+++ b/sub/src/a.txt` (repo-root-relative) while the
    /// lookup key is `src/a.txt` → every restored tab silently got empty marks.
    #[test]
    fn batch_gutter_diff_subdir_root_and_prefix_configs() {
        if std::process::Command::new("git").arg("--version").output().is_err() {
            eprintln!("git unavailable — skipping");
            return;
        }
        let repo = std::env::temp_dir()
            .join(format!("cauldron-gutterbatch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(repo.join("sub/src")).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .expect("run git");
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(repo.join("sub/src/a.txt"), "a\nb\nc\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "init"]);
        // Modify line 2 + append line 4 → marks (1, modified) and (3, added).
        std::fs::write(repo.join("sub/src/a.txt"), "a\nX\nc\nd\n").unwrap();

        let root = repo.join("sub"); // workspace root = subdir of the repo
        let rels = vec![PathBuf::from("src/a.txt")];
        let per_file = batch_gutter_diff(&root, &rels);
        assert_eq!(
            per_file.get("src/a.txt"),
            Some(&vec![(1, 1), (3, 0)]),
            "keys must be workspace-root-relative, not repo-root-relative: {per_file:?}"
        );

        // Hostile user config: repo-local config is honored exactly like ~/.gitconfig.
        git(&["config", "diff.noprefix", "true"]);
        git(&["config", "diff.mnemonicPrefix", "true"]);
        let per_file = batch_gutter_diff(&root, &rels);
        assert_eq!(
            per_file.get("src/a.txt"),
            Some(&vec![(1, 1), (3, 0)]),
            "diff.noprefix/mnemonicPrefix must not zero the batch split: {per_file:?}"
        );

        let _ = std::fs::remove_dir_all(&repo);
    }

    /// Lazy tabs (session restore): the placeholder renders chrome from the path only; first
    /// activation hydrates the real content and applies the PARKED session caret.
    #[test]
    fn lazy_tab_hydrates_and_applies_pending_caret() {
        let dir = std::env::temp_dir().join(format!("cauldron-lazy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("demo.rs");
        let text = "fn main() {\n    println!(\"hi\");\n}\n";
        std::fs::write(&path, text).unwrap();

        let mut f = OpenFile::lazy(path.clone());
        assert!(!f.loaded);
        assert_eq!(f.name(), "demo.rs", "chrome (tab title) works from the path alone");
        assert_eq!(f.lang, Some(Lang::Rust), "real lang is kept for LSP/C-deps decisions");
        assert!(!f.dirty, "a placeholder must never be saveable");
        assert_eq!(f.buffer.rope().len_bytes(), 0, "no file read before activation");

        let caret = 14; // inside line 2
        f.hydrate(Some(caret)).expect("hydrate reads the real file");
        assert!(f.loaded);
        assert_eq!(f.buffer.rope().to_string(), text);
        assert_eq!(f.view.caret_byte(), caret, "parked session caret applied on activation");

        // Hydrating again is a no-op (keeps buffer + caret).
        f.hydrate(Some(0)).unwrap();
        assert_eq!(f.view.caret_byte(), caret);

        // Out-of-range parked caret (file shrank between sessions) clamps.
        let mut g = OpenFile::lazy(path.clone());
        g.hydrate(Some(10_000)).unwrap();
        assert_eq!(g.view.caret_byte(), text.len());

        // A vanished file reports the error instead of fabricating an empty buffer.
        let mut gone = OpenFile::lazy(dir.join("gone.rs"));
        assert!(gone.hydrate(None).is_err());
        assert!(!gone.loaded);

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod boot_root_tests {
    use super::*;

    const HOME: &str = "/home/user";
    fn home() -> Option<&'static Path> {
        Some(Path::new(HOME))
    }
    fn proj(root: &str) -> RootChoice {
        RootChoice::Project { root: PathBuf::from(root), initial_file: None }
    }

    #[test]
    fn arg_dir_keeps_historic_behavior_even_for_home() {
        let arg = |p: &str| Some((PathBuf::from(p), true));
        assert_eq!(choose_root(arg("/x/proj"), None, home(), None), proj("/x/proj"));
        // Explicit `cauldron ~` is the user insisting — honored (but never recorded as
        // recent/last: record_recent + save_last_project reject it).
        assert_eq!(choose_root(arg(HOME), Some(Path::new(HOME)), home(), None), proj(HOME));
    }

    #[test]
    fn arg_file_opens_parent_as_project() {
        let f = PathBuf::from("/x/proj/src/main.rs");
        assert_eq!(
            choose_root(Some((f.clone(), false)), Some(Path::new(HOME)), home(), None),
            RootChoice::Project { root: PathBuf::from("/x/proj/src"), initial_file: Some(f) }
        );
        // Bare filename (empty parent) → cwd, exactly like the historic code path…
        let bare = PathBuf::from("notes.md");
        assert_eq!(
            choose_root(Some((bare.clone(), false)), Some(Path::new("/x/here")), home(), None),
            RootChoice::Project { root: PathBuf::from("/x/here"), initial_file: Some(bare.clone()) }
        );
        // …and the historic "." fallback when even cwd is unreadable.
        assert_eq!(
            choose_root(Some((bare.clone(), false)), None, home(), None),
            RootChoice::Project { root: PathBuf::from("."), initial_file: Some(bare) }
        );
    }

    #[test]
    fn no_arg_project_cwd_wins_over_pointer() {
        // `cd proj && cauldron` still opens THAT project, pointer or not.
        let last = PathBuf::from("/x/other");
        assert_eq!(
            choose_root(None, Some(Path::new("/x/proj")), home(), Some(&last)),
            proj("/x/proj")
        );
        assert_eq!(choose_root(None, Some(Path::new("/x/proj")), home(), None), proj("/x/proj"));
    }

    #[test]
    fn no_arg_unworthy_cwd_falls_back_to_last_project() {
        let last = PathBuf::from("/home/user/RustroverProjects/cauldron");
        for cwd in [HOME, "/", "/home"] {
            // Dock launch (cwd = $HOME), / and ancestors of $HOME: never opened as projects.
            assert_eq!(
                choose_root(None, Some(Path::new(cwd)), home(), Some(&last)),
                proj("/home/user/RustroverProjects/cauldron"),
                "cwd {cwd} must yield the last project"
            );
        }
        // Unreadable cwd (current_dir failed) → same fallback.
        assert_eq!(
            choose_root(None, None, home(), Some(&last)),
            proj("/home/user/RustroverProjects/cauldron")
        );
    }

    #[test]
    fn no_arg_nothing_available_is_no_project() {
        assert_eq!(choose_root(None, Some(Path::new(HOME)), home(), None), RootChoice::NoProject);
        assert_eq!(choose_root(None, Some(Path::new("/")), home(), None), RootChoice::NoProject);
        assert_eq!(choose_root(None, None, home(), None), RootChoice::NoProject);
        // No HOME either: "/" is still rejected, a real dir still accepted.
        assert_eq!(choose_root(None, Some(Path::new("/")), None, None), RootChoice::NoProject);
        assert_eq!(choose_root(None, Some(Path::new("/srv/p")), None, None), proj("/srv/p"));
    }
}

/// Flatten LSP hover contents to displayable text (code fences stripped, plain markdown kept).
fn hover_text(h: &lsp_types::Hover) -> Option<String> {
    fn marked(ms: &lsp_types::MarkedString) -> String {
        match ms {
            lsp_types::MarkedString::String(s) => s.clone(),
            lsp_types::MarkedString::LanguageString(l) => l.value.clone(),
        }
    }
    let raw = match &h.contents {
        lsp_types::HoverContents::Scalar(ms) => marked(ms),
        lsp_types::HoverContents::Array(a) => {
            a.iter().map(marked).collect::<Vec<_>>().join("\n\n")
        }
        lsp_types::HoverContents::Markup(m) => m.value.clone(),
    };
    let cleaned: String = raw
        .lines()
        .filter(|l| !l.trim_start().starts_with("```"))
        .collect::<Vec<_>>()
        .join("\n");
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.chars().take(1600).collect())
    }
}

/// Parse one `git diff -U0` hunk header into gutter marks: `@@ -a,b +c,d @@` — d>0 → lines
/// c..c+d are added (b==0) or modified (b>0); d==0 → a deletion sits below line c (1-based →
/// 0-based). Non-hunk lines are ignored.
fn parse_hunk_header(line: &str, marks: &mut Vec<(usize, u8)>) {
    let Some(rest) = line.strip_prefix("@@ -") else { return };
    let Some((old_part, rest)) = rest.split_once(" +") else { return };
    let Some((new_part, _)) = rest.split_once(" @@") else { return };
    let parse_pair = |s: &str| -> (usize, usize) {
        match s.split_once(',') {
            Some((a, b)) => (a.parse().unwrap_or(0), b.parse().unwrap_or(0)),
            None => (s.parse().unwrap_or(0), 1),
        }
    };
    let (_, old_count) = parse_pair(old_part);
    let (new_start, new_count) = parse_pair(new_part);
    if new_count == 0 {
        // Pure deletion: mark the boundary after `new_start` (0-based line index).
        marks.push((new_start.saturating_sub(1), 2));
    } else {
        let kind = if old_count == 0 { 0 } else { 1 };
        for l in new_start..new_start + new_count {
            marks.push((l.saturating_sub(1), kind));
        }
    }
}

/// Parse a SINGLE file's `git diff -U0` output into gutter marks.
fn parse_diff_gutter(diff: &str) -> Vec<(usize, u8)> {
    let mut marks = Vec::new();
    for line in diff.lines() {
        parse_hunk_header(line, &mut marks);
    }
    marks
}

/// ONE `git diff -U0` subprocess for many files, split into per-file gutter marks keyed by
/// the WORKSPACE-ROOT-relative path — the same key `refresh_gutter_batch` looks up.
///
/// `--relative` is load-bearing: git prints `+++ b/<path>` headers relative to the REPO
/// root, not the `-C` dir, so a project rooted in a subdirectory of its repo (a monorepo
/// crate dir, an app inside a cFS clone) would emit `b/crates/x/src/y.rs` while the lookup
/// key is `src/y.rs` — every restored tab silently got empty marks. The `-c` overrides pin
/// the diff format against user gitconfig: `diff.noprefix=true` drops the `b/` prefix and
/// `diff.mnemonicPrefix=true` turns it into `w/` — either would zero the split in ALL repos
/// — and `core.quotepath=false` keeps non-ASCII paths literal.
fn batch_gutter_diff(root: &Path, rels: &[PathBuf]) -> HashMap<String, Vec<(usize, u8)>> {
    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C").arg(root).args([
        "-c",
        "core.quotepath=false",
        "-c",
        "diff.noprefix=false",
        "-c",
        "diff.mnemonicprefix=false",
        "diff",
        "--no-color",
        "--relative",
        "-U0",
        "HEAD",
        "--",
    ]);
    for rel in rels {
        cmd.arg(rel); // args are exec'd, not shell-parsed — exotic paths need no quoting
    }
    match cmd.output() {
        Ok(o) if o.status.success() => parse_diff_batch(&String::from_utf8_lossy(&o.stdout)),
        _ => HashMap::new(), // not a repo / git failed: everyone gets empty marks
    }
}

/// Old+new line counts of a `@@ -a,b +c,d @@` hunk header — exactly how many CONTENT lines
/// follow it in `-U0` output (zero context: the body is `b` removed + `d` added lines).
/// `None` for anything that is not a well-formed hunk header.
fn hunk_body_lines(line: &str) -> Option<usize> {
    let rest = line.strip_prefix("@@ -")?;
    let (old_part, rest) = rest.split_once(" +")?;
    let (new_part, _) = rest.split_once(" @@")?;
    let count = |s: &str| -> Option<usize> {
        match s.split_once(',') {
            Some((a, b)) => {
                a.parse::<usize>().ok()?;
                b.parse().ok()
            }
            None => s.parse::<usize>().ok().map(|_| 1),
        }
    };
    Some(count(old_part)? + count(new_part)?)
}

/// Split a MULTI-file `git diff -U0` (one batched subprocess for the whole session restore)
/// into per-file gutter marks, keyed by the path from each `+++ b/<path>` header (relative
/// to whatever the diff was invoked with — see [`batch_gutter_diff`]). Requires
/// `core.quotepath=false` so non-ASCII paths arrive literal; a path containing spaces gets a
/// trailing tab (GNU diff convention) which is stripped. Deleted files (`+++ /dev/null`)
/// have no worktree lines to mark and are skipped.
///
/// Header detection NEVER runs inside a hunk body: each `@@` header announces exactly how
/// many content lines follow ([`hunk_body_lines`]) and they are consumed blindly, so
/// `---`/`+++ b/`-shaped CONTENT (a deleted `-- …` Lua/SQL comment directly followed by an
/// added `++ b/…` line, checked-in .patch files) can neither open a phantom file nor steal
/// the real file's remaining hunks. `\ No newline at end of file` markers are not counted
/// in hunk lengths and are skipped for free.
fn parse_diff_batch(diff: &str) -> HashMap<String, Vec<(usize, u8)>> {
    let mut out: HashMap<String, Vec<(usize, u8)>> = HashMap::new();
    let mut current: Option<String> = None;
    let mut prev_was_old_header = false;
    let mut body_left: usize = 0;
    for line in diff.lines() {
        if body_left > 0 {
            if !line.starts_with('\\') {
                body_left -= 1; // hunk content — structurally opaque, consume and move on
            }
            continue;
        }
        // Belt-and-braces on top of the body skip: a `+++` file header only ever follows
        // the `---` one.
        if let Some(rest) = line.strip_prefix("+++ b/") {
            if prev_was_old_header {
                let path = rest.trim_end_matches('\t').to_string();
                out.entry(path.clone()).or_default();
                current = Some(path);
            }
        } else if line.starts_with("+++ ") {
            if prev_was_old_header {
                current = None; // `+++ /dev/null` — the file is gone from the worktree
            }
        } else if line.starts_with("@@ -") {
            // Arm the body skip even when `current` is None (deleted file): ITS content
            // lines must not be header-scanned either.
            body_left = hunk_body_lines(line).unwrap_or(0);
            if let Some(cur) = &current {
                let marks = out.get_mut(cur).expect("current is always inserted");
                parse_hunk_header(line, marks);
            }
        }
        prev_was_old_header = line.starts_with("--- ");
    }
    out
}

/// The identifier containing/preceding `byte` (empty when the caret isn't on one) — seeds
/// rename and the index-backed find-usages.
fn ident_at(rope: &Rope, byte: usize) -> String {
    let start = word_start(rope, byte);
    let mut end_c = rope.byte_to_char(byte.min(rope.len_bytes()));
    while end_c < rope.len_chars() {
        let ch = rope.char(end_c);
        if ch.is_alphanumeric() || ch == '_' {
            end_c += 1;
        } else {
            break;
        }
    }
    rope.byte_slice(start..rope.char_to_byte(end_c)).to_string()
}

/// Start byte of the identifier containing/preceding `byte`.
fn word_start(rope: &Rope, byte: usize) -> usize {
    let mut c = rope.byte_to_char(byte.min(rope.len_bytes()));
    while c > 0 {
        let ch = rope.char(c - 1);
        if ch.is_alphanumeric() || ch == '_' {
            c -= 1;
        } else {
            break;
        }
    }
    rope.char_to_byte(c)
}

fn last_typed_char(edited: &[(char, PathBuf)]) -> Option<(char, &PathBuf)> {
    edited.last().map(|(c, p)| (*c, p))
}

/// LSP SymbolKind → a one-char glyph for the Structure panel.
fn symbol_kind_glyph(kind: lsp_types::SymbolKind) -> &'static str {
    use lsp_types::SymbolKind as K;
    match kind {
        K::FUNCTION | K::METHOD | K::CONSTRUCTOR => "ƒ",
        K::STRUCT | K::CLASS => "◆",
        K::INTERFACE => "◇",
        K::ENUM => "∷",
        K::ENUM_MEMBER | K::CONSTANT => "π",
        K::FIELD | K::PROPERTY | K::VARIABLE => "▪",
        K::MODULE | K::NAMESPACE | K::PACKAGE => "▣",
        K::TYPE_PARAMETER => "τ",
        _ => "·",
    }
}

/// LSP CompletionItemKind → a one-char glyph (colored icons come with the SVG icon pass).
fn completion_kind_glyph(kind: Option<lsp_types::CompletionItemKind>) -> &'static str {
    use lsp_types::CompletionItemKind as K;
    match kind {
        Some(K::FUNCTION) | Some(K::METHOD) => "ƒ",
        Some(K::STRUCT) | Some(K::CLASS) => "◆",
        Some(K::VARIABLE) | Some(K::FIELD) | Some(K::PROPERTY) => "▪",
        Some(K::MODULE) => "▣",
        Some(K::KEYWORD) => "κ",
        Some(K::SNIPPET) => "✂",
        Some(K::ENUM) | Some(K::ENUM_MEMBER) => "∷",
        Some(K::CONSTANT) => "π",
        Some(K::INTERFACE) => "◇",
        _ => "·",
    }
}

/// Reduce LSP snippet syntax to plain text: `${1:name}` → `name`, `$0`/`$1` → "".
/// UTF-16 code-unit offset → byte offset within a string (LSP `ParameterLabel` offsets are
/// UTF-16 per spec). Clamps past-the-end to the end and always lands on a char boundary.
fn utf16_off_to_byte(s: &str, off: usize) -> usize {
    let mut units = 0usize;
    for (bi, ch) in s.char_indices() {
        if units >= off {
            return bi;
        }
        units += ch.len_utf16();
    }
    s.len()
}

/// Rank completion items against the typed prefix, best-first (indices into `items`).
/// Match quality first — exact > case-insensitive exact > prefix > case-insensitive prefix >
/// in-order subsequence (fuzzy `redi` → `read_dir`); non-matches drop out. Ties break on the
/// server's `sortText` (its semantic relevance — locals before globals in rust-analyzer),
/// then shorter label, then arrival order. Replaces the old starts_with-in-server-order
/// filter, which buried the best match under whatever the server sent first.
fn rank_completions(items: &[lsp_types::CompletionItem], needle: &str) -> Vec<usize> {
    let needle_lower = needle.to_lowercase();
    let mut scored: Vec<(u32, &str, usize, usize)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, it)| {
            let hay = it.filter_text.as_deref().unwrap_or(&it.label);
            let score = if needle.is_empty() {
                50 // no prefix yet: pure server relevance via the sortText tiebreak
            } else {
                completion_match_score(hay, needle, &needle_lower)?
            };
            Some((score, it.sort_text.as_deref().unwrap_or(&it.label), hay.len(), i))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)).then(a.2.cmp(&b.2)).then(a.3.cmp(&b.3)));
    scored.into_iter().map(|(_, _, _, i)| i).collect()
}

/// Match quality of one candidate against the typed prefix; `None` = filtered out.
fn completion_match_score(hay: &str, needle: &str, needle_lower: &str) -> Option<u32> {
    if hay == needle {
        return Some(100);
    }
    let hay_lower = hay.to_lowercase();
    if hay_lower == *needle_lower {
        return Some(90);
    }
    if hay.starts_with(needle) {
        return Some(80);
    }
    if hay_lower.starts_with(needle_lower) {
        return Some(70);
    }
    // Fuzzy: every typed char appears, in order.
    let mut hs = hay_lower.chars();
    if needle_lower.chars().all(|nc| hs.any(|hc| hc == nc)) {
        return Some(40);
    }
    None
}

/// Does `name` appear as a whole word (identifier boundary) in `line`? Avoids matching `x`
/// inside `max` when annotating inline debug values.
fn word_appears(line: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let bytes = line.as_bytes();
    let nb = name.as_bytes();
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut i = 0;
    while let Some(pos) = line[i..].find(name) {
        let start = i + pos;
        let end = start + nb.len();
        let before_ok = start == 0 || !is_word(bytes[start - 1]);
        let after_ok = end >= bytes.len() || !is_word(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        i = start + 1;
    }
    false
}

/// Shorten a debug value for the inline annotation — long strings/collections would push the
/// note off-screen. Keeps it to ~40 chars on one line.
fn truncate_val(v: &str) -> String {
    let one_line: String = v.split('\n').next().unwrap_or(v).chars().take(40).collect();
    if one_line.len() < v.len() {
        format!("{one_line}…")
    } else {
        one_line
    }
}

fn strip_snippet_markers(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '$' {
            out.push(ch);
            continue;
        }
        match chars.peek() {
            Some('{') => {
                chars.next();
                // ${N:default} → keep default; ${N} → nothing
                let mut body = String::new();
                for c in chars.by_ref() {
                    if c == '}' {
                        break;
                    }
                    body.push(c);
                }
                if let Some(colon) = body.find(':') {
                    out.push_str(&body[colon + 1..]);
                }
            }
            Some(c) if c.is_ascii_digit() => {
                while matches!(chars.peek(), Some(c) if c.is_ascii_digit()) {
                    chars.next();
                }
            }
            _ => out.push('$'),
        }
    }
    out
}

fn zoom(ctx: &egui::Context, delta: f32) {
    let z = (ctx.zoom_factor() + delta).clamp(0.7, 3.0);
    ctx.set_zoom_factor(z);
}

/// Convert LSP diagnostics (positions in the server's negotiated encoding) into byte-ranged
/// [`ViewDiag`]s against the CURRENT rope.
fn to_view_diags(diags: &[lsp_types::Diagnostic], rope: &Rope, enc: Encoding) -> Vec<ViewDiag> {
    diags
        .iter()
        .map(|d| {
            let s = pos_to_byte(rope, &d.range.start, enc);
            let e = pos_to_byte(rope, &d.range.end, enc).max(s);
            let severity = match d.severity {
                Some(lsp_types::DiagnosticSeverity::ERROR) => 1,
                Some(lsp_types::DiagnosticSeverity::WARNING) => 2,
                Some(lsp_types::DiagnosticSeverity::INFORMATION) => 3,
                Some(lsp_types::DiagnosticSeverity::HINT) => 4,
                _ => 1,
            };
            let source = d.source.as_deref().unwrap_or("lsp");
            ViewDiag { range: s..e, severity, message: format!("[{source}] {}", d.message) }
        })
        .collect()
}

fn pos_to_byte(rope: &Rope, p: &lsp_types::Position, enc: Encoding) -> usize {
    let point = Point { line: p.line as usize, col: p.character as usize };
    match enc {
        Encoding::Utf8 => position::point_to_byte_clamped(rope, point),
        Encoding::Utf16 => position::utf16_to_byte(rope, point),
    }
}


/// Built ELF executables under `root` (exec bit + ELF magic), newest first, capped.
/// Skips VCS/registry dirs; .so and .o are excluded by the magic-byte type check field.
/// Interpreter pair for a debugpy launch. The DEBUGGEE always runs under the project's
/// resolved python (the venv when one exists — same as Run, [`runconfig::python_bin`]), so
/// the debugger sees the same packages; the ADAPTER host is that same interpreter when it can
/// import debugpy (the deps installer puts it in the venv), else global python3. The old
/// unconditional global `python3 -m debugpy.adapter` broke Python debugging on PEP-668
/// distros: venv-installed debugpy meant the adapter died instantly, and a global debugpy
/// still ran the debuggee without the venv's site-packages.
fn debugpy_env(root: &std::path::Path) -> cauldron_dap::PythonEnv {
    let debuggee = PathBuf::from(runconfig::python_bin(root));
    let has_debugpy = std::process::Command::new(&debuggee)
        .args(["-c", "import debugpy"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let host = if has_debugpy { debuggee.clone() } else { PathBuf::from("python3") };
    cauldron_dap::PythonEnv { host, debuggee }
}

fn find_executables(root: &std::path::Path) -> Vec<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let mut out: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    // Executables live in gitignored dirs (build/, target/) — disable ignore files entirely.
    let mut b = ignore::WalkBuilder::new(root);
    b.hidden(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .ignore(false)
        .parents(false)
        .max_depth(Some(7))
        .filter_entry(|e| {
            let n = e.file_name().to_string_lossy();
            n != ".git" && n != ".cauldron" && n != "node_modules" && n != "CMakeFiles"
        });
    let walker = b.build();
    for entry in walker.flatten() {
        let p = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
            continue;
        }
        if p.extension().is_some_and(|e| matches!(e.to_str(), Some("so") | Some("o") | Some("a") | Some("sh") | Some("py"))) {
            continue;
        }
        // ELF magic + e_type: 0x02 (EXEC) or 0x03 (DYN/PIE) both count; shared libs were
        // already dropped by extension.
        let Ok(mut f) = std::fs::File::open(p) else { continue };
        let mut head = [0u8; 18];
        use std::io::Read as _;
        if f.read_exact(&mut head).is_err() || &head[..4] != b"\x7fELF" {
            continue;
        }
        let mtime = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        out.push((mtime, p.to_path_buf()));
    }
    // Real targets first (test/coverage binaries are legion in flight-software trees),
    // newest first within each class.
    let is_testish = |p: &std::path::Path| {
        let s = p.to_string_lossy().to_lowercase();
        s.contains("test") || s.contains("coverage") || s.contains("ut-") || s.contains("stub")
    };
    out.sort_by(|a, b| is_testish(&a.1).cmp(&is_testish(&b.1)).then(b.0.cmp(&a.0)));
    out.truncate(20);
    out.into_iter().map(|(_, p)| p).collect()
}


/// Pipe `input` through a formatter binary, returning formatted text (None on any failure).
fn run_formatter(bin: &str, args: &[&str], _path: &std::path::Path, input: &str) -> Option<String> {
    use std::io::Write as _;
    let mut child = std::process::Command::new(bin)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(input.as_bytes()).ok()?;
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok().filter(|s| !s.is_empty())
}
