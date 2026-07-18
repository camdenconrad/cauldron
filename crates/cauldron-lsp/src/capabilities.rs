//! Initialize params + capability negotiation for the supported servers (clangd,
//! rust-analyzer, pyright, typescript-language-server, vscode css/html).
//!
//! The client-capability payload is RAW JSON (`serde_json::json!`) by design, not lsp-types'
//! `ClientCapabilities` structs: the wire shape is the contract (live-probe-verified against
//! clangd 22.1.6 and rust-analyzer 1.95.0), and raw JSON keeps the legacy clangd top-level
//! `offsetEncoding` field expressible without struct gymnastics. Negotiation reads the raw
//! initialize RESULT the same way: standard `capabilities.positionEncoding` → legacy top-level
//! `offsetEncoding` → the mandatory utf-16 default.
//!
//! The four npm servers (pyright/tsserver/css/html) all issue `workspace/configuration`
//! requests at runtime; the reader auto-replies `null` per item (server.rs), which every one of
//! them accepts — pyright explicitly falls back to its built-in defaults on a null config.

use std::path::{Path, PathBuf};

use lsp_types::Url;
use serde_json::{json, Value};

use crate::{Encoding, ServerKind};

/// Build the exact `initialize` request params for `kind`, rooted at `root`.
///
/// The shared skeleton advertises everything both servers need (position encodings, versioned
/// publishDiagnostics, resolve-round-trip completion, workDoneProgress, workspace edits); the
/// per-kind deltas add rust-analyzer's pull-diagnostics + serverStatus capabilities and its
/// initializationOptions config tree. clangd takes `initializationOptions: null` — it is
/// configured via CLI flags and `.clangd` files instead.
pub fn initialize_params(root: &Path, kind: ServerKind) -> Value {
    let root_uri = file_uri(root).to_string();
    let root_path = root.to_string_lossy().into_owned();
    let folder_name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| root_path.clone());
    // The literal SymbolKind range 1..=26 (File..TypeParameter).
    let symbol_kinds: Vec<i32> = (1..=26).collect();

    let initialization_options = match kind {
        // clangd configures via CLI flags + .clangd files, never initializationOptions.
        ServerKind::Clangd => Value::Null,
        // pyright: openFilesOnly matches Cauldron's open-docs-only sync model; the other two
        // are the stock "just works" analysis switches. Everything else falls back to pyright
        // defaults via our null workspace/configuration replies.
        ServerKind::Pyright => json!({
            "python": {"analysis": {
                "autoSearchPaths": true,
                "useLibraryCodeForTypes": true,
                "diagnosticMode": "openFilesOnly"
            }}
        }),
        // The web servers take the shared skeleton with no init options. csharp-ls likewise
        // configures itself from the discovered .sln/.csproj and needs no init options.
        ServerKind::TsServer
        | ServerKind::CssLs
        | ServerKind::HtmlLs
        | ServerKind::CSharpLs
        | ServerKind::JsonLs
        | ServerKind::YamlLs
        | ServerKind::Jdtls => Value::Null,
        // The whole rust-analyzer config tree: VS Code keys minus the `rust-analyzer.` prefix.
        // `files.watcher: "server"` = r-a runs its own notify watcher, so Cauldron implements
        // no didChangeWatchedFiles in v1.
        ServerKind::RustAnalyzer => json!({
            "checkOnSave": true,
            "check": {"command": "check"},
            "cargo": {"buildScripts": {"enable": true}},
            "procMacro": {"enable": true},
            "files": {"watcher": "server"},
            "diagnostics": {"enable": true}
        }),
    };

    let mut params = json!({
        "processId": std::process::id(),
        "clientInfo": {"name": "cauldron", "version": env!("CARGO_PKG_VERSION")},
        "rootUri": root_uri,
        "rootPath": root_path,
        "workspaceFolders": [{"uri": root_uri, "name": folder_name}],
        "capabilities": {
            "general": {"positionEncodings": ["utf-8", "utf-16"]},
            // Legacy clangd top-level capability — harmless to rust-analyzer, covers older clangd.
            "offsetEncoding": ["utf-8", "utf-16"],
            "textDocument": {
                "synchronization": {"didSave": true},
                "publishDiagnostics": {
                    "versionSupport": true,
                    "relatedInformation": true,
                    "codeDescriptionSupport": true,
                    "tagSupport": {"valueSet": [1, 2]}
                },
                "completion": {
                    "completionItem": {
                        // The editor expands `$1`/`${1:x}`/`$0` with Tab traversal
                        // (cauldron-editor::snippet); this also unlocks rust-analyzer's
                        // postfix completions (.if/.match/.dbg), suppressed without it.
                        "snippetSupport": true,
                        "insertReplaceSupport": true,
                        "deprecatedSupport": true,
                        "resolveSupport": {
                            "properties": ["documentation", "detail", "additionalTextEdits"]
                        }
                    },
                    "completionItemKind": {}
                },
                "hover": {"contentFormat": ["markdown", "plaintext"]},
                "signatureHelp": {
                    "signatureInformation": {
                        "documentationFormat": ["markdown", "plaintext"],
                        "parameterInformation": {"labelOffsetSupport": true},
                        "activeParameterSupport": true
                    }
                },
                "definition": {"linkSupport": false},
                "declaration": {"linkSupport": false},
                "implementation": {"linkSupport": false},
                "callHierarchy": {"dynamicRegistration": false},
                "references": {},
                // Literal support is what makes servers send CodeAction objects (with inline
                // WorkspaceEdits) instead of bare Commands wherever they can.
                "codeAction": {
                    "codeActionLiteralSupport": {
                        "codeActionKind": {
                            "valueSet": ["quickfix", "refactor", "refactor.rewrite", "source"]
                        }
                    },
                    "isPreferredSupport": true
                },
                "documentSymbol": {
                    "hierarchicalDocumentSymbolSupport": true,
                    "symbolKind": {"valueSet": symbol_kinds}
                },
                "rename": {"prepareSupport": true},
                "formatting": {},
                // Type/parameter hints (rust-analyzer, clangd, pyright all key on this).
                "inlayHint": {"dynamicRegistration": false}
            },
            "window": {"workDoneProgress": true},
            "workspace": {
                "workspaceFolders": true,
                // Project-wide symbol queries (workspace/symbol — Search Everywhere feed).
                "symbol": {"symbolKind": {"valueSet": symbol_kinds.clone()}},
                "applyEdit": true,
                "workspaceEdit": {"documentChanges": true, "failureHandling": "abort"},
                "didChangeConfiguration": {},
                "didChangeWatchedFiles": {"dynamicRegistration": false}
            }
        },
        "initializationOptions": initialization_options,
        "trace": "off"
    });

    if kind == ServerKind::RustAnalyzer {
        let caps = &mut params["capabilities"];
        // Native pull diagnostics — r-a's instant diagnostics NEVER arrive by push.
        caps["textDocument"]["diagnostic"] =
            json!({"dynamicRegistration": false, "relatedDocumentSupport": false});
        caps["workspace"]["diagnostics"] = json!({"refreshSupport": true});
        // `experimental/serverStatus {quiescent:true}` is THE ready signal.
        caps["experimental"] = json!({"serverStatusNotification": true});
    }
    params
}

