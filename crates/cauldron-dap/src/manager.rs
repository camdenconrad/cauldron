//! The app-facing coordinator: one debug session, stored breakpoints, and the pump.
//!
//! All manager methods run on the UI thread and never block: outbound traffic goes through the
//! session's writer thread, inbound arrives on one mpsc drained by [`DebugManager::pump`] once
//! per frame (top of `update()`). The handshake state machine advances HERE, driven purely by
//! reader events — `launch()` returns as soon as the adapter process is spawned and the
//! `initialize` request is queued.
//!
//! Breakpoints are owned by the manager (per file, 1-based lines) so they survive across
//! sessions: a fresh launch replays them all between the `initialized` event and
//! `configurationDone`; edits while live are pushed immediately.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::session::{PendingKind, Raw, Session};
use crate::{AdapterKind, DebugEvent, Notifier};

/// After `disconnect` (with terminateDebuggee) goes out, the adapter gets this long to exit
/// on its own before the child is hard-killed by the next pump.
const DISCONNECT_GRACE: Duration = Duration::from_millis(1500);

pub struct DebugManager {
    session: Option<Session>,
    /// Per-file breakpoint lines (1-based), the source of truth across sessions.
    breakpoints: HashMap<PathBuf, Vec<(u32, Option<String>)>>,
    events_rx: Receiver<Raw>,
    events_tx: Sender<Raw>,
    notifier: Notifier,
    /// Status chip: "idle" | "launching" | "running" | "stopped" | "exited".
    state: &'static str,
    /// Debounces the Terminated event (adapters send `terminated` AND then EOF).
    terminated_emitted: bool,
    /// Events produced outside pump (e.g. spawn failure), drained by the next pump.
    outbox: Vec<DebugEvent>,
    /// Break on exceptions (adapter-advertised filters; the "uncaught" tier + defaults).
    break_on_exceptions: bool,
}

impl DebugManager {
    pub fn new(notifier: Notifier) -> Self {
        let (events_tx, events_rx) = mpsc::channel();
        Self {
            session: None,
            breakpoints: HashMap::new(),
            events_rx,
            events_tx,
            notifier,
            state: "idle",
            terminated_emitted: false,
            outbox: Vec::new(),
            break_on_exceptions: true,
        }
    }

    pub fn break_on_exceptions(&self) -> bool {
        self.break_on_exceptions
    }

    /// Toggle exception breakpoints; applies to a LIVE session immediately (the DAP call
    /// replaces the whole filter set) and to every later launch.
    pub fn set_break_on_exceptions(&mut self, on: bool) {
        self.break_on_exceptions = on;
        if let Some(s) = self.session.as_mut() {
            if s.breakpoints_ok {
                let filters = exception_filters(&s.caps, on);
                s.request(
                    "setExceptionBreakpoints",
                    json!({ "filters": filters }),
                    PendingKind::Generic,
                );
            }
        }
    }

    /// Spawn `kind`'s adapter and start the async handshake for `program`. Returns once the
    /// process is up and `initialize` is queued — everything after that arrives via [`pump`]
    /// ([`DebugEvent::Started`] on success, [`DebugEvent::Error`] on a failed handshake).
    ///
    /// [`pump`]: DebugManager::pump
    pub fn launch(
        &mut self,
        kind: AdapterKind,
        program: &Path,
        args: &[String],
        cwd: &Path,
    ) -> Result<(), String> {
        self.launch_full(kind, program, args, cwd, None, &[])
    }

    /// [`launch`](Self::launch) with an explicit interpreter pair for debugpy — the project
    /// venv's python, so debugging sees the same packages as Run (PEP-668 distros keep
    /// project deps venv-only). Ignored by other adapters.
    pub fn launch_with_python(
        &mut self,
        kind: AdapterKind,
        program: &Path,
        args: &[String],
        cwd: &Path,
        python: Option<crate::PythonEnv>,
    ) -> Result<(), String> {
        self.launch_full(kind, program, args, cwd, python, &[])
    }

