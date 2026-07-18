//! pty.rs — the PTY bridge: spawn the user's `$SHELL` on a real pseudo-terminal and pump bytes.
//!
//! A dedicated OS thread owns the master read end; it blocks on `read`, forwards each chunk to the
//! UI over an mpsc channel, and pokes egui to repaint so shell output appears without polling. The
//! UI thread writes keystrokes through [`Pty::write`] and reflows the kernel winsize via
//! [`Pty::resize`]. All child-process handling stays inside this module.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::Arc;
use std::thread;

use anyhow::{Context as _, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

/// Owns one child shell running on a PTY plus the plumbing to talk to it.
pub struct Pty {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    /// Chunks of raw shell output, oldest first; drained by the UI each frame.
    output: Receiver<Vec<u8>>,
    /// Set by the reader thread when the master hits EOF — i.e. the shell exited (`exit`, Ctrl+D).
    /// The app watches this to close the tab.
    exited: Arc<AtomicBool>,
    // Keep the child alive for the session's lifetime. Dropping `Pty` closes the master fd, which
    // sends SIGHUP to the shell so it exits — no explicit kill needed.
    _child: Box<dyn Child + Send + Sync>,
}

impl Pty {
    /// Spawn `$SHELL` (fallback `/bin/bash`) on a fresh PTY sized `rows`×`cols`. `repaint` is the
    /// egui context the reader thread nudges whenever new output lands. `cwd` overrides the
    /// starting directory (embedded hosts pass their project root; `None` = the user's home).
    pub fn spawn(rows: u16, cols: u16, repaint: egui::Context, cwd: Option<std::path::PathBuf>) -> Result<Self> {
        let pair = native_pty_system()
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .context("openpty")?;

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
        let mut cmd = CommandBuilder::new(&shell);
        // Start in the caller's directory (embedded: the project root) or the user's home, and
        // advertise a modern terminal so programs enable 256-colour / truecolor output. The rest
        // of the environment is inherited.
        match cwd {
            Some(dir) => cmd.cwd(dir),
            None => {
                if let Some(home) = std::env::var_os("HOME") {
                    cmd.cwd(home);
                }
            }
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");

        let child = pair.slave.spawn_command(cmd).context("spawn shell")?;
        // Drop the slave handle now the child holds its own; leaving it open would prevent EOF when
        // the shell exits.
        drop(pair.slave);

        let writer = pair.master.take_writer().context("take pty writer")?;
        let mut reader = pair.master.try_clone_reader().context("clone pty reader")?;

        // BOUNDED: a process spewing output faster than the UI drains must not grow this queue
        // without limit (`yes`, an accidental `cat /dev/urandom`). 128 chunks × 8KiB caps the
        // buffered backlog at ~1MiB; when it fills, `send` blocks the reader thread, which stops
        // reading the master — kernel-level backpressure onto the child, exactly what a real
        // terminal does. No deadlock: every successful send requests a repaint, and the next
        // frame's `drain` frees slots, unblocking the reader.
        let (tx, output): (SyncSender<Vec<u8>>, Receiver<Vec<u8>>) = mpsc::sync_channel(128);
        let exited = Arc::new(AtomicBool::new(false));
        let exited_reader = exited.clone();
        thread::Builder::new()
            .name("cider-pty-reader".into())
            .spawn(move || {
                use std::io::ErrorKind;
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => {
                            crate::diag::note("pty reader: EOF (read 0) on master — treating as shell exit");
                            break;
                        }
                        Ok(n) => {
                            if tx.send(buf[..n].to_vec()).is_err() {
                                break; // UI gone
                            }
                            repaint.request_repaint();
                        }
                        // Transient, non-fatal conditions: the master isn't gone, so retrying is
                        // correct. Treating these as "shell exited" is what made cider vanish
                        // mid-session (the window closes once the last session reports exited).
                        // WouldBlock can appear on the pty master under load; a short yield avoids
                        // a busy spin while we wait for it to clear.
                        Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                        Err(e) if e.kind() == ErrorKind::WouldBlock => {
                            thread::sleep(std::time::Duration::from_millis(2));
                            continue;
                        }
                        Err(e) => {
                            // A genuine error (EIO once the slave is gone, etc.) → the shell really
                            // exited. Log the exact kind + errno so an UNEXPECTED close (a transient
                            // errno we should be retrying instead of treating as exit) leaves a trail.
                            crate::diag::note(&format!(
                                "pty reader stopped on read error: {e} (kind={:?}, raw_os_error={:?})",
                                e.kind(),
                                e.raw_os_error()
                            ));
                            break;
                        }
                    }
                }
                // The read loop only ends when the shell is gone (EOF/error) or the UI dropped.
                exited_reader.store(true, Ordering::Relaxed);
                // A final nudge so the UI paints the shell's last output and notices the exit.
                repaint.request_repaint();
            })
            .context("spawn pty reader thread")?;

        Ok(Self { master: pair.master, writer, output, exited, _child: child })
    }

    /// True once the shell has exited (EOF on the master) — the app closes the tab.
    pub fn exited(&self) -> bool {
        self.exited.load(Ordering::Relaxed)
    }

    /// Take all shell output that has arrived since the last call (never blocks).
    pub fn drain(&self) -> Vec<Vec<u8>> {
        self.output.try_iter().collect()
    }

    /// Write bytes (keystrokes, paste, query replies) to the shell. A dead shell just fails the
    /// write; the session stays up showing its final screen.
    pub fn write(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    /// Tell the kernel the window resized so line-oriented apps (vim, htop) reflow.
    pub fn resize(&self, rows: u16, cols: u16) {
        let _ = self.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
    }
}
