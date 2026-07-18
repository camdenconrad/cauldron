//! One running language server: process + writer/reader/stderr threads + dispatch.
//!
//! THREADS (per server, all plain `std::thread` — the cider PTY template):
//! - **writer** owns `ChildStdin`: drains an mpsc of [`Outgoing`] and frames them. A wedged server
//!   (mid-index, not draining its pipe) blocks THIS thread, never a frame.
//! - **reader** owns `BufReader<ChildStdout>`: frames messages, resolves responses against the
//!   pending map, AUTO-REPLIES server→client requests (via a writer-channel clone — responses
//!   interleave correctly because one thread owns stdin), converts notifications into events, and
//!   wakes the UI through the injected notifier.
//! - **stderr** drains child stderr into `log::debug!` so the pipe can never fill up.
//!
//! The reader emits crate-internal [`Raw`] events; `LspManager::pump` folds them into public
//! [`LspEvent`]s and drives the init/respawn state machine on the UI thread.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::Instant;

use serde_json::{json, Value};

use crate::{transport, Encoding, LspEvent, Notifier, ServerId, ServerKind, ServerState};

/// Everything the writer thread can be asked to put on the wire.
#[derive(Debug)]
pub(crate) enum Outgoing {
    Request { id: i64, method: &'static str, params: Value },
    Notification { method: &'static str, params: Value },
    /// Reply to a server→client request. `id` is echoed verbatim (servers may use string ids).
    Response { id: Value, result: Value },
    ErrorResponse { id: Value, code: i64, message: String },
    /// Write the `exit` notification, signal `exit_flushed`, and stop the writer.
    Exit,
}

/// What a request id is waiting for — decides how its response is decoded.
#[derive(Debug, Clone)]
pub(crate) enum PendingKind {
    Initialize,
    Shutdown,
    Hover { generation: u64 },
    Definition { generation: u64 },
    Completion { generation: u64 },
    CodeAction { generation: u64, path: PathBuf },
    ResolveCodeAction { generation: u64, path: PathBuf },
    Rename { generation: u64 },
    Formatting { generation: u64, path: PathBuf },
    References { generation: u64 },
    ResolveCompletion { generation: u64, path: PathBuf },
    SignatureHelp { generation: u64 },
    DocumentSymbols { generation: u64, path: PathBuf },
    InlayHints { generation: u64, path: PathBuf },
    PrepareCallHierarchy { generation: u64 },
    IncomingCalls { generation: u64 },
    WorkspaceSymbols { generation: u64 },
    PullDiagnostics { path: PathBuf, version: i32 },
    /// A request whose response carries nothing we act on (e.g. `workspace/executeCommand` —
    /// any resulting edits arrive separately as a server→client `workspace/applyEdit`).
    Generic,
}

#[derive(Debug)]
pub(crate) struct Pending {
    pub kind: PendingKind,
    pub sent: Instant,
}

/// Crate-internal reader→manager events; the manager folds these into public [`LspEvent`]s
/// and state transitions on the UI thread.
#[derive(Debug)]
pub(crate) enum Raw {
    Lsp(LspEvent),
    InitResult(Value),
    InitFailed(String),
    ShutdownAck,
    /// `$/progress` folded down to the bit the status bar needs.
    Progress { pct: Option<u32>, done: bool },
    /// Reader hit EOF — the process is gone.
    Eof,
}

/// Per-document sync bookkeeping. Text lives in the app's buffers, never here.
#[derive(Debug)]
pub(crate) struct DocState {
    pub version: i32,
}

pub(crate) struct LspServer {
    pub id: ServerId,
    pub child: Child,
    pub to_writer: Sender<Outgoing>,
    pub pending: Arc<Mutex<HashMap<i64, Pending>>>,
    next_id: i64,
    pub encoding: Encoding,
    /// Negotiated textDocumentSync change kind: 0 none, 1 full, 2 incremental.
    pub sync_kind: i64,
    pub state: ServerState,
    pub docs: HashMap<PathBuf, DocState>,
    /// Messages held back until the initialize handshake completes, flushed in order.
    queued: Vec<Outgoing>,
    /// Raw `capabilities` object from the initialize result.
    pub caps: Value,
    /// Server supports `textDocument/diagnostic` pull (rust-analyzer's native channel).
    pub diag_provider: bool,
    /// rust-analyzer reached `experimental/serverStatus {quiescent:true}` — its workspace-wide
    /// features (e.g. `workspace/symbol`) are trustworthy AND won't stall on a half-built index.
    /// Meaningless for other kinds (they never send it); stays false.
    pub quiescent: bool,
    pub shutting_down: Arc<AtomicBool>,
    /// Set by the writer after the `exit` notification is flushed (graceful-quit handshake).
    pub exit_flushed: Arc<(Mutex<bool>, Condvar)>,
}

impl LspServer {
    /// Spawn the server process + its three threads and fire the `initialize` request.
    /// Never blocks on the child: the handshake completes asynchronously via [`Raw::InitResult`].
    pub(crate) fn spawn(
        id: ServerId,
        kind: ServerKind,
        root: &PathBuf,
        compile_db: Option<&PathBuf>,
        events_tx: Sender<(ServerId, Raw)>,
        notifier: Notifier,
    ) -> std::io::Result<Self> {
        let mut cmd = match kind {
            ServerKind::Clangd => {
                let mut c = Command::new("clangd");
                // NASA layer owns clang-tidy (double-reporting otherwise); stderr tamed but drained.
                c.args(["--background-index", "--clang-tidy=0", "--header-insertion=never", "--log=error"]);
                if let Some(db) = compile_db {
                    c.arg(format!("--compile-commands-dir={}", db.display()));
                }
                c
            }
            // Plain name on PATH so the rustup proxy tracks toolchain switches.
            ServerKind::RustAnalyzer => Command::new("rust-analyzer"),
            ServerKind::Pyright => {
                let mut c = Command::new("pyright-langserver");
                c.arg("--stdio");
                c
            }
            ServerKind::TsServer => {
                let mut c = Command::new("typescript-language-server");
                c.arg("--stdio");
                c
            }
            ServerKind::CssLs => {
                let mut c = Command::new("vscode-css-language-server");
                c.arg("--stdio");
                c
            }
            ServerKind::HtmlLs => {
                let mut c = Command::new("vscode-html-language-server");
                c.arg("--stdio");
                c
            }
            // csharp-ls speaks LSP over stdio with no flag. Installed as a global dotnet tool,
            // so it lands in ~/.dotnet/tools (see the PATH fallback below).
            ServerKind::CSharpLs => Command::new("csharp-ls"),
            ServerKind::JsonLs => {
                let mut c = Command::new("vscode-json-language-server");
                c.arg("--stdio");
                c
            }
            ServerKind::YamlLs => {
                let mut c = Command::new("yaml-language-server");
                c.arg("--stdio");
                c
            }
            // The jdtls launcher speaks LSP over stdio by default and manages its own
            // per-project workspace data dir (keyed off cwd); no flags needed.
            ServerKind::Jdtls => Command::new("jdtls"),
        };
        cmd.current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            // Not on PATH: retry from the well-known per-user tool prefixes. GUI sessions don't
            // always inherit the shell's PATH additions for these — ~/.npm-global/bin (npm
            // globals: pyright, tsserver, css/html) and ~/.dotnet/tools (dotnet globals:
            // csharp-ls).
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let prog = cmd.get_program().to_owned();
                let fallback = std::env::var_os("HOME").and_then(|h| {
                    let home = PathBuf::from(h);
                    [".npm-global/bin", ".dotnet/tools"]
                        .into_iter()
                        .map(|dir| home.join(dir).join(&prog))
                        .find(|p| p.is_file())
                });
                match fallback {
                    Some(p) => {
                        let mut c = Command::new(p);
                        c.args(cmd.get_args())
                            .current_dir(root)
                            .stdin(Stdio::piped())
                            .stdout(Stdio::piped())
                            .stderr(Stdio::piped());
                        c.spawn()?
                    }
                    _ => return Err(e),
                }
            }
            Err(e) => return Err(e),
        };

        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let (to_writer, from_ui): (Sender<Outgoing>, Receiver<Outgoing>) = mpsc::channel();
        let pending: Arc<Mutex<HashMap<i64, Pending>>> = Arc::new(Mutex::new(HashMap::new()));
        let shutting_down = Arc::new(AtomicBool::new(false));
        let exit_flushed = Arc::new((Mutex::new(false), Condvar::new()));

        // --- writer ------------------------------------------------------------------------
        {
            let exit_flushed = Arc::clone(&exit_flushed);
            std::thread::Builder::new().name(format!("lsp-writer-{}", id.0)).spawn(move || {
                let mut w = stdin;
                while let Ok(out) = from_ui.recv() {
                    let v = match &out {
                        Outgoing::Request { id, method, params } => {
                            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
                        }
                        Outgoing::Notification { method, params } => {
                            json!({"jsonrpc": "2.0", "method": method, "params": params})
                        }
                        Outgoing::Response { id, result } => {
                            json!({"jsonrpc": "2.0", "id": id, "result": result})
                        }
                        Outgoing::ErrorResponse { id, code, message } => {
                            json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
                        }
                        Outgoing::Exit => {
                            let _ = transport::write_message(
                                &mut w,
                                &json!({"jsonrpc": "2.0", "method": "exit"}),
                            );
                            let (lock, cv) = &*exit_flushed;
                            *lock.lock().unwrap_or_else(|p| p.into_inner()) = true;
                            cv.notify_all();
                            return;
                        }
                    };
                    if transport::write_message(&mut w, &v).is_err() {
                        return; // pipe gone — reader EOF handles the rest
                    }
                }
            })?;
        }

        // --- reader ------------------------------------------------------------------------
        {
            let pending = Arc::clone(&pending);
            let to_writer = to_writer.clone();
            let shutting_down = Arc::clone(&shutting_down);
            let notifier = Arc::clone(&notifier);
            std::thread::Builder::new().name(format!("lsp-reader-{}", id.0)).spawn(move || {
                reader_loop(id, stdout, pending, to_writer, events_tx, notifier, shutting_down);
            })?;
        }

        // --- stderr drain --------------------------------------------------------------------
        std::thread::Builder::new().name(format!("lsp-stderr-{}", id.0)).spawn(move || {
            let r = BufReader::new(stderr);
            for line in r.lines().map_while(Result::ok) {
                log::debug!("lsp[{}] stderr: {line}", id.0);
            }
        })?;

        let mut server = Self {
            id,
            child,
            to_writer,
            pending,
            next_id: 0,
            encoding: Encoding::Utf16,
            sync_kind: 0,
            state: ServerState::Initializing,
            docs: HashMap::new(),
            queued: Vec::new(),
            caps: Value::Null,
            diag_provider: false,
            quiescent: false,
            shutting_down,
            exit_flushed,
        };
        // Fire initialize immediately — the only message allowed pre-handshake.
        let params = crate::capabilities::initialize_params(root, kind);
        server.send_request_now("initialize", params, PendingKind::Initialize);
        Ok(server)
    }

