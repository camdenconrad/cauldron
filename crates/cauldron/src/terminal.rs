//! The embedded terminal slot of the bottom dock: MULTIPLE shells (cider engine) with a tab
//! strip, each spawned at the project root. Keyboard reaches a shell only while its grid has
//! egui focus (click it), so the editor never loses keystrokes to a background shell.

use std::path::{Path, PathBuf};

use cider::config::Config;
use cider::emoji::Emoji;
use cider::session::Session;
use egui::Color32;

const DIM: Color32 = Color32::from_rgb(128, 124, 122);

struct Term {
    session: Session,
    /// Stable label ("1", "2", …) — the shell's OSC title shows next to it when set.
    label: String,
}

pub struct TerminalPane {
    pub open: bool,
    terms: Vec<Term>,
    active: usize,
    next_label: u32,
    cfg: Config,
    emoji: Emoji,
    dragging_sel: bool,
    /// Root the shells spawn in (restart/new-tab target).
    root: PathBuf,
    /// The root changed under us (project switch) and the replacement shell is owed. Spawning needs
    /// an `egui::Context`, which [`TerminalPane::set_root`] does not have — so the spawn is deferred
    /// to the next render. Distinct from "no shells": closing the last shell by hand must leave the
    /// pane empty (with its start button), not immediately conjure another one.
    respawn_pending: bool,
    spawn_error: Option<String>,
    /// Grab egui focus for the active grid on the next frame (just opened / restarted).
    focus_pending: bool,
    /// The Run button's PTY. Deliberately NOT a member of `terms`: a program you launched is
    /// output, not a shell you opened, and it belongs under Output where the user goes looking
    /// for it. It is still a real PTY — colors, progress bars, and a tty-shaped stdout are why
    /// Run never used the piped [`crate::runner::Runner`] — but it is rendered READ-ONLY, so it
    /// never competes with the actual terminal for the keyboard.
    run: Option<Term>,
    /// Did the active grid hold egui focus on the last frame it rendered? Read via
    /// [`TerminalPane::shell_focused`] by the global key handling, which must keep its hands off
    /// keys the shell owns. Stale by at most one frame, which focus changes tolerate.
    has_focus: bool,
}

impl TerminalPane {
    pub fn new() -> Self {
        Self {
            open: false,
            terms: Vec::new(),
            active: 0,
            next_label: 0,
            cfg: Config::load(), // the user's cider config — same shell feel as standalone
            emoji: Emoji::load_system(),
            dragging_sel: false,
            root: PathBuf::new(),
            respawn_pending: false,
            spawn_error: None,
            focus_pending: false,
            has_focus: false,
            run: None,
        }
    }

    /// Point the terminal at a NEW project root. Call this ONLY on a real project switch
    /// (`App::open_folder`) — never from a render path. It reaps shells, so anything that can
    /// report a spurious root change (see `toggle` / `ui_embedded`) must not reach it.
    ///
    /// The old project's shells are DROPPED, which SIGHUPs them. Keeping them would be the actual
    /// bug this exists to fix: a shell's cwd is fixed at spawn, so a surviving shell sits in the
    /// project you just left — `ls`, `make`, `git` all answer for the wrong tree. There is no
    /// re-`cd`ing them either; the user may be sitting at a subshell, an editor, a REPL. A fresh
    /// shell at the new root is the only honest state. The replacement is spawned on the next
    /// render ([`respawn_pending`](Self::respawn_pending)) — this has no `egui::Context`.
    pub fn set_root(&mut self, root: &Path) {
        if self.root == root {
            return;
        }
        let had_shells = !self.terms.is_empty();
        self.root = root.to_path_buf();
        self.terms.clear(); // drop = SIGHUP to every shell of the old project
        self.run = None; // and the old project's program, for the same reason
        self.active = 0;
        self.next_label = 0;
        self.spawn_error = None;
        // Only owe a replacement to a pane that HAD one: a project switch must not conjure a shell
        // in a terminal the user had left empty.
        self.respawn_pending = had_shells;
    }

    /// Toggle the pane (Alt+F12). The first open spawns a shell at `root`.
    ///
    /// Like `ui_embedded`, this only LATCHES an unset root — it never re-roots. A project switch
    /// has already called [`set_root`](Self::set_root); anything else reaching here is the same
    /// project, and `root` may be a transient $HOME fallback from `terminal_root()`'s `is_dir()`
    /// probe, which must never be allowed to reap a running shell.
    pub fn toggle(&mut self, ctx: &egui::Context, root: &Path) {
        self.open = !self.open;
        if self.open {
            if self.root.as_os_str().is_empty() {
                self.root = root.to_path_buf();
            }
            if self.terms.is_empty() {
                self.spawn_tab(ctx);
                self.respawn_pending = false;
            }
            self.focus_pending = true;
        }
    }

