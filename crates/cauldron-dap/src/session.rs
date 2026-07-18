//! One running debug adapter: process + writer/reader/stderr threads + dispatch.
//!
//! THREADS (per session, all plain `std::thread` — the cider PTY template):
//! - **writer** owns `ChildStdin`: drains an mpsc of [`Outgoing`] and frames them. A wedged
//!   adapter (not draining its pipe) blocks THIS thread, never a frame.
//! - **reader** owns `BufReader<ChildStdout>`: frames messages, correlates responses against the
//!   pending map by `request_seq`, converts adapter events into crate-internal [`Raw`]s, replies
//!   to adapter→client reverse requests (via a writer-channel clone), and wakes the UI through
//!   the injected notifier.
//! - **stderr** drains child stderr into `log::debug!` so the pipe can never fill up.
//!
//! `DebugManager::pump` folds the [`Raw`]s into public [`DebugEvent`]s and drives the
//! handshake state machine on the UI thread.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Instant;

use serde_json::{json, Value};

use crate::{transport, AdapterKind, DebugEvent, Frame, Notifier, Scope, Var};

/// Everything the writer thread can be asked to put on the wire.
#[derive(Debug)]
pub(crate) enum Outgoing {
    Request {
        seq: i64,
        command: &'static str,
        arguments: Value,
    },
    /// Reply to an adapter→client reverse request (e.g. `runInTerminal`). `request_seq` is
    /// echoed verbatim; we always refuse (`success: false`) — Cauldron runs debuggees on the
    /// adapter's internal console, never a user terminal.
    Reject {
        request_seq: Value,
        command: String,
        message: String,
    },
}

/// What a request seq is waiting for — decides how its response is decoded.
#[derive(Debug, Clone)]
pub(crate) enum PendingKind {
    Initialize,
    Launch,
    SetBreakpoints {
        path: PathBuf,
    },
    ConfigurationDone,
    Continue,
    StackTrace,
    Threads,
    Scopes {
        frame_id: i64,
    },
    Variables {
        reference: i64,
    },
    Evaluate { tag: u64 },
    Disconnect,
    /// A request whose success response carries nothing we act on (next/stepIn/stepOut/pause —
    /// the interesting part arrives later as a `stopped` event).
    Generic,
}

#[derive(Debug)]
pub(crate) struct Pending {
    pub kind: PendingKind,
    #[allow(dead_code)] // timeout sweeps come with multi-session support; kept for parity.
    pub sent: Instant,
}

/// Crate-internal reader→manager events; the manager folds these into public [`DebugEvent`]s
/// and handshake-state transitions on the UI thread.
#[derive(Debug)]
pub(crate) enum Raw {
    /// The initialize response landed (body = adapter capabilities) — time to send `launch`.
    InitResult(Value),
    /// The `initialized` event — time for `setBreakpoints` + `configurationDone`.
    Initialized,
    /// `configurationDone` acked — the debuggee is running.
    ConfigDone,
    /// A request failed (`success: false`); `fatal` requests tear the session down.
    Failed { message: String, fatal: bool },
    /// A decoded event/response with a public shape.
    Ev(DebugEvent),
    /// Reader hit EOF — the adapter process is gone.
    Eof,
}

pub(crate) struct Session {
    pub kind: AdapterKind,
    pub child: Child,
    to_writer: Sender<Outgoing>,
    pending: Arc<Mutex<HashMap<i64, Pending>>>,
    next_seq: i64,
    /// Raw `body` of the initialize response (adapter capabilities).
    pub caps: Value,
    /// Thread id from the last `stopped` event — what continue/step/pause act on.
    pub thread_id: i64,
    /// Set once the `initialized` event arrived: `setBreakpoints` is legal from here on.
    pub breakpoints_ok: bool,
    /// Armed by `disconnect`; the manager hard-kills the child when this passes.
    pub kill_deadline: Option<Instant>,
    // Launch parameters, held until the initialize response arrives.
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// Debugpy only: the debuggee runs under `python.debuggee` (project venv).
    pub python: Option<crate::PythonEnv>,
    /// Run-configuration environment extras, injected into the launch request.
    pub env: Vec<(String, String)>,
}

