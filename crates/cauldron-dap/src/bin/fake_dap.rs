//! fake-dap — a scripted stdio debug adapter for the headless e2e tests.
//!
//! Speaks real Content-Length framing (via the crate's own [`cauldron_dap::transport`]) and
//! plays the canonical breakpoint session. The e2e test shims it onto PATH as `lldb-dap` so the
//! FULL production path — spawn, async handshake, breakpoint replay, reader dispatch, stop
//! grace-kill — is exercised with zero wall-clock cost.
//!
//! Script (mirrors what lldb-dap/debugpy actually do):
//! initialize → capabilities; launch → `initialized` event, then the launch ack;
//! setBreakpoints → verifies every line EXCEPT 999 (the deliberately-bogus one);
//! configurationDone → ack, one stdout `output` event, then `stopped` (reason "breakpoint",
//! threadId 7) if any breakpoint is set, else straight to exited+terminated;
//! stackTrace/scopes/variables/evaluate → canned bodies; continue → ack then exited+terminated;
//! disconnect → ack and quit.

use serde_json::{json, Value};

use cauldron_dap::transport::{read_message, write_message};

fn main() {
    let stdin = std::io::stdin();
    let mut r = std::io::BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut w = stdout.lock();
    let mut header = String::new();
    let mut body = Vec::new();
    let mut seq = 0_i64;
    let mut have_breakpoints = false;

    while let Ok(Some(msg)) = read_message(&mut r, &mut header, &mut body) {
        let req_seq = msg.get("seq").cloned().unwrap_or(Value::Null);
        let command = msg
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let args = msg.get("arguments").cloned().unwrap_or(Value::Null);
        match command.as_str() {
            "initialize" => {
                // Honest fake: refuse a client that doesn't identify as 1-based.
                let ok = args["clientID"] == "cauldron"
                    && args["linesStartAt1"] == true
                    && args["columnsStartAt1"] == true;
                if !ok {
                    respond_err(&mut w, &mut seq, &req_seq, &command, "bad initialize args");
                    return;
                }
                respond(
                    &mut w,
                    &mut seq,
                    &req_seq,
                    &command,
                    json!({
                        "supportsConfigurationDoneRequest": true,
                        "supportsEvaluateForHovers": true,
                    }),
                );
            }
            "launch" => {
                event(&mut w, &mut seq, "initialized", json!({}));
                respond(&mut w, &mut seq, &req_seq, &command, Value::Null);
            }
            "setBreakpoints" => {
                let lines: Vec<u64> = args["breakpoints"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|b| b["line"].as_u64()).collect())
                    .unwrap_or_default();
                have_breakpoints = !lines.is_empty();
                let bps: Vec<Value> = lines
                    .iter()
                    .map(|l| json!({"verified": *l != 999, "line": l}))
                    .collect();
                respond(
                    &mut w,
                    &mut seq,
                    &req_seq,
                    &command,
                    json!({"breakpoints": bps}),
                );
            }
            "configurationDone" => {
                respond(&mut w, &mut seq, &req_seq, &command, Value::Null);
                event(
                    &mut w,
                    &mut seq,
                    "output",
                    json!({
                        "category": "stdout", "output": "hello from fake debuggee\n",
                    }),
                );
                if have_breakpoints {
                    event(
                        &mut w,
                        &mut seq,
                        "stopped",
                        json!({
                            "reason": "breakpoint", "threadId": 7, "description": "hit it",
                        }),
                    );
                } else {
                    finish(&mut w, &mut seq);
                }
            }
            "stackTrace" => {
                assert_eq!(
                    args["threadId"], 7,
                    "stackTrace must target the stopped thread"
                );
                respond(
                    &mut w,
                    &mut seq,
                    &req_seq,
                    &command,
                    json!({
                        "stackFrames": [
                            {"id": 1000, "name": "work", "line": 4,
                             "source": {"path": "/tmp/fake/main.c"}},
                            {"id": 1001, "name": "main", "line": 9,
                             "source": {"path": "/tmp/fake/main.c"}},
                        ],
                        "totalFrames": 2,
                    }),
                );
            }
            "scopes" => {
                assert_eq!(
                    args["frameId"], 1000,
                    "scopes must echo the requested frame"
                );
                respond(
                    &mut w,
                    &mut seq,
                    &req_seq,
                    &command,
                    json!({
                        "scopes": [{"name": "Locals", "variablesReference": 500, "expensive": false}],
                    }),
                );
            }
            "variables" => {
                assert_eq!(args["variablesReference"], 500);
                respond(
                    &mut w,
                    &mut seq,
                    &req_seq,
                    &command,
                    json!({
                        "variables": [
                            {"name": "total", "value": "3", "type": "int", "variablesReference": 0},
                            {"name": "box", "value": "{…}", "type": "Box", "variablesReference": 501},
                        ],
                    }),
                );
            }
            "evaluate" => {
                assert_eq!(args["context"], "repl");
                respond(
                    &mut w,
                    &mut seq,
                    &req_seq,
                    &command,
                    json!({
                        "result": "42", "variablesReference": 0,
                    }),
                );
            }
            "continue" => {
                respond(
                    &mut w,
                    &mut seq,
                    &req_seq,
                    &command,
                    json!({
                        "allThreadsContinued": true,
                    }),
                );
                finish(&mut w, &mut seq);
            }
            "disconnect" => {
                respond(&mut w, &mut seq, &req_seq, &command, Value::Null);
                return;
            }
            _ => respond(&mut w, &mut seq, &req_seq, &command, Value::Null),
        }
    }
}

fn respond(
    w: &mut impl std::io::Write,
    seq: &mut i64,
    req_seq: &Value,
    command: &str,
    body: Value,
) {
    *seq += 1;
    let mut v = json!({
        "seq": *seq, "type": "response", "request_seq": req_seq,
        "success": true, "command": command,
    });
    if !body.is_null() {
        v["body"] = body;
    }
    let _ = write_message(w, &v);
}

fn respond_err(
    w: &mut impl std::io::Write,
    seq: &mut i64,
    req_seq: &Value,
    command: &str,
    message: &str,
) {
    *seq += 1;
    let _ = write_message(
        w,
        &json!({
            "seq": *seq, "type": "response", "request_seq": req_seq,
            "success": false, "command": command, "message": message,
        }),
    );
}

fn event(w: &mut impl std::io::Write, seq: &mut i64, event: &str, body: Value) {
    *seq += 1;
    let _ = write_message(
        w,
        &json!({
            "seq": *seq, "type": "event", "event": event, "body": body,
        }),
    );
}

/// The debuggee ran to completion: exited(0) then terminated.
fn finish(w: &mut impl std::io::Write, seq: &mut i64) {
    event(w, seq, "exited", json!({"exitCode": 0}));
    event(w, seq, "terminated", json!({}));
}