    fn alloc_id(&mut self, kind: PendingKind) -> i64 {
        self.next_id += 1;
        self.pending
            .lock()
            .unwrap()
            .insert(self.next_id, Pending { kind, sent: Instant::now() });
        self.next_id
    }

    /// Send a request immediately, bypassing the pre-Ready queue (initialize/shutdown only).
    fn send_request_now(&mut self, method: &'static str, params: Value, kind: PendingKind) {
        let id = self.alloc_id(kind);
        let _ = self.to_writer.send(Outgoing::Request { id, method, params });
    }

    /// Queue-or-send: notifications and feature requests hold until the handshake completes.
    pub(crate) fn send(&mut self, out: Outgoing) {
        if matches!(self.state, ServerState::Initializing | ServerState::Spawning) {
            self.queued.push(out);
        } else {
            let _ = self.to_writer.send(out);
        }
    }

    pub(crate) fn request(&mut self, method: &'static str, params: Value, kind: PendingKind) {
        let id = self.alloc_id(kind);
        self.send(Outgoing::Request { id, method, params });
    }

    /// Complete the handshake: negotiate, mark Ready, send `initialized`, flush the queue.
    pub(crate) fn finish_initialize(&mut self, result: &Value) {
        let caps = result.get("capabilities").cloned().unwrap_or(Value::Null);
        let (enc, sync) = crate::capabilities::negotiate(result);
        self.encoding = enc;
        self.sync_kind = sync;
        self.diag_provider = caps.get("diagnosticProvider").is_some();
        self.caps = caps;
        self.state = ServerState::Ready;
        let _ = self
            .to_writer
            .send(Outgoing::Notification { method: "initialized", params: json!({}) });
        for out in std::mem::take(&mut self.queued) {
            let _ = self.to_writer.send(out);
        }
    }