impl Session {
    /// Spawn the adapter process + its three threads and fire the `initialize` request.
    /// Never blocks on the child: the handshake completes asynchronously via [`Raw`]s.
    pub(crate) fn spawn(
        kind: AdapterKind,
        program: &std::path::Path,
        args: &[String],
        cwd: &std::path::Path,
        python: Option<crate::PythonEnv>,
        events_tx: Sender<Raw>,
        notifier: Notifier,
    ) -> std::io::Result<Self> {
        let mut cmd = match kind {
            AdapterKind::LldbDap => Command::new("lldb-dap"),
            AdapterKind::Debugpy => {
                // The adapter runs under python.host (a python that can import debugpy);
                // the DEBUGGEE interpreter is python.debuggee via launch_arguments.
                let mut c = match &python {
                    Some(py) => Command::new(&py.host),
                    None => Command::new("python3"),
                };
                c.args(["-m", "debugpy.adapter"]);
                c
            }
            AdapterKind::Netcoredbg => {
                let mut c = Command::new("netcoredbg");
                c.arg("--interpreter=vscode");
                c
            }
        };
        cmd.current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn()?;

        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let (to_writer, from_ui): (Sender<Outgoing>, Receiver<Outgoing>) = mpsc::channel();
        let pending: Arc<Mutex<HashMap<i64, Pending>>> = Arc::new(Mutex::new(HashMap::new()));

        // --- writer ------------------------------------------------------------------------
        std::thread::Builder::new()
            .name("dap-writer".into())
            .spawn(move || {
                let mut w = stdin;
                // The writer allocates the wire `seq` for rejects; requests carry theirs already.
                let mut reject_seq = 1_000_000_i64;
                while let Ok(out) = from_ui.recv() {
                    let v = match out {
                        Outgoing::Request {
                            seq,
                            command,
                            arguments,
                        } => json!({
                            "seq": seq, "type": "request",
                            "command": command, "arguments": arguments,
                        }),
                        Outgoing::Reject {
                            request_seq,
                            command,
                            message,
                        } => {
                            reject_seq += 1;
                            json!({
                                "seq": reject_seq, "type": "response", "request_seq": request_seq,
                                "success": false, "command": command, "message": message,
                            })
                        }
                    };
                    if transport::write_message(&mut w, &v).is_err() {
                        return; // pipe gone — reader EOF handles the rest
                    }
                }
            })?;

        // --- reader ------------------------------------------------------------------------
        {
            let pending = Arc::clone(&pending);
            let to_writer = to_writer.clone();
            let notifier = Arc::clone(&notifier);
            std::thread::Builder::new()
                .name("dap-reader".into())
                .spawn(move || {
                    reader_loop(stdout, pending, to_writer, events_tx, notifier);
                })?;
        }

        // --- stderr drain --------------------------------------------------------------------
        std::thread::Builder::new()
            .name("dap-stderr".into())
            .spawn(move || {
                let r = BufReader::new(stderr);
                for line in r.lines().map_while(Result::ok) {
                    log::debug!("dap stderr: {line}");
                }
            })?;

        let mut session = Self {
            kind,
            child,
            to_writer,
            pending,
            next_seq: 0,
            caps: Value::Null,
            thread_id: 1,
            breakpoints_ok: false,
            kill_deadline: None,
            program: program.to_path_buf(),
            args: args.to_vec(),
            cwd: cwd.to_path_buf(),
            python,
            env: Vec::new(),
        };
        session.request(
            "initialize",
            json!({
                "clientID": "cauldron",
                "clientName": "Cauldron",
                "adapterID": kind.display_name(),
                "linesStartAt1": true,
                "columnsStartAt1": true,
                "pathFormat": "path",
                "supportsVariableType": true,
                "supportsRunInTerminalRequest": false,
            }),
            PendingKind::Initialize,
        );
        Ok(session)
    }

    fn alloc_seq(&mut self, kind: PendingKind) -> i64 {
        self.next_seq += 1;
        self.pending.lock().unwrap_or_else(|p| p.into_inner()).insert(
            self.next_seq,
            Pending {
                kind,
                sent: Instant::now(),
            },
        );
        self.next_seq
    }

