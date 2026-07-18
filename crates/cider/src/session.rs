//! session.rs — one terminal session: a shell on a PTY wired to an Alacritty grid.
//!
//! A [`Session`] is the self-contained unit the app renders — it owns exactly one [`Pty`] and one
//! [`Terminal`] plus that shell's window title, and drives the bytes between them each frame. v1
//! only ever creates and shows one session, but the app holds sessions in a `Vec` with an `active`
//! index so a tab bar can wrap this model later without reshaping it.

use alacritty_terminal::event::{Event, WindowSize};
use anyhow::Result;

use crate::pty::Pty;
use crate::term::Terminal;

const DEFAULT_TITLE: &str = "cider";

/// How long a grid size must hold still before it's pushed to the shell.
///
/// Every resize reflows the grid AND sends SIGWINCH, which makes the shell repaint its prompt. Cell
/// metrics come from `ctx.fonts()` and are recomputed each frame, so they *move* while the font
/// fallback chain is still loading (and during a window drag, and on a scale change) — a single
/// pixel of jitter flips the computed column count back and forth. Resizing on every such flip
/// machine-guns SIGWINCH at the shell, and a prompt that redraws mid-reflow spills its previous
/// rendering into scrollback: that's the wall of half-drawn prompt fragments cider used to open
/// with. Waiting for the size to settle collapses the whole burst into one resize.
pub const RESIZE_DEBOUNCE: f64 = 0.12;

/// A pending resize is force-applied once it has been outstanding this long, even if the size is
/// still moving.
///
/// Without this, a size that oscillates every frame (A→B→A→B — an embedded pane whose available
/// rect feeds back into the layout can do exactly that) would never look "settled", and a pure
/// debounce would then never resize the shell AT ALL, pinning the grid at its spawn size forever.
/// The deadline turns a permanent flap into "at most one resize per deadline" instead of one per
/// frame: still a ~30× reduction at 60fps, and the grid always tracks the window.
pub const RESIZE_MAX_PENDING: f64 = 0.5;

/// What [`ResizeGate`] decided to do with the size the renderer just computed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Resize {
    /// The grid already matches — nothing to do.
    Idle,
    /// Hold off; ask for a repaint in this many seconds so the pending resize can land.
    Wait(f64),
    /// Push this size to the grid and the PTY now.
    Apply(usize, usize),
}

/// The timing policy for shell resizes, split out from [`Session`] so it can be tested without a
/// PTY: every resize reflows the grid AND raises SIGWINCH, which makes the shell repaint its
/// prompt. Cell metrics come from `ctx.fonts()` and are recomputed each frame, so they *move* while
/// the font fallback chain loads, while a pane is dragged, and on a scale change — a pixel of
/// jitter flips the computed column count. Resizing on every flip machine-guns SIGWINCH, and a
/// prompt redrawing mid-reflow spills its previous rendering into scrollback.
#[derive(Default)]
pub struct ResizeGate {
    /// Size we're waiting on, when we last saw it change, and when this pending run began.
    /// `None` = nothing pending.
    pending: Option<Pending>,
}

#[derive(Debug, Clone, Copy)]
struct Pending {
    size: (usize, usize),
    /// Last time `size` changed — the debounce clock.
    changed_at: f64,
    /// When the current pending run started; survives `size` changing — the deadline clock.
    started_at: f64,
}

impl ResizeGate {
    /// Decide what to do with `target` given the grid's `current` size and the clock `now`.
    pub fn poll(&mut self, current: (usize, usize), target: (usize, usize), now: f64) -> Resize {
        if current == target {
            self.pending = None;
            return Resize::Idle;
        }
        let p = match self.pending {
            Some(mut p) => {
                if p.size != target {
                    // Still moving: restart the debounce clock, but NOT the deadline.
                    p.size = target;
                    p.changed_at = now;
                }
                p
            }
            None => Pending { size: target, changed_at: now, started_at: now },
        };
        let settled = now - p.changed_at >= RESIZE_DEBOUNCE;
        let expired = now - p.started_at >= RESIZE_MAX_PENDING;
        if settled || expired {
            self.pending = None;
            return Resize::Apply(target.0, target.1);
        }
        self.pending = Some(p);
        // Wake for whichever comes first: the size settling, or the deadline.
        let until_settled = RESIZE_DEBOUNCE - (now - p.changed_at);
        let until_deadline = RESIZE_MAX_PENDING - (now - p.started_at);
        Resize::Wait(until_settled.min(until_deadline).max(0.0))
    }
}