/// Read the negotiated position encoding and sync kind off the raw initialize RESULT.
///
/// Encoding chain: standard `capabilities.positionEncoding` (LSP 3.17) → legacy top-level
/// `offsetEncoding` (clangd pre-3.17; array or bare string, first supported entry wins) →
/// the mandatory Utf16 default. Sync kind: `capabilities.textDocumentSync` as a bare number
/// or an object's `change` field; missing → 0 (None — skip didChange entirely).
pub fn negotiate(result: &Value) -> (Encoding, i64) {
    let caps = &result["capabilities"];
    let encoding = caps["positionEncoding"]
        .as_str()
        .and_then(encoding_from_str)
        .or_else(|| legacy_offset_encoding(&result["offsetEncoding"]))
        .unwrap_or(Encoding::Utf16);

    let sync = &caps["textDocumentSync"];
    let change_kind = sync.as_i64().or_else(|| sync["change"].as_i64()).unwrap_or(0);
    (encoding, change_kind)
}

fn encoding_from_str(s: &str) -> Option<Encoding> {
    match s {
        "utf-8" => Some(Encoding::Utf8),
        "utf-16" => Some(Encoding::Utf16),
        _ => None,
    }
}

/// The legacy `InitializeResult.offsetEncoding`: clangd sends a bare string, older builds an
/// array; either way the first entry we support wins.
fn legacy_offset_encoding(v: &Value) -> Option<Encoding> {
    match v {
        Value::String(s) => encoding_from_str(s),
        Value::Array(entries) => entries
            .iter()
            .filter_map(|e| e.as_str())
            .find_map(encoding_from_str),
        _ => None,
    }
}

