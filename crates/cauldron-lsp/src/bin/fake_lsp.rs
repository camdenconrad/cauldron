//! fake-lsp — a scripted stdio language server for the headless e2e tests.
//!
//! Speaks real Content-Length framing (via the crate's own [`cauldron_lsp::transport`]) and
//! plays the scenario named by `FAKE_LSP_SCENARIO`. The e2e tests shim it onto PATH under a
//! real server's name (e.g. `clangd`) so the FULL production path — spawn, handshake,
//! negotiation, queue-gating, reader dispatch — is exercised with zero wall-clock cost.
//!
//! Scenarios:
//! - `code-actions`: advertises `codeActionProvider`; replies to `textDocument/codeAction`
//!   with one quickfix CodeAction carrying a changes-map WorkspaceEdit (edits deliberately
//!   ASCENDING — the client flattener must reverse them) plus one bare Command. On
//!   `workspace/executeCommand` it acks, then pushes the edit back the realistic way: a
//!   server→client `workspace/applyEdit` request.

use serde_json::{json, Value};

use cauldron_lsp::transport::{read_message, write_message};

fn main() {
    let scenario = std::env::var("FAKE_LSP_SCENARIO").unwrap_or_default();
    let stdin = std::io::stdin();
    let mut r = std::io::BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut w = stdout.lock();
    let mut header = String::new();
    let mut body = Vec::new();

    while let Ok(Some(msg)) = read_message(&mut r, &mut header, &mut body) {
        // Responses to requests WE sent (e.g. the applyEdit ack) carry an id but no method.
        let Some(method) = msg.get("method").and_then(Value::as_str) else { continue };
        let id = msg.get("id").cloned();
        match method {
            "initialize" => reply(
                &mut w,
                &id,
                json!({
                    "capabilities": {
                        "positionEncoding": "utf-8",
                        "textDocumentSync": {"change": 2},
                        "codeActionProvider": true,
                        "executeCommandProvider": {"commands": ["fake.fix"]},
                    }
                }),
            ),
            "shutdown" => reply(&mut w, &id, Value::Null),
            "exit" => return,
            "textDocument/codeAction" if scenario == "code-actions" => {
                let uri = msg["params"]["textDocument"]["uri"].as_str().unwrap_or("").to_string();
                reply(&mut w, &id, code_actions_result(&uri));
            }
            "workspace/executeCommand" if scenario == "code-actions" => {
                let uri = msg["params"]["arguments"][0].as_str().unwrap_or("").to_string();
                reply(&mut w, &id, Value::Null);
                // The realistic follow-up: push the command's edit as a server→client request.
                let _ = write_message(
                    &mut w,
                    &json!({
                        "jsonrpc": "2.0", "id": 999, "method": "workspace/applyEdit",
                        "params": {
                            "label": "fake command edit",
                            "edit": {"changes": {uri: [
                                {"range": {"start": {"line": 2, "character": 0},
                                           "end": {"line": 2, "character": 0}},
                                 "newText": "// via executeCommand\n"},
                            ]}}
                        }
                    }),
                );
            }
            _ if id.is_some() => reply(&mut w, &id, Value::Null), // unknown request: bland ack
            _ => {}                                               // notification: ignore
        }
    }
}

/// Frame one JSON-RPC response. `id` is echoed verbatim (None never happens for requests,
/// but a null id beats a panic in a test fixture).
fn reply(w: &mut impl std::io::Write, id: &Option<Value>, result: Value) {
    let _ = write_message(w, &json!({"jsonrpc": "2.0", "id": id, "result": result}));
}

/// One quickfix CodeAction (changes-map WorkspaceEdit, edits ASCENDING on purpose) plus one
/// bare Command whose argument carries the uri back to us.
fn code_actions_result(uri: &str) -> Value {
    json!([
        {
            "title": "replace oops with 42",
            "kind": "quickfix",
            "isPreferred": true,
            "diagnostics": [],
            "edit": {"changes": {uri: [
                {"range": {"start": {"line": 0, "character": 0},
                           "end": {"line": 0, "character": 0}},
                 "newText": "/* fixed */ "},
                {"range": {"start": {"line": 1, "character": 8},
                           "end": {"line": 1, "character": 12}},
                 "newText": "fortytwo"},
            ]}}
        },
        {
            "title": "fix via command",
            "command": "fake.fix",
            "arguments": [uri]
        }
    ])
}