    /// Graceful quit, phase 1: `shutdown` request (the manager sends `Exit` on ack/timeout).
    pub(crate) fn start_shutdown(&mut self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        self.send_request_now("shutdown", Value::Null, PendingKind::Shutdown);
    }
}

// -------------------------------------------------------------------------------------------
// reader-side dispatch
// -------------------------------------------------------------------------------------------

fn emit(events: &Sender<(ServerId, Raw)>, notifier: &Notifier, id: ServerId, ev: Raw) {
    let _ = events.send((id, ev));
    notifier();
}

fn reader_loop(
    id: ServerId,
    stdout: std::process::ChildStdout,
    pending: Arc<Mutex<HashMap<i64, Pending>>>,
    to_writer: Sender<Outgoing>,
    events: Sender<(ServerId, Raw)>,
    notifier: Notifier,
    shutting_down: Arc<AtomicBool>,
) {
    let mut r = BufReader::new(stdout);
    let mut header_buf = String::new();
    let mut body_buf = Vec::new();
    // EOF (Ok(None)) or a torn pipe (Err) both mean the process is gone.
    while let Ok(Some(msg)) = transport::read_message(&mut r, &mut header_buf, &mut body_buf) {
        route(id, msg, &pending, &to_writer, &events, &notifier, &shutting_down);
    }
    pending.lock().unwrap_or_else(|p| p.into_inner()).clear();
    emit(&events, &notifier, id, Raw::Eof);
}