    /// The full launch: interpreter pair for debugpy AND run-configuration env extras —
    /// Run honored RunConfig.env, debug silently dropped it.
    pub fn launch_full(
        &mut self,
        kind: AdapterKind,
        program: &Path,
        args: &[String],
        cwd: &Path,
        python: Option<crate::PythonEnv>,
        env: &[(String, String)],
    ) -> Result<(), String> {
        if self.session.is_some() {
            return Err("a debug session is already running".into());
        }
        // A stale channel could replay a previous session's tail into the new one.
        let (events_tx, events_rx) = mpsc::channel();
        self.events_tx = events_tx;
        self.events_rx = events_rx;
        self.terminated_emitted = false;
        match Session::spawn(
            kind,
            program,
            args,
            cwd,
            python,
            self.events_tx.clone(),
            Arc::clone(&self.notifier),
        ) {
            Ok(mut session) => {
                session.env = env.to_vec();
                self.session = Some(session);
                self.state = "launching";
                Ok(())
            }
            Err(e) => Err(format!("failed to spawn {}: {e}", kind.display_name())),
        }
    }

    /// Replace the breakpoint set for `path` (1-based lines, each with an optional CONDITION
    /// expression the adapter evaluates — the debuggee only stops when it is truthy). Always
    /// stored; pushed to the adapter immediately when a session is live and past its
    /// `initialized` event.
    pub fn set_breakpoints(&mut self, path: &Path, bps: Vec<(u32, Option<String>)>) {
        if bps.is_empty() {
            self.breakpoints.remove(path);
        } else {
            self.breakpoints.insert(path.to_path_buf(), bps.clone());
        }
        if let Some(s) = self.session.as_mut() {
            if s.breakpoints_ok {
                send_breakpoints(s, path, &bps);
            }
        }
    }

    /// Resume the stopped thread (`continue` request).
    pub fn continue_run(&mut self) {
        self.thread_request("continue", PendingKind::Continue);
    }

    /// Step over (`next` request). The debuggee runs until the step completes; unlike
    /// `continue` a step gets no `continued` event, so the state is advanced to "running"
    /// here (the following `stopped` event flips it back). Without this the toolbar showed
    /// stopped controls mid-step and let a second step race the first.
    pub fn next(&mut self) {
        self.state = "running";
        self.thread_request("next", PendingKind::Generic);
    }

    /// Step into (`stepIn` request).
    pub fn step_in(&mut self) {
        self.state = "running";
        self.thread_request("stepIn", PendingKind::Generic);
    }

    /// Step out (`stepOut` request).
    pub fn step_out(&mut self) {
        self.state = "running";
        self.thread_request("stepOut", PendingKind::Generic);
    }

    /// Interrupt the running debuggee (`pause` request); the halt arrives as
    /// [`DebugEvent::Stopped`] with reason "pause".
    pub fn pause(&mut self) {
        self.thread_request("pause", PendingKind::Generic);
    }

    /// End the session: `disconnect` with `terminateDebuggee: true`, then a hard kill of the
    /// adapter child if it hasn't exited within the grace window (enforced by [`pump`]).
    ///
    /// [`pump`]: DebugManager::pump
    pub fn stop(&mut self) {
        if let Some(s) = self.session.as_mut() {
            s.request(
                "disconnect",
                json!({"terminateDebuggee": true}),
                PendingKind::Disconnect,
            );
            s.kill_deadline = Some(Instant::now() + DISCONNECT_GRACE);
        }
    }

    /// `stackTrace` for the last stopped thread → [`DebugEvent::Stack`]. Fired automatically
    /// on every stop; call it again only to refresh.
    pub fn request_stack(&mut self) {
        if let Some(s) = self.session.as_mut() {
            let tid = s.thread_id;
            s.request(
                "stackTrace",
                json!({"threadId": tid, "startFrame": 0, "levels": 64}),
                PendingKind::StackTrace,
            );
        }
    }

    /// `threads` → [`DebugEvent::Threads`]. Fired on every stop; call again to refresh.
    pub fn request_threads(&mut self) {
        if let Some(s) = self.session.as_mut() {
            s.request("threads", json!({}), PendingKind::Threads);
        }
    }

