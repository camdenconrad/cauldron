//! cider as a LIBRARY: the Rune terminal's engine + embeddable widget, so other Rune apps
//! (the Cauldron IDE's terminal pane) can host a real shell without forking the code. The
//! standalone `cider` binary is a thin shell over the same modules (see `main.rs`/`app.rs`).
//!
//! Embed recipe: `Session::spawn(cols, rows, scrollback, ctx, Some(project_root))` +
//! `widget::terminal_ui(ui, &mut session, &cfg, &mut emoji, &mut dragging)` each frame.

pub mod clip;
pub mod config;
pub mod diag;
pub mod emoji;
pub mod fonts;
pub mod pty;
pub mod session;
pub mod term;
pub mod widget;