/// Route one inbound message: response → pending decode; server request → auto-reply;
/// notification → event. Runs on the reader thread; only cheap parsing happens here.
fn route(
    id: ServerId,
    msg: Value,
    pending: &Arc<Mutex<HashMap<i64, Pending>>>,
    to_writer: &Sender<Outgoing>,
    events: &Sender<(ServerId, Raw)>,
    notifier: &Notifier,
    shutting_down: &Arc<AtomicBool>,
) {
    let has_id = msg.get("id").is_some();
    let has_method = msg.get("method").is_some();

    if has_id && !has_method {
        // ---- response to one of our requests ------------------------------------------------
        let Some(req_id) = msg.get("id").and_then(id_as_i64) else { return };
        let Some(p) = pending.lock().unwrap_or_else(|p| p.into_inner()).remove(&req_id) else { return };
        if let Some(err) = msg.get("error") {
            let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
            match p.kind {
                PendingKind::Initialize => {
                    let m = err.get("message").and_then(Value::as_str).unwrap_or("initialize failed");
                    emit(events, notifier, id, Raw::InitFailed(m.to_string()));
                }
                PendingKind::Shutdown => emit(events, notifier, id, Raw::ShutdownAck),
                // The user pressed a refactor and is waiting on it — a silent swallow here reads
                // as "the IDE ignored me". Report the failure so the app can say so.
                PendingKind::ResolveCodeAction { generation, path } => {
                    log::warn!("lsp[{}] codeAction/resolve failed ({code}): {err}", id.0);
                    emit(
                        events,
                        notifier,
                        id,
                        Raw::Lsp(LspEvent::ResolvedCodeAction { generation, path, action: None }),
                    );
                }
                // ContentModified / ServerNotInitialized are expected races — swallow silently.
                _ => {
                    if code != -32801 && code != -32002 {
                        log::warn!("lsp[{}] request failed ({code}): {err}", id.0);
                    }
                }
            }
            return;
        }
        let result = msg.get("result").cloned().unwrap_or(Value::Null);
        match p.kind {
            PendingKind::Initialize => emit(events, notifier, id, Raw::InitResult(result)),
            PendingKind::Shutdown => emit(events, notifier, id, Raw::ShutdownAck),
            PendingKind::Hover { generation } => {
                let contents = serde_json::from_value(result).ok();
                emit(events, notifier, id, Raw::Lsp(LspEvent::Hover { generation, contents }));
            }
            PendingKind::Definition { generation } => {
                let locations = parse_definition(result);
                emit(events, notifier, id, Raw::Lsp(LspEvent::Definition { generation, locations }));
            }
            PendingKind::Completion { generation } => {
                let items = parse_completion(result);
                emit(events, notifier, id, Raw::Lsp(LspEvent::Completions { generation, items }));
            }
            PendingKind::CodeAction { generation, path } => {
                let actions = parse_code_actions(result);
                emit(
                    events,
                    notifier,
                    id,
                    Raw::Lsp(LspEvent::CodeActions { generation, path, actions }),
                );
            }
            PendingKind::ResolveCodeAction { generation, path } => {
                // A resolve that comes back undecodable is a failed refactor, not a no-op —
                // emit `None` so the app can say so instead of leaving the user guessing.
                let action = serde_json::from_value::<lsp_types::CodeAction>(result)
                    .ok()
                    .map(Box::new);
                emit(
                    events,
                    notifier,
                    id,
                    Raw::Lsp(LspEvent::ResolvedCodeAction { generation, path, action }),
                );
            }
            PendingKind::Rename { generation } => {
                let edit = serde_json::from_value::<lsp_types::WorkspaceEdit>(result).ok();
                emit(events, notifier, id, Raw::Lsp(LspEvent::RenameEdit { generation, edit }));
            }
            PendingKind::Formatting { generation, path } => {
                // null result = server declined / nothing to do → empty edit list (a no-op apply).
                let edits =
                    serde_json::from_value::<Vec<lsp_types::TextEdit>>(result).unwrap_or_default();
                emit(events, notifier, id, Raw::Lsp(LspEvent::Formatting { generation, path, edits }));
            }
            PendingKind::References { generation } => {
                let locations = serde_json::from_value::<Vec<lsp_types::Location>>(result)
                    .unwrap_or_default();
                emit(events, notifier, id, Raw::Lsp(LspEvent::References { generation, locations }));
            }
            PendingKind::ResolveCompletion { generation, path } => {
                if let Ok(item) = serde_json::from_value::<lsp_types::CompletionItem>(result) {
                    emit(
                        events,
                        notifier,
                        id,
                        Raw::Lsp(LspEvent::ResolvedCompletion { generation, path, item }),
                    );
                }
            }
            PendingKind::SignatureHelp { generation } => {
                let help = serde_json::from_value::<lsp_types::SignatureHelp>(result).ok();
                emit(events, notifier, id, Raw::Lsp(LspEvent::SignatureHelp { generation, help }));
            }
            PendingKind::DocumentSymbols { generation, path } => {
                // Nested DocumentSymbol[] (modern) or flat SymbolInformation[] (legacy) — both.
                let symbols = serde_json::from_value::<lsp_types::DocumentSymbolResponse>(result).ok();
                emit(
                    events,
                    notifier,
                    id,
                    Raw::Lsp(LspEvent::DocumentSymbols { generation, path, symbols }),
                );
            }
            PendingKind::InlayHints { generation, path } => {
                // `InlayHint[] | null` — null (declined / no hints) becomes an empty list so
                // the app still clears stale hints for the doc.
                let hints =
                    serde_json::from_value::<Vec<lsp_types::InlayHint>>(result).unwrap_or_default();
                emit(events, notifier, id, Raw::Lsp(LspEvent::InlayHints { generation, path, hints }));
            }
            PendingKind::PrepareCallHierarchy { generation } => {
                // `CallHierarchyItem[] | null` — the manager fires incomingCalls for the
                // first item (that follow-up needs the item object round-tripped).
                let items =
                    serde_json::from_value::<Vec<lsp_types::CallHierarchyItem>>(result).unwrap_or_default();
                emit(events, notifier, id, Raw::Lsp(LspEvent::CallHierarchyItems { generation, items }));
            }
            PendingKind::IncomingCalls { generation } => {
                // `CallHierarchyIncomingCall[] | null` → (caller name, its first call site).
                let calls = serde_json::from_value::<Vec<lsp_types::CallHierarchyIncomingCall>>(result)
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|c| {
                        let range = c.from_ranges.first().copied().unwrap_or(c.from.range);
                        Some((c.from.name, lsp_types::Location { uri: c.from.uri, range }))
                    })
                    .collect();
                emit(events, notifier, id, Raw::Lsp(LspEvent::IncomingCalls { generation, calls }));
            }
            PendingKind::WorkspaceSymbols { generation } => {
                let symbols = parse_workspace_symbols(result);
                emit(
                    events,
                    notifier,
                    id,
                    Raw::Lsp(LspEvent::WorkspaceSymbols { generation, symbols }),
                );
            }
            PendingKind::Generic => {}
            PendingKind::PullDiagnostics { path, version } => {
                // {"kind":"full","items":[...]} — "unchanged" means keep what we have.
                if pull_kind_is_full(&result) {
                    let diags = result
                        .get("items")
                        .and_then(|v| serde_json::from_value(v.clone()).ok())
                        .unwrap_or_default();
                    emit(
                        events,
                        notifier,
                        id,
                        Raw::Lsp(LspEvent::PullDiagnostics { path, version, diags }),
                    );
                }
            }
        }
        return;
    }

    if has_id && has_method {
        // ---- server→client request: auto-reply ----------------------------------------------
        let req_id = msg.get("id").cloned().unwrap_or(Value::Null);
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let reply = |result: Value| {
            let _ = to_writer.send(Outgoing::Response { id: req_id.clone(), result });
        };
        match method {
            "workspace/configuration" => {
                let n = params.get("items").and_then(Value::as_array).map_or(1, Vec::len);
                reply(Value::Array(vec![Value::Null; n]));
            }
            "client/registerCapability" | "client/unregisterCapability" => reply(Value::Null),
            "window/workDoneProgress/create" => reply(Value::Null),
            "workspace/diagnostic/refresh" => {
                reply(Value::Null);
                emit(events, notifier, id, Raw::Lsp(LspEvent::PullAllDiagnostics));
            }
            "workspace/applyEdit" => {
                if let Ok(p) = serde_json::from_value(params) {
                    emit(events, notifier, id, Raw::Lsp(LspEvent::ApplyEdit(p)));
                }
                reply(json!({"applied": true}));
            }
            _ if shutting_down.load(Ordering::SeqCst) => reply(Value::Null), // never error mid-quit
            _ => {
                let _ = to_writer.send(Outgoing::ErrorResponse {
                    id: req_id,
                    code: -32601,
                    message: format!("method not found: {method}"),
                });
            }
        }
        return;
    }

    // ---- notification -------------------------------------------------------------------------
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    match method {
        "textDocument/publishDiagnostics" => {
            let Some(uri) = params.get("uri").and_then(Value::as_str) else { return };
            let Ok(url) = lsp_types::Url::parse(uri) else { return };
            let Some(path) = crate::capabilities::uri_to_path(&url) else { return };
            let version = params.get("version").and_then(Value::as_i64).map(|v| v as i32);
            let diags = params
                .get("diagnostics")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            emit(events, notifier, id, Raw::Lsp(LspEvent::Diagnostics { path, version, diags }));
        }
        "$/progress" => {
            let value = params.get("value").cloned().unwrap_or(Value::Null);
            let kind = value.get("kind").and_then(Value::as_str).unwrap_or("");
            let pct = value.get("percentage").and_then(Value::as_u64).map(|p| p as u32);
            match kind {
                "begin" | "report" => emit(events, notifier, id, Raw::Progress { pct, done: false }),
                "end" => emit(events, notifier, id, Raw::Progress { pct: None, done: true }),
                _ => {}
            }
        }
        "experimental/serverStatus" => {
            if params.get("quiescent").and_then(Value::as_bool) == Some(true) {
                emit(events, notifier, id, Raw::Lsp(LspEvent::Quiescent));
            }
        }
        "window/showMessage" => {
            // type 1 = error, 2 = warning — surface those; info/log stay in the log.
            let ty = params.get("type").and_then(Value::as_i64).unwrap_or(3);
            let m = params.get("message").and_then(Value::as_str).unwrap_or("");
            if ty <= 2 && !m.is_empty() {
                emit(events, notifier, id, Raw::Lsp(LspEvent::Message(m.to_string())));
            } else {
                log::info!("lsp[{}]: {m}", id.0);
            }
        }
        "window/logMessage" => {
            log::debug!(
                "lsp[{}] log: {}",
                id.0,
                params.get("message").and_then(Value::as_str).unwrap_or("")
            );
        }
        _ => log::trace!("lsp[{}] unhandled notification {method}", id.0),
    }
}