    /// Switch the active thread: subsequent continue/step/stack act on `tid`, and its stack is
    /// re-fetched immediately (clicking a thread in the view jumps to its frames).
    pub fn set_active_thread(&mut self, tid: i64) {
        if let Some(s) = self.session.as_mut() {
            s.thread_id = tid;
        }
        self.request_stack();
    }

    /// The thread the debugger's controls currently act on.
    pub fn active_thread(&self) -> Option<i64> {
        self.session.as_ref().map(|s| s.thread_id)
    }

    /// `scopes` for one frame → [`DebugEvent::Scopes`].
    pub fn request_scopes(&mut self, frame_id: i64) {
        if let Some(s) = self.session.as_mut() {
            s.request(
                "scopes",
                json!({"frameId": frame_id}),
                PendingKind::Scopes { frame_id },
            );
        }
    }

    /// `variables` for a scope's / variable's reference → [`DebugEvent::Variables`].
    pub fn request_variables(&mut self, variables_reference: i64) {
        if let Some(s) = self.session.as_mut() {
            s.request(
                "variables",
                json!({"variablesReference": variables_reference}),
                PendingKind::Variables {
                    reference: variables_reference,
                },
            );
        }
    }

    /// Evaluate `expr` in the debug REPL (optionally against one frame) →
    /// [`DebugEvent::Evaluated`] carrying `tag` back. The tag is how the app tells a console
    /// eval from a Watches-panel eval — DAP itself echoes nothing.
    pub fn evaluate_tagged(&mut self, expr: &str, frame_id: Option<i64>, tag: u64) {
        self.evaluate_ctx(expr, frame_id, tag, "repl");
    }

    /// Watch-context eval: adapters treat "watch" as side-effect-averse (no implicit calls in
    /// some runtimes) — the right context for expressions re-run on every stop.
    pub fn evaluate_watch(&mut self, expr: &str, frame_id: Option<i64>, tag: u64) {
        self.evaluate_ctx(expr, frame_id, tag, "watch");
    }

    fn evaluate_ctx(&mut self, expr: &str, frame_id: Option<i64>, tag: u64, context: &str) {
        if let Some(s) = self.session.as_mut() {
            let mut args = json!({"expression": expr, "context": context});
            if let Some(fid) = frame_id {
                args["frameId"] = json!(fid);
            }
            s.request("evaluate", args, PendingKind::Evaluate { tag });
        }
    }

    /// Console eval (tag 0) — see [`Self::evaluate_tagged`].
    pub fn evaluate(&mut self, expr: &str, frame_id: Option<i64>) {
        self.evaluate_tagged(expr, frame_id, 0);
    }