    /// Run one command line in a fresh PTY owned by the Output pane — real terminal semantics for
    /// the Run button: colors and progress bars work because the program still sees a tty. Any
    /// previous run is dropped first (SIGHUP ends its foreground child). The command is written
    /// into a fresh shell's stdin; the PTY buffers it until the shell is ready.
    ///
    /// The program cannot READ stdin here — the Output grid is read-only (see [`Self::run_ui`]).
    /// A program that needs interactive input should be run from the terminal pane instead.
    ///
    /// This does NOT open the terminal pane: the output surfaces under Output, rendered by
    /// [`TerminalPane::run_ui`]. The caller is responsible for showing that tab.
    pub fn run_command(&mut self, cmdline: &str, cwd: &Path, ctx: &egui::Context) {
        self.stop_run_tab();
        match Session::spawn(80, 24, self.cfg.scrollback, ctx, Some(cwd.to_path_buf())) {
            Ok(mut s) => {
                s.write(format!("{cmdline}\n").as_bytes());
                self.run = Some(Term { session: s, label: cmdline.to_string() });
                self.spawn_error = None;
            }
            Err(e) => self.spawn_error = Some(format!("{e:#}")),
        }
    }

    /// Is a launched program still alive? Drives the Run/Stop button swap. A run whose shell has
    /// exited stays VISIBLE (you want to read what it printed) but is no longer "running".
    pub fn run_running(&self) -> bool {
        self.run.as_ref().is_some_and(|t| !t.session.exited())
    }

    /// Is there a run session at all — live or finished — for Output to show?
    pub fn has_run(&self) -> bool {
        self.run.is_some()
    }

    /// Render the run PTY into the Output pane. Returns true if it drew a grid; false when there
    /// is no run, so Output can fall back to the piped build log.
    pub fn run_ui(&mut self, ui: &mut egui::Ui) -> bool {
        let Some(t) = self.run.as_mut() else { return false };
        // READ-ONLY. The Output pane SHOWS a program's output; it is not a shell. Making it
        // focusable so a program could read stdin meant it swallowed the keyboard whenever it was
        // visible, and the real terminal beside it became unusable. Scrolling, selecting and
        // copying still work; keystrokes do not reach the PTY.
        let _resp = cider::widget::terminal_ui_opts(
            ui,
            &mut t.session,
            &self.cfg,
            &mut self.emoji,
            &mut self.dragging_sel,
            false,
        );
        true
    }

    /// Kill the "run" tab if one exists (dropping the session SIGHUPs the shell and its child).
    /// Drain every session's PTY channel — called ONCE PER FRAME from App::update whether or
    /// not the pane is visible. cider's reader thread sends 8 KB chunks into an unbounded
    /// mpsc forever; when only ui_embedded pumped, a hidden pane (Alt+F12, keep coding while
    /// a build runs) buffered the job's entire output in the channel — unbounded memory, and
    /// one megabyte-sized feed hanging the frame on reopen. Draining keeps the grid's capped
    /// scrollback the only cost, and an idle channel makes this a free try_recv miss.
    pub fn pump_all(&mut self, ctx: &egui::Context) {
        for t in &mut self.terms {
            t.session.pump(ctx);
        }
        if let Some(t) = self.run.as_mut() {
            t.session.pump(ctx);
        }
    }

    /// Kill the running program (dropping the session SIGHUPs the shell and its child) and clear
    /// the pane. Safe to call when nothing is running.
    pub fn stop_run_tab(&mut self) {
        self.run = None;
    }

    fn spawn_tab(&mut self, ctx: &egui::Context) {
        match Session::spawn(80, 24, self.cfg.scrollback, ctx, Some(self.root.clone())) {
            Ok(s) => {
                self.next_label += 1;
                self.terms.push(Term { session: s, label: format!("{}", self.next_label) });
                self.active = self.terms.len() - 1;
                self.spawn_error = None;
                self.focus_pending = true;
            }
            Err(e) => self.spawn_error = Some(format!("{e:#}")),
        }
    }

    /// Render INTO the dock slot `ui` (the dock owns panel layout / resize).
    /// Does the shell grid currently own the keyboard? The global key handling asks this before
    /// claiming any key the shell wants (Tab, Ctrl+Tab): while you're typing in the terminal, the
    /// terminal gets the keystroke. False whenever the pane is closed or shows no grid at all.
    pub fn shell_focused(&self) -> bool {
        // The run grid is deliberately excluded: it is read-only and never holds the keyboard.
        self.open && self.has_focus
    }