/// Request ids we allocate are always integers, but be tolerant reading them back.
fn id_as_i64(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)).or_else(|| v.as_str()?.parse().ok())
}

fn pull_kind_is_full(result: &Value) -> bool {
    result.get("kind").and_then(Value::as_str) != Some("unchanged")
}

/// `textDocument/definition` result: Location | Location[] | LocationLink[].
fn parse_definition(result: Value) -> Vec<lsp_types::Location> {
    if let Ok(one) = serde_json::from_value::<lsp_types::Location>(result.clone()) {
        return vec![one];
    }
    if let Ok(many) = serde_json::from_value::<Vec<lsp_types::Location>>(result.clone()) {
        return many;
    }
    // LocationLink[] — map to plain Locations via targetUri/targetSelectionRange.
    if let Some(arr) = result.as_array() {
        return arr
            .iter()
            .filter_map(|l| {
                let uri = l.get("targetUri")?.as_str()?;
                let range = l.get("targetSelectionRange").or_else(|| l.get("targetRange"))?;
                Some(lsp_types::Location {
                    uri: lsp_types::Url::parse(uri).ok()?,
                    range: serde_json::from_value(range.clone()).ok()?,
                })
            })
            .collect();
    }
    Vec::new()
}

/// `textDocument/codeAction` result: `(Command | CodeAction)[] | null`. Parsed element-wise so
/// one exotic entry can't discard the whole batch.
fn parse_code_actions(result: Value) -> Vec<lsp_types::CodeActionOrCommand> {
    match result {
        Value::Array(items) => {
            items.into_iter().filter_map(|v| serde_json::from_value(v).ok()).collect()
        }
        _ => Vec::new(),
    }
}