    /// Drain events + advance the handshake state machine + enforce the disconnect grace kill.
    /// Call once per frame, first thing.
    pub fn pump(&mut self) -> Vec<DebugEvent> {
        let mut out: Vec<DebugEvent> = std::mem::take(&mut self.outbox);

        while let Ok(raw) = self.events_rx.try_recv() {
            match raw {
                Raw::InitResult(caps) => {
                    // Capabilities are in; fire `launch` with per-adapter arguments.
                    if let Some(s) = self.session.as_mut() {
                        s.caps = caps;
                        let args = launch_arguments(s);
                        s.request("launch", args, PendingKind::Launch);
                    }
                }
                Raw::Initialized => {
                    // Breakpoint window is open: replay every stored file, then finish.
                    if let Some(s) = self.session.as_mut() {
                        s.breakpoints_ok = true;
                        let bps: Vec<(PathBuf, Vec<(u32, Option<String>)>)> = self
                            .breakpoints
                            .iter()
                            .map(|(p, l)| (p.clone(), l.clone()))
                            .collect();
                        for (path, lines) in &bps {
                            send_breakpoints(s, path, lines);
                        }
                        // Exception breakpoints ride the same configuration window. Sent
                        // even when OFF: an explicit empty set beats relying on adapter
                        // defaults (debugpy enables "uncaught" on its own otherwise).
                        let filters = exception_filters(&s.caps, self.break_on_exceptions);
                        s.request(
                            "setExceptionBreakpoints",
                            json!({ "filters": filters }),
                            PendingKind::Generic,
                        );
                        s.request(
                            "configurationDone",
                            json!({}),
                            PendingKind::ConfigurationDone,
                        );
                    }
                }
                Raw::ConfigDone => {
                    self.state = "running";
                    out.push(DebugEvent::Started);
                }
                Raw::Failed { message, fatal } => {
                    out.push(DebugEvent::Error(message));
                    if fatal {
                        self.teardown(&mut out);
                    }
                }
                Raw::Ev(ev) => {
                    match &ev {
                        DebugEvent::Stopped { thread_id, .. } => {
                            self.state = "stopped";
                            if let Some(s) = self.session.as_mut() {
                                s.thread_id = *thread_id;
                            }
                            out.push(ev);
                            // The app always wants the stack on a stop — fire it unasked, plus
                            // the thread list (for the threads view).
                            self.request_stack();
                            self.request_threads();
                            continue;
                        }
                        DebugEvent::Continued => self.state = "running",
                        DebugEvent::Exited { .. } => self.state = "exited",
                        DebugEvent::Terminated => {
                            self.state = "exited";
                            if self.terminated_emitted {
                                continue;
                            }
                            self.terminated_emitted = true;
                        }
                        _ => {}
                    }
                    out.push(ev);
                }
                Raw::Eof => {
                    if self.state != "exited" && self.state != "idle" {
                        self.state = "exited";
                    }
                    self.teardown(&mut out);
                }
            }
        }

        // Disconnect grace: the adapter was asked to quit — make sure it actually does.
        if let Some(s) = self.session.as_mut() {
            let expired = s.kill_deadline.is_some_and(|d| d <= Instant::now());
            let dead = s.child.try_wait().ok().flatten().is_some();
            if dead || expired {
                self.teardown(&mut out);
            }
        }
        out
    }

    /// A session exists (spawned and not yet torn down).
    pub fn is_running(&self) -> bool {
        self.session.is_some()
    }

    /// The debuggee is halted at a breakpoint/step/pause.
    pub fn is_stopped(&self) -> bool {
        self.state == "stopped"
    }

    /// Status-chip text: "idle" | "launching" | "running" | "stopped" | "exited".
    pub fn state(&self) -> &str {
        self.state
    }

    // ------------------------------------------------------------------------------------------

    /// Requests that act on the last stopped thread (continue/step/pause).
    fn thread_request(&mut self, command: &'static str, kind: PendingKind) {
        if let Some(s) = self.session.as_mut() {
            let tid = s.thread_id;
            s.request(command, json!({"threadId": tid}), kind);
        }
    }

    /// Reap the child and drop the session, emitting the final Terminated exactly once.
    fn teardown(&mut self, out: &mut Vec<DebugEvent>) {
        if let Some(mut s) = self.session.take() {
            let _ = s.child.kill();
            let _ = s.child.wait();
        }
        if !self.terminated_emitted && self.state != "idle" {
            self.terminated_emitted = true;
            self.state = "exited";
            out.push(DebugEvent::Terminated);
        }
    }
}

impl Drop for DebugManager {
    /// Never leave an adapter (and its debuggee) orphaned past the manager.
    fn drop(&mut self) {
        if let Some(mut s) = self.session.take() {
            let _ = s.child.kill();
            let _ = s.child.wait();
        }
    }
}

// -------------------------------------------------------------------------------------------

