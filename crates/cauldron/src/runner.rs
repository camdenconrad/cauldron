//! Run/Build: spawn a command (cargo run / cargo build / make), stream its combined output into
//! the OUTPUT bottom panel line-by-line — reader threads + mpsc + request_repaint, the same
//! worker shape as everything else in this codebase. No PTY: this is the build-output pane, not
//! the terminal (that's cider's job).

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{mpsc, Arc, Mutex};

use egui::Color32;

const MAX_LINES: usize = 10_000;
const DIM: Color32 = Color32::from_rgb(128, 124, 122);
const RED: Color32 = Color32::from_rgb(224, 82, 60);

/// Events carry the generation of the run that produced them: a restart bumps it, so the killed
/// run's straggler lines — and above all its waiter's `Done` — can't corrupt the new run's state
/// (the untagged `Done` used to flip `running` off and drop the NEW child's handle, orphaning it).
enum RunEvent {
    Line(u64, String),
    Done(u64, Option<i32>),
}

pub struct Runner {
    rx: Receiver<RunEvent>,
    tx: Sender<RunEvent>,
    child: Option<Arc<Mutex<Child>>>,
    /// Bumped by every start(); pump() drops events from any other generation.
    generation: u64,
    pub lines: Vec<String>,
    pub running: bool,
    pub title: String,
    pub open: bool,
    /// Exit code of the last finished run.
    pub exit: Option<i32>,
    stick_to_bottom: bool,
    /// One-shot env extras applied by the next start() (set via start_with_env).
    extra_env: Vec<(String, String)>,
}

impl Default for Runner {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            rx,
            tx,
            child: None,
            generation: 0,
            lines: Vec::new(),
            running: false,
            title: String::new(),
            open: false,
            exit: None,
            stick_to_bottom: true,
            extra_env: Vec::new(),
        }
    }
}

impl Runner {
    /// [`Self::start`] with extra environment variables (run configurations).
    pub fn start_with_env(
        &mut self,
        program: &str,
        args: &[&str],
        cwd: &Path,
        env: &[(String, String)],
        ctx: &egui::Context,
    ) {
        self.extra_env = env.to_vec();
        self.start(program, args, cwd, ctx);
        self.extra_env.clear();
    }