/// `textDocument/completion` result: CompletionItem[] | CompletionList | null.
fn parse_completion(result: Value) -> Vec<lsp_types::CompletionItem> {
    if let Ok(items) = serde_json::from_value::<Vec<lsp_types::CompletionItem>>(result.clone()) {
        return items;
    }
    result
        .get("items")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default()
}

/// `workspace/symbol` result: SymbolInformation[] | WorkspaceSymbol[] | null, normalized to flat
/// `SymbolInformation` rows. Parsed element-wise (parse_code_actions-style) so one exotic entry
/// can't discard the whole batch. A 3.17 `WorkspaceSymbol` whose location is range-less (`{uri}`
/// only) gets a zero range — line 0 is still an openable target, and requesting resolve support
/// we don't advertise would be dishonest.
#[allow(deprecated)] // SymbolInformation::deprecated must be populated to construct the struct.
fn parse_workspace_symbols(result: Value) -> Vec<lsp_types::SymbolInformation> {
    let Value::Array(items) = result else { return Vec::new() };
    items
        .into_iter()
        .filter_map(|v| {
            // SymbolInformation (and WorkspaceSymbol with a full Location) decode directly.
            if let Ok(si) = serde_json::from_value::<lsp_types::SymbolInformation>(v.clone()) {
                return Some(si);
            }
            // WorkspaceSymbol with a range-less location: normalize by hand.
            let name = v.get("name")?.as_str()?.to_string();
            let kind = serde_json::from_value(v.get("kind")?.clone()).ok()?;
            let location = v.get("location")?;
            let uri = lsp_types::Url::parse(location.get("uri")?.as_str()?).ok()?;
            let range = location
                .get("range")
                .and_then(|r| serde_json::from_value(r.clone()).ok())
                .unwrap_or_else(|| {
                    lsp_types::Range::new(
                        lsp_types::Position::new(0, 0),
                        lsp_types::Position::new(0, 0),
                    )
                });
            Some(lsp_types::SymbolInformation {
                name,
                kind,
                tags: None,
                deprecated: None,
                location: lsp_types::Location { uri, range },
                container_name: v
                    .get("containerName")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Notifier;

    #[test]
    fn parse_workspace_symbols_flat_symbol_information() {
        let result = json!([
            {
                "name": "OS_TaskDelay",
                "kind": 12, // Function
                "location": {
                    "uri": "file:///proj/src/os_task.c",
                    "range": {"start": {"line": 41, "character": 4},
                              "end": {"line": 41, "character": 16}}
                },
                "containerName": "osal"
            }
        ]);
        let syms = parse_workspace_symbols(result);
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "OS_TaskDelay");
        assert_eq!(syms[0].kind, lsp_types::SymbolKind::FUNCTION);
        assert_eq!(syms[0].location.uri.as_str(), "file:///proj/src/os_task.c");
        assert_eq!(syms[0].location.range.start.line, 41);
        assert_eq!(syms[0].container_name.as_deref(), Some("osal"));
    }

    #[test]
    fn parse_workspace_symbols_range_less_workspace_symbol_and_bad_entry() {
        // A 3.17 WorkspaceSymbol whose location has no range (uri-only), plus one garbage
        // element that must not sink the batch, plus a null-result shape.
        let result = json!([
            {"name": "Renderer", "kind": 5, "location": {"uri": "file:///proj/src/lib.rs"}},
            {"kind": 5}, // no name — dropped
            {"name": "no_location", "kind": 12} // no location — dropped
        ]);
        let syms = parse_workspace_symbols(result);
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "Renderer");
        assert_eq!(syms[0].kind, lsp_types::SymbolKind::CLASS);
        assert_eq!(syms[0].location.range.start.line, 0);
        assert_eq!(syms[0].location.range.end.character, 0);
        assert_eq!(syms[0].container_name, None);

        assert!(parse_workspace_symbols(Value::Null).is_empty());
        assert!(parse_workspace_symbols(json!({"not": "an array"})).is_empty());
    }

    /// The full decode arm: a canned `workspace/symbol` JSON response routed through the reader
    /// dispatch resolves the pending entry and lands as [`LspEvent::WorkspaceSymbols`] with the
    /// generation stamped at request time.
    #[test]
    fn workspace_symbol_response_decodes_through_route() {
        let (events_tx, events_rx) = mpsc::channel();
        let (to_writer, _writer_rx) = mpsc::channel();
        let notifier: Notifier = Arc::new(|| {});
        let shutting_down = Arc::new(AtomicBool::new(false));
        let pending: Arc<Mutex<HashMap<i64, Pending>>> = Arc::new(Mutex::new(HashMap::new()));
        pending.lock().unwrap_or_else(|p| p.into_inner()).insert(
            7,
            Pending { kind: PendingKind::WorkspaceSymbols { generation: 3 }, sent: Instant::now() },
        );

        let msg = json!({
            "jsonrpc": "2.0",
            "id": 7,
            "result": [
                {"name": "main", "kind": 12, "location": {
                    "uri": "file:///proj/main.c",
                    "range": {"start": {"line": 5, "character": 0},
                              "end": {"line": 5, "character": 4}}
                }},
                {"name": "Config", "kind": 23, "location": {"uri": "file:///proj/cfg.rs"}}
            ]
        });
        route(ServerId(1), msg, &pending, &to_writer, &events_tx, &notifier, &shutting_down);

        assert!(pending.lock().unwrap().is_empty(), "pending entry must be consumed");
        match events_rx.try_recv() {
            Ok((sid, Raw::Lsp(LspEvent::WorkspaceSymbols { generation, symbols }))) => {
                assert_eq!(sid, ServerId(1));
                assert_eq!(generation, 3);
                assert_eq!(symbols.len(), 2);
                assert_eq!(symbols[0].name, "main");
                assert_eq!(symbols[0].location.range.start.line, 5);
                assert_eq!(symbols[1].name, "Config");
                assert_eq!(symbols[1].kind, lsp_types::SymbolKind::STRUCT);
            }
            other => panic!("expected WorkspaceSymbols event, got {other:?}"),
        }
    }
}