pub struct Session {
    pub terminal: Terminal,
    pub pty: Pty,
    /// The window title the running program set via OSC (falls back to "cider").
    pub title: String,
    /// Fractional scrollback lines carried between wheel events, so pixel-precise /
    /// high-resolution wheels accumulate instead of losing sub-line deltas to rounding.
    pub scroll_accum: f32,
    /// Debounce + deadline policy for pushing a new grid size to the shell.
    gate: ResizeGate,
    /// CIDER_RESIZE_DEBUG bookkeeping only.
    debug_last_seen: Option<(usize, usize)>,
    debug_raw: u32,
    debug_applied: u32,
}

impl Session {
    /// Spawn a shell and its grid at `cols`×`lines`. `cwd` = starting directory (None = home).
    pub fn spawn(
        cols: usize,
        lines: usize,
        scrollback: usize,
        ctx: &egui::Context,
        cwd: Option<std::path::PathBuf>,
    ) -> Result<Self> {
        let terminal = Terminal::new(cols, lines, scrollback);
        let pty = Pty::spawn(lines as u16, cols as u16, ctx.clone(), cwd)?;
        Ok(Self {
            terminal,
            pty,
            title: DEFAULT_TITLE.into(),
            scroll_accum: 0.0,
            gate: ResizeGate::default(),
            debug_last_seen: None,
            debug_raw: 0,
            debug_applied: 0,
        })
    }

    /// Pull shell output into the grid, then act on any events the grid emitted (write-backs, title
    /// changes, clipboard stores). Called once per frame before rendering.
    pub fn pump(&mut self, ctx: &egui::Context) {
        for chunk in self.pty.drain() {
            self.terminal.feed(&chunk);
        }
        // Feeding may have queued replies (query responses, OSC side-effects). Drain them all.
        while let Some(ev) = self.terminal.next_event() {
            match ev {
                // The grid wants bytes written back (device-attribute / cursor-position replies,
                // focus events…). Not writing these back hangs programs that query the terminal.
                Event::PtyWrite(text) => self.pty.write(text.as_bytes()),
                Event::Title(t) => self.title = t,
                Event::ResetTitle => self.title = DEFAULT_TITLE.into(),
                // OSC 52 copy → system clipboard.
                Event::ClipboardStore(_, text) => ctx.copy_text(text),
                // Program asked for the text-area size; answer with our current grid size.
                Event::TextAreaSizeRequest(fun) => {
                    let reply = fun(self.window_size());
                    self.pty.write(reply.as_bytes());
                }
                // Bell, cursor-blink toggles, OSC-52 paste-load, colour queries and shutdown signals
                // are non-essential for v1 and safely ignored.
                _ => {}
            }
        }
    }

    fn window_size(&self) -> WindowSize {
        WindowSize {
            num_lines: self.terminal.size.lines as u16,
            num_cols: self.terminal.size.cols as u16,
            cell_width: 0,
            cell_height: 0,
        }
    }

    /// Reflow both the grid and the kernel winsize to a new cell grid.
    pub fn resize(&mut self, cols: usize, lines: usize) {
        self.terminal.resize(cols, lines);
        self.pty.resize(lines as u16, cols as u16);
    }

    /// Resize to `cols`×`lines`, but on [`ResizeGate`]'s terms — the single seam every render path
    /// must use instead of calling [`Session::resize`] directly.
    ///
    /// `now` is egui's input clock (`ctx.input(|i| i.time)`). Returns `Some(seconds_left)` while a
    /// resize is still pending, so the caller can schedule the repaint that will apply it (egui is
    /// reactive: with no repaint queued, a size the user stopped moving would sit unapplied until
    /// the next unrelated frame). Returns `None` when the grid already matches — the steady state.
    pub fn resize_settled(&mut self, cols: usize, lines: usize, now: f64) -> Option<f64> {
        let current = (self.terminal.size.cols, self.terminal.size.lines);
        let decision = self.gate.poll(current, (cols, lines), now);

        // CIDER_RESIZE_DEBUG=1 reports both what the old resize-on-every-delta code WOULD have sent
        // and what the gate actually sends, so the two are comparable from a single run.
        if std::env::var_os("CIDER_RESIZE_DEBUG").is_some()
            && self.debug_last_seen != Some((cols, lines))
        {
            self.debug_last_seen = Some((cols, lines));
            self.debug_raw += 1;
            eprintln!(
                "[resize-debug] grid changed to {cols}x{lines} (undebounced resizes would be: {}, \
                 actually sent: {})",
                self.debug_raw, self.debug_applied
            );
        }

        match decision {
            Resize::Idle => None,
            Resize::Wait(left) => Some(left),
            Resize::Apply(w, h) => {
                self.resize(w, h);
                self.debug_applied += 1;
                None
            }
        }
    }