/// `file://` URI for `path`. Callers pass ABSOLUTE paths (workspace roots, opened files), for
/// which `Url::from_file_path` always succeeds; the manual percent-encoded fallback only exists
/// so this can never panic on a stray relative path.
pub fn file_uri(path: &Path) -> Url {
    if let Ok(url) = Url::from_file_path(path) {
        return url;
    }
    let mut encoded = percent_encode_path(&path.to_string_lossy());
    if !encoded.starts_with('/') {
        encoded.insert(0, '/');
    }
    Url::parse(&format!("file://{encoded}"))
        .unwrap_or_else(|_| Url::parse("file:///").expect("static file-root URI is valid"))
}

/// Inverse of [`file_uri`]: `None` for non-`file://` schemes (e.g. rust-analyzer's
/// `rust-analyzer://` macro-expansion documents).
pub fn uri_to_path(uri: &Url) -> Option<PathBuf> {
    uri.to_file_path().ok()
}

/// Minimal RFC-3986 path escaping for the [`file_uri`] fallback: unreserved chars + `/` pass
/// through, everything else (spaces, `%`, non-ASCII bytes) becomes `%XX`.
fn percent_encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for &b in path.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clangd_params() -> Value {
        initialize_params(Path::new("/tmp/proj"), ServerKind::Clangd)
    }

    fn ra_params() -> Value {
        initialize_params(Path::new("/tmp/proj"), ServerKind::RustAnalyzer)
    }

    #[test]
    fn shared_skeleton_load_bearing_fields() {
        let p = clangd_params();
        assert_eq!(p["processId"].as_u64(), Some(u64::from(std::process::id())));
        assert_eq!(p["clientInfo"]["name"], "cauldron");
        assert_eq!(p["clientInfo"]["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(p["rootUri"], "file:///tmp/proj");
        assert_eq!(p["rootPath"], "/tmp/proj");
        assert_eq!(p["workspaceFolders"], json!([{"uri": "file:///tmp/proj", "name": "proj"}]));
        assert_eq!(p["trace"], "off");

        let caps = &p["capabilities"];
        assert_eq!(caps["general"]["positionEncodings"], json!(["utf-8", "utf-16"]));
        assert_eq!(caps["offsetEncoding"], json!(["utf-8", "utf-16"])); // legacy top-level
        assert_eq!(caps["window"]["workDoneProgress"], true);
        assert_eq!(caps["textDocument"]["synchronization"]["didSave"], true);
        assert_eq!(caps["textDocument"]["publishDiagnostics"]["versionSupport"], true);
        assert_eq!(
            caps["textDocument"]["completion"]["completionItem"]["snippetSupport"],
            true
        );
        assert_eq!(caps["textDocument"]["inlayHint"]["dynamicRegistration"], false);
        assert_eq!(caps["workspace"]["workspaceEdit"]["failureHandling"], "abort");
        assert_eq!(
            caps["textDocument"]["codeAction"],
            json!({
                "codeActionLiteralSupport": {
                    "codeActionKind": {
                        "valueSet": ["quickfix", "refactor", "refactor.rewrite", "source"]
                    }
                },
                "isPreferredSupport": true
            })
        );

        // "symbolKind": {"valueSet":[1..26]} means the literal integers 1 through 26.
        let value_set = caps["textDocument"]["documentSymbol"]["symbolKind"]["valueSet"]
            .as_array()
            .expect("valueSet is an array");
        assert_eq!(value_set.len(), 26);
        assert_eq!(value_set[0], 1);
        assert_eq!(value_set[25], 26);

        // workspace/symbol is advertised with the same full kind set (item 9).
        let ws_set = caps["workspace"]["symbol"]["symbolKind"]["valueSet"]
            .as_array()
            .expect("workspace symbol valueSet is an array");
        assert_eq!(ws_set.len(), 26);
        assert_eq!(ws_set[0], 1);
        assert_eq!(ws_set[25], 26);
    }

    #[test]
    fn clangd_null_options_and_no_rust_analyzer_extras() {
        let p = clangd_params();
        assert!(p["initializationOptions"].is_null());
        let caps = &p["capabilities"];
        assert!(caps["textDocument"].get("diagnostic").is_none());
        assert!(caps["workspace"].get("diagnostics").is_none());
        assert!(caps.get("experimental").is_none());
    }

    #[test]
    fn rust_analyzer_deltas() {
        let p = ra_params();
        let caps = &p["capabilities"];
        assert_eq!(
            caps["textDocument"]["diagnostic"],
            json!({"dynamicRegistration": false, "relatedDocumentSupport": false})
        );
        assert_eq!(caps["workspace"]["diagnostics"]["refreshSupport"], true);
        assert_eq!(caps["experimental"]["serverStatusNotification"], true);

        assert_eq!(
            p["initializationOptions"],
            json!({
                "checkOnSave": true,
                "check": {"command": "check"},
                "cargo": {"buildScripts": {"enable": true}},
                "procMacro": {"enable": true},
                "files": {"watcher": "server"},
                "diagnostics": {"enable": true}
            })
        );
    }

    #[test]
    fn pyright_initialization_options() {
        let p = initialize_params(Path::new("/tmp/proj"), ServerKind::Pyright);
        assert_eq!(
            p["initializationOptions"],
            json!({
                "python": {"analysis": {
                    "autoSearchPaths": true,
                    "useLibraryCodeForTypes": true,
                    "diagnosticMode": "openFilesOnly"
                }}
            })
        );
        // The rust-analyzer deltas must not leak into other kinds.
        let caps = &p["capabilities"];
        assert!(caps["textDocument"].get("diagnostic").is_none());
        assert!(caps["workspace"].get("diagnostics").is_none());
        assert!(caps.get("experimental").is_none());
    }

    #[test]
    fn web_servers_take_shared_skeleton_with_null_options() {
        for kind in [ServerKind::TsServer, ServerKind::CssLs, ServerKind::HtmlLs] {
            let p = initialize_params(Path::new("/tmp/proj"), kind);
            assert!(p["initializationOptions"].is_null(), "{kind:?}");
            // Shared skeleton intact…
            assert_eq!(p["capabilities"]["general"]["positionEncodings"], json!(["utf-8", "utf-16"]), "{kind:?}");
            // …and no rust-analyzer extras.
            assert!(p["capabilities"].get("experimental").is_none(), "{kind:?}");
            assert!(p["capabilities"]["textDocument"].get("diagnostic").is_none(), "{kind:?}");
        }
    }

    #[test]
    fn negotiate_standard_position_encoding() {
        let r = json!({
            "capabilities": {"positionEncoding": "utf-8", "textDocumentSync": {"change": 2}}
        });
        assert_eq!(negotiate(&r), (Encoding::Utf8, 2));
    }

    #[test]
    fn negotiate_legacy_offset_encoding_array() {
        let r = json!({
            "capabilities": {"textDocumentSync": 1},
            "offsetEncoding": ["utf-8", "utf-16"]
        });
        assert_eq!(negotiate(&r), (Encoding::Utf8, 1));
    }

    #[test]
    fn negotiate_legacy_offset_encoding_skips_unsupported() {
        let r = json!({"capabilities": {}, "offsetEncoding": ["utf-32", "utf-16"]});
        assert_eq!(negotiate(&r).0, Encoding::Utf16);
    }

    #[test]
    fn negotiate_legacy_offset_encoding_bare_string() {
        let r = json!({"capabilities": {}, "offsetEncoding": "utf-8"});
        assert_eq!(negotiate(&r).0, Encoding::Utf8);
    }

    #[test]
    fn negotiate_defaults_when_nothing_advertised() {
        let r = json!({"capabilities": {}});
        assert_eq!(negotiate(&r), (Encoding::Utf16, 0));
    }

    #[test]
    fn negotiate_sync_kind_as_bare_number() {
        let r = json!({"capabilities": {"textDocumentSync": 1}});
        assert_eq!(negotiate(&r).1, 1);
    }

    #[test]
    fn uri_round_trip() {
        let path = Path::new("/tmp/x.c");
        let uri = file_uri(path);
        assert_eq!(uri.as_str(), "file:///tmp/x.c");
        assert_eq!(uri_to_path(&uri), Some(PathBuf::from("/tmp/x.c")));
    }
}