    pub(crate) fn request(&mut self, command: &'static str, arguments: Value, kind: PendingKind) {
        let seq = self.alloc_seq(kind);
        let _ = self.to_writer.send(Outgoing::Request {
            seq,
            command,
            arguments,
        });
    }
}

// -------------------------------------------------------------------------------------------
// reader-side dispatch
// -------------------------------------------------------------------------------------------

fn emit(events: &Sender<Raw>, notifier: &Notifier, ev: Raw) {
    let _ = events.send(ev);
    notifier();
}

fn reader_loop(
    stdout: std::process::ChildStdout,
    pending: Arc<Mutex<HashMap<i64, Pending>>>,
    to_writer: Sender<Outgoing>,
    events: Sender<Raw>,
    notifier: Notifier,
) {
    let mut r = BufReader::new(stdout);
    let mut header_buf = String::new();
    let mut body_buf = Vec::new();
    // EOF (Ok(None)) or a torn pipe (Err) both mean the process is gone.
    while let Ok(Some(msg)) = transport::read_message(&mut r, &mut header_buf, &mut body_buf) {
        route(msg, &pending, &to_writer, &events, &notifier);
    }
    pending.lock().unwrap_or_else(|p| p.into_inner()).clear();
    emit(&events, &notifier, Raw::Eof);
}

/// Route one inbound message by its `type`: response → pending decode; event → [`Raw`];
/// reverse request → polite refusal. Runs on the reader thread; only cheap parsing here.
/// Free function so the seq-correlation tests can drive it without a child process.
pub(crate) fn route(
    msg: Value,
    pending: &Arc<Mutex<HashMap<i64, Pending>>>,
    to_writer: &Sender<Outgoing>,
    events: &Sender<Raw>,
    notifier: &Notifier,
) {
    match msg.get("type").and_then(Value::as_str).unwrap_or("") {
        "response" => route_response(msg, pending, events, notifier),
        "event" => route_event(msg, events, notifier),
        "request" => {
            // Adapter→client reverse request (runInTerminal, startDebugging, …). Refuse:
            // launches use the internal console, and child sessions are out of scope for v1.
            let command = msg
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let _ = to_writer.send(Outgoing::Reject {
                request_seq: msg.get("seq").cloned().unwrap_or(Value::Null),
                message: format!("cauldron does not support {command}"),
                command,
            });
        }
        other => log::trace!("dap: unhandled message type {other:?}"),
    }
}