    pub fn write(&mut self, bytes: &[u8]) {
        self.pty.write(bytes);
    }

    /// True once this session's shell has exited — the app closes its tab.
    pub fn exited(&self) -> bool {
        self.pty.exited()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A size the renderer keeps reporting unchanged is pushed once, after the debounce — not on
    /// the first frame it appears, and not again afterwards.
    #[test]
    fn steady_size_applies_once_after_the_debounce() {
        let mut g = ResizeGate::default();
        let (cur, want) = ((80, 24), (200, 50));
        assert!(matches!(g.poll(cur, want, 0.0), Resize::Wait(_)));
        assert!(matches!(g.poll(cur, want, RESIZE_DEBOUNCE / 2.0), Resize::Wait(_)));
        assert_eq!(g.poll(cur, want, RESIZE_DEBOUNCE), Resize::Apply(200, 50));
        // Grid now matches: steady state is Idle, so no repaints are requested forever.
        assert_eq!(g.poll(want, want, RESIZE_DEBOUNCE + 1.0), Resize::Idle);
    }

    /// The storm case: a size that changes every frame keeps restarting the debounce, so nothing is
    /// sent — until the deadline forces one through. This is the whole point of the gate.
    #[test]
    fn flapping_size_is_collapsed_but_still_makes_progress() {
        let mut g = ResizeGate::default();
        let cur = (80, 24);
        let mut applied = 0;
        let mut t = 0.0;
        // 2 seconds of a pane oscillating between two sizes at 60fps — what the old code would have
        // turned into ~120 SIGWINCHes.
        for frame in 0..120 {
            let target = if frame % 2 == 0 { (100, 30) } else { (101, 30) };
            if let Resize::Apply(..) = g.poll(cur, target, t) {
                applied += 1;
            }
            t += 1.0 / 60.0;
        }
        // Bounded by the deadline (2s / 0.5s = 4), and — critically — NOT zero: a pure debounce
        // would never settle here and would leave the shell stuck at its spawn size.
        assert!(applied > 0, "a permanently flapping pane must still resize the shell");
        assert!(applied <= 5, "expected the storm collapsed to ~1 per deadline, got {applied}");
    }

    /// A size still moving when the deadline hits is applied at its LATEST value, never a stale one.
    #[test]
    fn deadline_applies_the_newest_size() {
        let mut g = ResizeGate::default();
        let cur = (80, 24);
        assert!(matches!(g.poll(cur, (90, 24), 0.0), Resize::Wait(_)));
        // Keep it moving so the debounce never settles, right up to the deadline.
        let mut t = 0.0;
        while t < RESIZE_MAX_PENDING - 0.02 {
            t += 0.02;
            assert!(matches!(g.poll(cur, (91, 24), t), Resize::Wait(_)));
            t += 0.02;
            if let Resize::Apply(..) = g.poll(cur, (92, 24), t) {
                panic!("applied before the deadline at t={t}");
            }
        }
        assert_eq!(g.poll(cur, (99, 24), RESIZE_MAX_PENDING + 0.01), Resize::Apply(99, 24));
    }

    /// The gate never asks for a repaint further out than the work it's actually waiting on.
    #[test]
    fn wait_hint_is_bounded_and_non_negative() {
        let mut g = ResizeGate::default();
        match g.poll((80, 24), (120, 40), 0.0) {
            Resize::Wait(d) => assert!(d > 0.0 && d <= RESIZE_DEBOUNCE, "bad wait hint {d}"),
            other => panic!("expected Wait, got {other:?}"),
        }
    }
}