/// Per-adapter `launch` arguments. lldb-dap wants stopOnEntry spelled out; debugpy wants its
/// console and justMyCode knobs (internalConsole = no runInTerminal round-trip).
fn launch_arguments(s: &Session) -> Value {
    let program = s.program.display().to_string();
    let cwd = s.cwd.display().to_string();
    match s.kind {
        AdapterKind::LldbDap => {
            let mut v = json!({
                "program": program,
                "args": s.args,
                "cwd": cwd,
                "stopOnEntry": false,
            });
            // lldb-dap takes env as "K=V" strings.
            if !s.env.is_empty() {
                v["env"] = json!(s.env.iter().map(|(k, val)| format!("{k}={val}")).collect::<Vec<_>>());
            }
            v
        }
        AdapterKind::Debugpy => {
            // `-m:<module>` (from a pytest/uvicorn run config) launches debugpy in MODULE mode —
            // `{"module": "pytest"}` — instead of running a script path. A bare program is a
            // normal script.
            let mut v = if let Some(module) = program.strip_prefix("-m:") {
                json!({
                    "module": module,
                    "args": s.args,
                    "cwd": cwd,
                    "console": "internalConsole",
                    "justMyCode": true,
                })
            } else {
                json!({
                    "program": program,
                    "args": s.args,
                    "cwd": cwd,
                    "console": "internalConsole",
                    "justMyCode": true,
                })
            };
            // The debuggee runs under the project venv's interpreter — same packages as Run.
            if let Some(py) = &s.python {
                v["python"] = json!(py.debuggee.display().to_string());
            }
            if !s.env.is_empty() {
                let m: serde_json::Map<String, Value> =
                    s.env.iter().map(|(k, val)| (k.clone(), json!(val))).collect();
                v["env"] = Value::Object(m);
            }
            v
        }
        // netcoredbg speaks the VS Code coreclr launch shape: `program` is the managed DLL,
        // stopAtEntry mirrors lldb-dap's stopOnEntry, no runInTerminal round-trip.
        AdapterKind::Netcoredbg => json!({
            "program": program,
            "args": s.args,
            "cwd": cwd,
            "env": s.env.iter().cloned().collect::<std::collections::HashMap<_, _>>(),
            "stopAtEntry": false,
        }),
    }
}

/// Which of the adapter's advertised exception filters to enable. ON = every filter the
/// adapter marks `default: true` plus any "uncaught" tier (the JetBrains default — caught
/// exceptions that get handled shouldn't stop the program); OFF = none. Pure — unit-tested.
fn exception_filters(caps: &Value, on: bool) -> Vec<String> {
    if !on {
        return Vec::new();
    }
    caps.get("exceptionBreakpointFilters")
        .and_then(|f| f.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|f| {
                    let id = f.get("filter")?.as_str()?;
                    let default = f.get("default").and_then(|d| d.as_bool()).unwrap_or(false);
                    (default || id.to_lowercase().contains("uncaught")).then(|| id.to_string())
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `setBreakpoints`: the full desired set for one source file (DAP replaces, never appends).
/// Legacy `lines` is included alongside `breakpoints` for maximum adapter compatibility.
fn send_breakpoints(s: &mut Session, path: &Path, bps: &[(u32, Option<String>)]) {
    let lines: Vec<u32> = bps.iter().map(|(l, _)| *l).collect();
    let bps: Vec<Value> = bps
        .iter()
        .map(|(l, cond)| match cond {
            Some(c) if !c.trim().is_empty() => json!({"line": l, "condition": c}),
            _ => json!({"line": l}),
        })
        .collect();
    s.request(
        "setBreakpoints",
        json!({
            "source": {"path": path.display().to_string()},
            "breakpoints": bps,
            "lines": lines,
        }),
        PendingKind::SetBreakpoints {
            path: path.to_path_buf(),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// debugpy's real capability shape: "raised" (default false) + "uncaught" (default true)
    /// + "userUnhandled" (default false). ON keeps uncaught only; OFF sends the empty set.
    #[test]
    fn exception_filters_pick_uncaught_and_defaults() {
        let caps = serde_json::json!({
            "exceptionBreakpointFilters": [
                {"filter": "raised", "label": "Raised Exceptions", "default": false},
                {"filter": "uncaught", "label": "Uncaught Exceptions", "default": true},
                {"filter": "userUnhandled", "label": "User Uncaught", "default": false},
            ]
        });
        assert_eq!(exception_filters(&caps, true), vec!["uncaught"]);
        assert!(exception_filters(&caps, false).is_empty());
        // lldb-dap style: cpp_throw/cpp_catch, neither default → ON enables none (adapter
        // stops on its own defaults being empty is the honest reading).
        let caps = serde_json::json!({
            "exceptionBreakpointFilters": [
                {"filter": "cpp_throw", "default": false},
                {"filter": "cpp_catch", "default": false},
            ]
        });
        assert!(exception_filters(&caps, true).is_empty());
        // No filters advertised at all.
        assert!(exception_filters(&serde_json::json!({}), true).is_empty());
    }
}