fn route_response(
    msg: Value,
    pending: &Arc<Mutex<HashMap<i64, Pending>>>,
    events: &Sender<Raw>,
    notifier: &Notifier,
) {
    let Some(req_seq) = msg.get("request_seq").and_then(Value::as_i64) else {
        return;
    };
    let Some(p) = pending.lock().unwrap_or_else(|p| p.into_inner()).remove(&req_seq) else {
        return;
    };
    let body = msg.get("body").cloned().unwrap_or(Value::Null);

    if msg.get("success").and_then(Value::as_bool) != Some(true) {
        let message = msg
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| {
                body.get("error")
                    .and_then(|e| e.get("format"))
                    .and_then(Value::as_str)
            })
            .unwrap_or("request failed")
            .to_string();
        // A failed evaluate keeps its TAG: watch rows show their own error inline instead of
        // dropping the correlation and spamming the console once per bad watch per stop.
        if let PendingKind::Evaluate { tag } = p.kind {
            emit(
                events,
                notifier,
                Raw::Ev(DebugEvent::Evaluated { tag, result: format!("⚠ {message}") }),
            );
            return;
        }
        // Handshake failures kill the session; a failed step just surfaces.
        let fatal = matches!(
            p.kind,
            PendingKind::Initialize | PendingKind::Launch | PendingKind::ConfigurationDone
        );
        if !matches!(p.kind, PendingKind::Disconnect) {
            emit(events, notifier, Raw::Failed { message, fatal });
        }
        return;
    }

    match p.kind {
        PendingKind::Initialize => emit(events, notifier, Raw::InitResult(body)),
        // Launch success carries nothing; the debuggee's fate arrives as events. (debugpy only
        // acks launch after configurationDone — never treat this as a sequencing point.)
        PendingKind::Launch | PendingKind::Generic | PendingKind::Disconnect => {}
        PendingKind::ConfigurationDone => emit(events, notifier, Raw::ConfigDone),
        PendingKind::Continue => emit(events, notifier, Raw::Ev(DebugEvent::Continued)),
        PendingKind::SetBreakpoints { path } => {
            let verified_lines = body
                .get("breakpoints")
                .and_then(Value::as_array)
                .map(|bps| {
                    bps.iter()
                        .filter(|b| b.get("verified").and_then(Value::as_bool) == Some(true))
                        .filter_map(|b| b.get("line").and_then(Value::as_u64))
                        .map(|l| l as u32)
                        .collect()
                })
                .unwrap_or_default();
            emit(
                events,
                notifier,
                Raw::Ev(DebugEvent::BreakpointsResolved {
                    path,
                    verified_lines,
                }),
            );
        }
        PendingKind::StackTrace => {
            let frames = body
                .get("stackFrames")
                .and_then(Value::as_array)
                .map(|fs| fs.iter().map(parse_frame).collect())
                .unwrap_or_default();
            emit(events, notifier, Raw::Ev(DebugEvent::Stack { frames }));
        }
        PendingKind::Threads => {
            let threads = body
                .get("threads")
                .and_then(Value::as_array)
                .map(|ts| {
                    ts.iter()
                        .map(|t| (i64_of(t, "id"), str_of(t, "name")))
                        .collect()
                })
                .unwrap_or_default();
            emit(events, notifier, Raw::Ev(DebugEvent::Threads { threads }));
        }
        PendingKind::Scopes { frame_id } => {
            let scopes = body
                .get("scopes")
                .and_then(Value::as_array)
                .map(|ss| {
                    ss.iter()
                        .map(|s| Scope {
                            name: str_of(s, "name"),
                            variables_reference: i64_of(s, "variablesReference"),
                        })
                        .collect()
                })
                .unwrap_or_default();
            emit(
                events,
                notifier,
                Raw::Ev(DebugEvent::Scopes { frame_id, scopes }),
            );
        }
        PendingKind::Variables { reference } => {
            let vars = body
                .get("variables")
                .and_then(Value::as_array)
                .map(|vs| {
                    vs.iter()
                        .map(|v| Var {
                            name: str_of(v, "name"),
                            value: str_of(v, "value"),
                            type_name: v.get("type").and_then(Value::as_str).map(str::to_string),
                            variables_reference: i64_of(v, "variablesReference"),
                        })
                        .collect()
                })
                .unwrap_or_default();
            emit(
                events,
                notifier,
                Raw::Ev(DebugEvent::Variables { reference, vars }),
            );
        }
        PendingKind::Evaluate { tag } => {
            emit(
                events,
                notifier,
                Raw::Ev(DebugEvent::Evaluated {
                    tag,
                    result: str_of(&body, "result"),
                }),
            );
        }
    }
}

fn route_event(msg: Value, events: &Sender<Raw>, notifier: &Notifier) {
    let body = msg.get("body").cloned().unwrap_or(Value::Null);
    match msg.get("event").and_then(Value::as_str).unwrap_or("") {
        "initialized" => emit(events, notifier, Raw::Initialized),
        "stopped" => {
            emit(
                events,
                notifier,
                Raw::Ev(DebugEvent::Stopped {
                    reason: str_of(&body, "reason"),
                    thread_id: body.get("threadId").and_then(Value::as_i64).unwrap_or(1),
                    description: body
                        .get("description")
                        .or_else(|| body.get("text"))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                }),
            );
        }
        "continued" => emit(events, notifier, Raw::Ev(DebugEvent::Continued)),
        "output" => {
            let category = body
                .get("category")
                .and_then(Value::as_str)
                .unwrap_or("console")
                .to_string();
            emit(
                events,
                notifier,
                Raw::Ev(DebugEvent::Output {
                    category,
                    text: str_of(&body, "output"),
                }),
            );
        }
        "exited" => {
            let code = body.get("exitCode").and_then(Value::as_i64).unwrap_or(0) as i32;
            emit(events, notifier, Raw::Ev(DebugEvent::Exited { code }));
        }
        "terminated" => emit(events, notifier, Raw::Ev(DebugEvent::Terminated)),
        // process/thread/module/breakpoint churn — nothing the v1 UI shows.
        other => log::trace!("dap: unhandled event {other:?}"),
    }
}

