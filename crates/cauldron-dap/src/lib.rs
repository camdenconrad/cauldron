//! cauldron-dap — the Debug Adapter Protocol client behind the Cauldron IDE.
//!
//! Design of record: cauldron-lsp (threads-not-tokio; the cider PTY template). Per adapter: a
//! blocking READER thread frames Content-Length JSON off child stdout, correlates responses
//! against the pending map by `request_seq`, converts adapter events into [`DebugEvent`]s pushed
//! over `std::sync::mpsc`, then wakes the UI via the injected [`Notifier`] closure. A dedicated
//! WRITER thread owns stdin so a wedged adapter can never stall a frame. The crate is egui-free:
//! everything except the `#[ignore]`d lldb-dap live smoke runs headless.
//!
//! HANDSHAKE (async, never blocks the caller — the state machine advances in
//! [`DebugManager::pump`] as reader events land):
//! `initialize` → (capabilities response) → `launch` → (`initialized` event) →
//! `setBreakpoints` for every stored file → `configurationDone` → [`DebugEvent::Started`].
//! Launch goes out on the initialize RESPONSE rather than the `initialized` event because
//! debugpy only emits `initialized` after it has seen launch/attach; lldb-dap accepts either
//! order. This is the canonical VS Code sequence and works for both adapters.

use std::path::PathBuf;
use std::sync::Arc;

pub mod manager;
pub mod session;
pub mod transport;

pub use manager::DebugManager;

/// Wakes the UI thread after an event lands on the queue (`ctx.request_repaint()` in the app;
/// a no-op closure in tests). Injected so this crate never depends on egui.
pub type Notifier = Arc<dyn Fn() + Send + Sync>;

/// Which debug adapter binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AdapterKind {
    /// `lldb-dap` on PATH — C/C++/Rust natives.
    LldbDap,
    /// `python3 -m debugpy.adapter` — Python (pip: debugpy).
    Debugpy,
    /// `netcoredbg --interpreter=vscode` — .NET (managed DLLs).
    Netcoredbg,
}

impl AdapterKind {
    /// Short human name for the status chip and spawn-failure messages.
    pub fn display_name(self) -> &'static str {
        match self {
            AdapterKind::LldbDap => "lldb-dap",
            AdapterKind::Debugpy => "debugpy",
            AdapterKind::Netcoredbg => "netcoredbg",
        }
    }
}

/// Interpreter pair for debugpy sessions. The adapter process runs under `host` (any python
/// with the debugpy package importable), while the DEBUGGEE runs under `debuggee` — the
/// project's venv interpreter, matching the Run path. Without this split, a global adapter
/// ran the program without the venv's site-packages (first project import crashed under the
/// debugger while Run worked), and a venv without debugpy couldn't host the adapter at all.
#[derive(Clone, Debug)]
pub struct PythonEnv {
    pub host: PathBuf,
    pub debuggee: PathBuf,
}

/// One frame of a stopped thread's call stack (`stackTrace` response).
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub id: i64,
    pub name: String,
    /// Absent for frames without source (libc, interpreter internals).
    pub path: Option<PathBuf>,
    pub line: u32,
}

/// One variable scope of a frame (`scopes` response). `variables_reference` feeds
/// [`DebugManager::request_variables`].
#[derive(Debug, Clone, PartialEq)]
pub struct Scope {
    pub name: String,
    pub variables_reference: i64,
}

/// One variable (`variables` response). `variables_reference > 0` means expandable —
/// feed it back into [`DebugManager::request_variables`] for children.
#[derive(Debug, Clone, PartialEq)]
pub struct Var {
    pub name: String,
    pub value: String,
    pub type_name: Option<String>,
    pub variables_reference: i64,
}

/// Everything the reader/dispatch layer reports up to the app. Drained via
/// [`DebugManager::pump`] on the UI thread.
#[derive(Debug, Clone)]
pub enum DebugEvent {
    /// The handshake completed (`configurationDone` acked) — the debuggee is off and running.
    Started,
    /// The debuggee halted (`stopped` event). A `stackTrace` is fired automatically; the
    /// matching [`DebugEvent::Stack`] follows without the app asking.
    Stopped {
        reason: String,
        thread_id: i64,
        description: Option<String>,
    },
    /// Execution resumed (continue/step acked, or a `continued` event).
    Continued,
    /// Debuggee output (`output` event) — category is "stdout", "stderr", "console", …
    Output { category: String, text: String },
    /// The debuggee process ended with this exit code (`exited` event).
    Exited { code: i32 },
    /// The debug session is over (`terminated` event, or the adapter's pipe closed).
    Terminated,
    /// Response to the automatic (or explicit) `stackTrace` request.
    Stack { frames: Vec<Frame> },
    /// Response to a `threads` request: `(id, name)` for each live thread. Fired on every stop.
    Threads { threads: Vec<(i64, String)> },
    /// Response to [`DebugManager::request_scopes`], echoing the frame it was asked for.
    Scopes { frame_id: i64, scopes: Vec<Scope> },
    /// Response to [`DebugManager::request_variables`], echoing the reference it was asked for.
    Variables { reference: i64, vars: Vec<Var> },
    /// Response to [`DebugManager::evaluate`] / `evaluate_tagged` (tag 0 = console).
    Evaluated { tag: u64, result: String },
    /// `setBreakpoints` response: which of the requested lines the adapter verified.
    BreakpointsResolved {
        path: PathBuf,
        verified_lines: Vec<u32>,
    },
    /// Anything that went wrong out-of-band (failed launch, failed request, spawn error).
    Error(String),
}