    /// Launch `program args…` in `cwd`, streaming stdout+stderr. A prior run is stopped first.
    pub fn start(&mut self, program: &str, args: &[&str], cwd: &Path, ctx: &egui::Context) {
        self.stop();
        self.generation += 1;
        let generation = self.generation;
        self.lines.clear();
        self.exit = None;
        self.title = format!("{program} {}", args.join(" "));
        self.open = true;
        self.stick_to_bottom = true;

        let mut command = Command::new(program);
        for (k, v) in &self.extra_env {
            command.env(k, v);
        }
        own_process_group(&mut command);
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
                // Clear `running` explicitly. `start` already stopped the previous child and
                // bumped the generation, so that child's `Done` is now dropped by pump as stale —
                // leaving a failed spawn to inherit the old `true` forever. The toolbar then shows
                // a Stop button for nothing, and anything parked on "the run finished" never
                // resolves.
                self.running = false;
                self.exit = None;
                self.lines.push(format!("failed to start {program}: {e}"));
                return;
            }
        };
        self.running = true;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        for pipe in [stdout.map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
                     stderr.map(|p| Box::new(p) as Box<dyn std::io::Read + Send>)]
            .into_iter()
            .flatten()
        {
            let tx = self.tx.clone();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let r = BufReader::new(pipe);
                for line in r.lines().map_while(Result::ok) {
                    if tx.send(RunEvent::Line(generation, line)).is_err() {
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
            let code = loop {
                match child.lock().unwrap_or_else(|p| p.into_inner()).try_wait() {
                    Ok(Some(status)) => break status.code(),
                    Ok(None) => {}
                    Err(_) => break None,
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            };
            let _ = tx.send(RunEvent::Done(generation, code));
            ctx.request_repaint();
        });
    }

    /// Kill the running process (the waiter thread reports Done).
    pub fn stop(&mut self) {
        if let Some(child) = self.child.take() {
            kill_process_group(&mut child.lock().unwrap_or_else(|p| p.into_inner()));
        }
    }

    /// The current run's generation. A caller parking work on "this build finishing" stores
    /// this and compares later: a NEW run (Ctrl+F9, Run, another debug) bumps it, which cancels
    /// the park unambiguously. Matching on `title` instead was wrong — two `cargo build` runs
    /// produce identical titles, so a user's own build could satisfy someone else's park.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Drain events; call once per frame before drawing.
    pub fn pump(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                RunEvent::Line(g, _) | RunEvent::Done(g, _) if g != self.generation => {}
                RunEvent::Line(_, l) => {
                    self.lines.push(l);
                    if self.lines.len() > MAX_LINES {
                        let drop = self.lines.len() - MAX_LINES;
                        self.lines.drain(..drop);
                    }
                }
                RunEvent::Done(_, code) => {
                    self.running = false;
                    self.exit = code;
                    self.child = None;
                    self.lines.push(match code {
                        Some(0) => "── finished (exit 0) ──".to_string(),
                        Some(c) => format!("── finished (exit {c}) ──"),
                        None => "── killed ──".to_string(),
                    });
                }
            }
        }
    }

    /// The Output body, embedded in the dock's right slot (the dock owns tabs/close/resize).
    pub fn ui_embedded(&mut self, ui: &mut egui::Ui) {
        // NOT pumped here. Draining only while the Output pane is drawn meant a build whose pane
        // was hidden (another bottom tab, or the dock closed) never observed its own completion:
        // `running` stayed true forever, the child's output buffered unboundedly in the channel,
        // and anything parked on "the build finished" — build-before-debug — silently never
        // fired. App::update pumps once per frame instead, visible or not.
        ui.horizontal(|ui| {
            ui.colored_label(DIM, &self.title);
            if self.running {
                ui.spinner();
                if ui.button("■ stop").clicked_by(egui::PointerButton::Primary) {
                    self.stop();
                }
            } else if let Some(c) = self.exit {
                if c != 0 {
                    ui.colored_label(RED, format!("exit {c}"));
                }
            }
        });
        let out = egui::ScrollArea::vertical().auto_shrink([false, false]);
        let out = if self.stick_to_bottom { out.stick_to_bottom(true) } else { out };
        out.show(ui, |ui| {
            let font = egui::TextStyle::Monospace.resolve(ui.style());
            for line in &self.lines {
                // Cheap severity tint: cargo/gcc error lines pop.
                let color = if line.starts_with("error") || line.contains(": error") {
                    RED
                } else if line.starts_with("warning") || line.contains(": warning") {
                    Color32::from_rgb(230, 180, 60)
                } else {
                    Color32::from_rgb(220, 216, 212)
                };
                ui.label(egui::RichText::new(line).font(font.clone()).color(color));
            }
        });
    }
}

/// Put the child in its OWN process group at spawn, so a stop can reach its whole tree.
pub(crate) fn own_process_group(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
}

/// Kill `child` and everything it spawned. Requires [`own_process_group`] at spawn — the
/// child's pid is then its pgid, and `kill -- -pid` reaches grandchildren (cargo run's
/// binary, rustc jobs, in-flight test binaries, npm's dev server) that a bare Child::kill
/// SIGKILLs past: they reparent to init and keep running/holding ports.
pub(crate) fn kill_process_group(child: &mut Child) {
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-9", "--", &format!("-{}", child.id())])
            .status();
    }
    // Fallback (non-unix, or the group kill raced the child's exit) — a double kill is a no-op.
    let _ = child.kill();
}

#[cfg(test)]
mod runner_tests {
    use super::*;

    /// A park on "this build finished" is keyed on the generation, so it must advance on every
    /// start — otherwise a second run could satisfy the first run's park.
    #[test]
    fn generation_advances_on_every_start() {
        let ctx = egui::Context::default();
        let mut r = Runner::default();
        let g0 = r.generation();
        r.start("true", &[], Path::new("/"), &ctx);
        let g1 = r.generation();
        assert!(g1 > g0, "start must bump the generation");
        r.start("true", &[], Path::new("/"), &ctx);
        assert!(r.generation() > g1, "and again");
    }

    /// A spawn that cannot even start must not leave the runner looking busy forever.
    #[test]
    fn failed_spawn_clears_running() {
        let ctx = egui::Context::default();
        let mut r = Runner::default();
        r.running = true; // as if a previous run were live
        r.start("cauldron-no-such-program-xyz", &[], Path::new("/"), &ctx);
        assert!(!r.running, "a failed spawn must clear running, not inherit the old value");
        assert!(
            r.lines.iter().any(|l| l.contains("failed to start")),
            "and must say so: {:?}",
            r.lines
        );
    }
}