    pub fn ui_embedded(&mut self, ui: &mut egui::Ui, root: &Path) {
        let ctx = ui.ctx().clone();
        // Latch the root ONCE (first render). Deliberately not `set_root` per frame: `root` comes
        // from `terminal_root()`, which is a live `is_dir()` probe that falls back to $HOME the
        // moment the project directory stops existing — a `mv` of the project, or a flaky network
        // mount, would otherwise read as a "root change" and kill every running shell. Re-rooting
        // is an event (a project switch calls `set_root`), not a per-frame observation.
        if self.root.as_os_str().is_empty() {
            self.root = root.to_path_buf();
        }
        // A project switch left us owing a shell in the new root; spawn it now that there's a ctx.
        if self.respawn_pending {
            self.respawn_pending = false;
            self.spawn_tab(&ctx);
        }
        // Re-established below only if a grid actually renders and holds focus — so an error state,
        // a closed pane or a shell-less pane can never leave a stale "shell has the keyboard".
        self.has_focus = false;
        // Reap exited shells. `active` is remapped by IDENTITY: every reaped tab below it
        // shifts it down one — a bare clamp silently pointed it at the NEXT live shell, and
        // keystrokes (the grid keeps egui focus) landed in a session the user never picked.
        let removed_below = self.terms.iter().take(self.active).filter(|t| t.session.exited()).count();
        self.terms.retain(|t| !t.session.exited());
        self.active = self.active.saturating_sub(removed_below);
        if self.active >= self.terms.len() {
            self.active = self.terms.len().saturating_sub(1);
        }

        // --- header: TERMINAL + shell tabs + [+] ------------------------------------------
        ui.horizontal(|ui| {
            crate::style::panel_header_inline(ui, "Terminal");
            ui.add_space(8.0);
            // Shell tabs = the SAME component as file tabs: ✕ lives INSIDE the tab (hover),
            // hairline separators, orange underline on the active one.
            let mut close: Option<usize> = None;
            ui.scope(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.set_height(26.0);
                for (i, t) in self.terms.iter().enumerate() {
                    let title = if t.session.title == "cider" || t.session.title.is_empty() {
                        format!("shell {}", t.label)
                    } else {
                        format!("{} · {}", t.label, truncate(&t.session.title, 30))
                    };
                    let tab = crate::style::tab(ui, &title, i == self.active, false);
                    if tab.clicked {
                        self.active = i;
                        self.focus_pending = true;
                    }
                    if tab.closed {
                        close = Some(i);
                    }
                    tab.response.context_menu(|ui| {
                        if ui.button("Close this shell").clicked_by(egui::PointerButton::Primary) {
                            close = Some(i);
                            ui.close_menu();
                        }
                    });
                }
                // The [+] rides the end of the row as its own little tab stub.
                if crate::style::tab_add_button(ui)
                    .on_hover_text("New shell at project root")
                    .clicked_by(egui::PointerButton::Primary)
                {
                    self.spawn_tab(&ctx);
                }
            });
            if let Some(i) = close {
                self.terms.remove(i); // drop = SIGHUP to that shell
                // Same identity remap as the reap above: closing a tab left of the active one
                // must shift `active` down, not let it slide onto a different live shell.
                if i < self.active {
                    self.active -= 1;
                } else if self.active >= self.terms.len() {
                    self.active = self.terms.len().saturating_sub(1);
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("✕").on_hover_text("Hide (Alt+F12)").clicked_by(egui::PointerButton::Primary) {
                    self.open = false;
                }
                if ui.button("↻").on_hover_text("Restart active shell").clicked_by(egui::PointerButton::Primary)
                    && !self.terms.is_empty() {
                        let label = self.terms[self.active].label.clone();
                        match Session::spawn(80, 24, self.cfg.scrollback, &ctx, Some(self.root.clone())) {
                            Ok(s) => self.terms[self.active] = Term { session: s, label },
                            Err(e) => self.spawn_error = Some(format!("{e:#}")),
                        }
                        self.focus_pending = true;
                    }
            });
        });
        crate::style::hairline(ui);

        // --- the active grid -----------------------------------------------------------------
        if let Some(err) = &self.spawn_error {
            ui.colored_label(crate::style::colors::ERROR(), format!("could not start a shell: {err}"));
            return;
        }
        // (Background shells are pumped by pump_all from App::update every frame — including
        // while this pane is hidden or showing a spawn error.)
        match self.terms.get_mut(self.active) {
            Some(t) => {
                let resp = cider::widget::terminal_ui(
                    ui,
                    &mut t.session,
                    &self.cfg,
                    &mut self.emoji,
                    &mut self.dragging_sel,
                );
                if self.focus_pending {
                    resp.request_focus();
                    self.focus_pending = false;
                }
                self.has_focus = resp.has_focus();
            }
            None => {
                ui.centered_and_justified(|ui| {
                    if ui.button("start a shell at the project root").clicked_by(egui::PointerButton::Primary) {
                        self.spawn_tab(&ctx);
                    }
                });
            }
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}