fn str_of(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn i64_of(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(Value::as_i64).unwrap_or(0)
}

fn parse_frame(f: &Value) -> Frame {
    Frame {
        id: i64_of(f, "id"),
        name: str_of(f, "name"),
        path: f
            .get("source")
            .and_then(|s| s.get("path"))
            .and_then(Value::as_str)
            .map(PathBuf::from),
        line: f.get("line").and_then(Value::as_u64).unwrap_or(0) as u32,
    }
}

// -------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// A pending map, writer/event channels, and a no-op notifier — `route` in a bottle.
    /// Everything `route` needs, bundled so the tests read cleanly.
    type Rig = (
        Arc<Mutex<HashMap<i64, Pending>>>,
        Sender<Outgoing>,
        Receiver<Outgoing>,
        Sender<Raw>,
        Receiver<Raw>,
        Notifier,
    );

    fn rig() -> Rig {
        let (wtx, wrx) = mpsc::channel();
        let (etx, erx) = mpsc::channel();
        (
            Arc::new(Mutex::new(HashMap::new())),
            wtx,
            wrx,
            etx,
            erx,
            Arc::new(|| {}),
        )
    }

    fn add_pending(pending: &Arc<Mutex<HashMap<i64, Pending>>>, seq: i64, kind: PendingKind) {
        pending.lock().unwrap().insert(
            seq,
            Pending {
                kind,
                sent: Instant::now(),
            },
        );
    }

    #[test]
    fn response_correlates_by_request_seq_and_consumes_the_pending_entry() {
        let (pending, wtx, _wrx, etx, erx, n) = rig();
        add_pending(&pending, 7, PendingKind::Evaluate { tag: 0 });
        add_pending(&pending, 8, PendingKind::StackTrace);
        let resp = json!({"seq": 40, "type": "response", "request_seq": 7, "success": true,
                          "command": "evaluate", "body": {"result": "42"}});
        route(resp.clone(), &pending, &wtx, &etx, &n);
        match erx.try_recv().unwrap() {
            Raw::Ev(DebugEvent::Evaluated { result, .. }) => assert_eq!(result, "42"),
            other => panic!("wrong event: {other:?}"),
        }
        // Seq 8 is still pending; seq 7 is gone, so a replay resolves nothing.
        assert_eq!(pending.lock().unwrap().len(), 1);
        route(resp, &pending, &wtx, &etx, &n);
        assert!(
            erx.try_recv().is_err(),
            "duplicate response must not re-fire"
        );
    }

    #[test]
    fn unknown_request_seq_is_ignored() {
        let (pending, wtx, _wrx, etx, erx, n) = rig();
        route(
            json!({"type": "response", "request_seq": 99, "success": true, "command": "x"}),
            &pending,
            &wtx,
            &etx,
            &n,
        );
        assert!(erx.try_recv().is_err());
    }

    #[test]
    fn failed_handshake_response_is_fatal_failed_evaluate_is_not() {
        let (pending, wtx, _wrx, etx, erx, n) = rig();
        add_pending(&pending, 1, PendingKind::Launch);
        add_pending(&pending, 2, PendingKind::Evaluate { tag: 0 });
        route(
            json!({"type": "response", "request_seq": 1, "success": false,
                   "command": "launch", "message": "no such program"}),
            &pending,
            &wtx,
            &etx,
            &n,
        );
        route(
            json!({"type": "response", "request_seq": 2, "success": false,
                   "command": "evaluate", "message": "bad expr"}),
            &pending,
            &wtx,
            &etx,
            &n,
        );
        match erx.try_recv().unwrap() {
            Raw::Failed { message, fatal } => {
                assert_eq!(message, "no such program");
                assert!(fatal);
            }
            other => panic!("wrong event: {other:?}"),
        }
        // A failed evaluate keeps its TAG and surfaces inline (watch rows own their errors)
        // instead of a console-spamming Failed.
        match erx.try_recv().unwrap() {
            Raw::Ev(DebugEvent::Evaluated { tag: 0, result }) => {
                assert_eq!(result, "⚠ bad expr");
            }
            other => panic!("wrong event: {other:?}"),
        }
    }

    #[test]
    fn stopped_event_decodes_reason_thread_and_description() {
        let (pending, wtx, _wrx, etx, erx, n) = rig();
        route(
            json!({"seq": 5, "type": "event", "event": "stopped",
                   "body": {"reason": "breakpoint", "threadId": 12, "description": "hit bp 1"}}),
            &pending,
            &wtx,
            &etx,
            &n,
        );
        match erx.try_recv().unwrap() {
            Raw::Ev(DebugEvent::Stopped {
                reason,
                thread_id,
                description,
            }) => {
                assert_eq!(reason, "breakpoint");
                assert_eq!(thread_id, 12);
                assert_eq!(description.as_deref(), Some("hit bp 1"));
            }
            other => panic!("wrong event: {other:?}"),
        }
    }

    #[test]
    fn set_breakpoints_response_reports_only_verified_lines() {
        let (pending, wtx, _wrx, etx, erx, n) = rig();
        add_pending(
            &pending,
            3,
            PendingKind::SetBreakpoints {
                path: PathBuf::from("/tmp/a.py"),
            },
        );
        route(
            json!({"type": "response", "request_seq": 3, "success": true,
                   "command": "setBreakpoints",
                   "body": {"breakpoints": [
                       {"verified": true, "line": 4},
                       {"verified": false, "line": 900},
                       {"verified": true, "line": 7}]}}),
            &pending,
            &wtx,
            &etx,
            &n,
        );
        match erx.try_recv().unwrap() {
            Raw::Ev(DebugEvent::BreakpointsResolved {
                path,
                verified_lines,
            }) => {
                assert_eq!(path, PathBuf::from("/tmp/a.py"));
                assert_eq!(verified_lines, vec![4, 7]);
            }
            other => panic!("wrong event: {other:?}"),
        }
    }

    #[test]
    fn stack_trace_response_parses_frames_with_and_without_source() {
        let (pending, wtx, _wrx, etx, erx, n) = rig();
        add_pending(&pending, 4, PendingKind::StackTrace);
        route(
            json!({"type": "response", "request_seq": 4, "success": true,
                   "command": "stackTrace",
                   "body": {"stackFrames": [
                       {"id": 1000, "name": "main", "line": 3,
                        "source": {"path": "/tmp/x.py"}},
                       {"id": 1001, "name": "<module>", "line": 0}]}}),
            &pending,
            &wtx,
            &etx,
            &n,
        );
        match erx.try_recv().unwrap() {
            Raw::Ev(DebugEvent::Stack { frames }) => {
                assert_eq!(frames.len(), 2);
                assert_eq!(
                    frames[0],
                    Frame {
                        id: 1000,
                        name: "main".into(),
                        path: Some(PathBuf::from("/tmp/x.py")),
                        line: 3,
                    }
                );
                assert_eq!(frames[1].path, None);
            }
            other => panic!("wrong event: {other:?}"),
        }
    }

    #[test]
    fn reverse_request_is_rejected_on_the_wire() {
        let (pending, wtx, wrx, etx, _erx, n) = rig();
        route(
            json!({"seq": 77, "type": "request", "command": "runInTerminal", "arguments": {}}),
            &pending,
            &wtx,
            &etx,
            &n,
        );
        match wrx.try_recv().unwrap() {
            Outgoing::Reject {
                request_seq,
                command,
                ..
            } => {
                assert_eq!(request_seq, json!(77));
                assert_eq!(command, "runInTerminal");
            }
            other => panic!("wrong outgoing: {other:?}"),
        }
    }
}
