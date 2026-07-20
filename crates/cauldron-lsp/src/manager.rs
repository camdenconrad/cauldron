//! The app-facing coordinator: server registry, document routing, and the pump.
//!
//! All manager methods run on the UI thread and never block: outbound traffic goes through each
//! server's writer thread, inbound arrives on one mpsc drained by [`LspManager::pump`] once per
//! frame (top of `update()`), which also runs the timer work — rust-analyzer pull debounce,
//! pending-request timeout sweep, crash-respawn backoff. `pump` returns a wake hint the app feeds
//! to `ctx.request_repaint_after` so timers fire on idle frames too.
//!
//! SYNC MODEL: the manager keeps an O(1) `Rope` clone per open doc (`docs_ropes`). Pre-handshake
//! edits just bump the version and update the rope — when the initialize response lands, a single
//! `didOpen` is synthesized with the CURRENT text (no stale-content replay, and no queueing of
//! didChanges built before the encoding was even negotiated). The same store powers the Full-sync
//! fallback and crash-respawn re-opens, at zero per-keystroke cost (ropes are Arc-shared).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use cauldron_editor::buffer::Transaction;
use cauldron_editor::position;
use cauldron_editor::syntax::Lang;
use ropey::Rope;
use serde_json::{json, Value};

use crate::server::{ClangdOptions, DocState, LspServer, Outgoing, PendingKind, Raw};
use crate::{discovery, txsync, Encoding, LspEvent, Notifier, ServerId, ServerKind, ServerState};

/// rust-analyzer native-diagnostics pull debounce (sliding, per doc).
const PULL_DEBOUNCE: Duration = Duration::from_millis(300);
/// Delay after `quiescent` before the pull-everything pass (a pull at the exact quiescent
/// instant returns empty — probe-verified).
const QUIESCENT_PULL_DELAY: Duration = Duration::from_secs(2);
/// Outstanding feature requests older than this are swept (the server likely dropped them).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// Respawn attempts are capped: give up after this many crashes.
const MAX_RESTARTS: u32 = 5;

pub struct LspManager {
    servers: HashMap<(ServerKind, PathBuf), LspServer>,
    /// Which server key a document belongs to.
    docs_index: HashMap<PathBuf, (ServerKind, PathBuf)>,
    /// Current text of every open doc (Arc-shared rope snapshots, O(1) to update).
    docs_ropes: HashMap<PathBuf, Rope>,
    events_rx: Receiver<(ServerId, Raw)>,
    events_tx: Sender<(ServerId, Raw)>,
    notifier: Notifier,
    next_server_id: u32,
    /// Sliding per-doc deadlines for the rust-analyzer diagnostic pull.
    pull_deadlines: HashMap<PathBuf, Instant>,
    /// Events produced outside pump (e.g. Degraded at open time), drained by the next pump.
    outbox: Vec<(ServerId, LspEvent)>,
    restarts: HashMap<(ServerKind, PathBuf), u32>,
    /// AUTO-INSTALL: kinds we already tried to install this run (one attempt each).
    install_attempted: std::collections::HashSet<ServerKind>,
    /// Finished background installs: `(kind, root, success)` → respawn on success.
    install_rx: Receiver<(ServerKind, PathBuf, bool)>,
    install_tx: Sender<(ServerKind, PathBuf, bool)>,
    /// clangd CLI knobs. The manager is the single owner: the app reads them back through
    /// [`LspManager::clangd_options`] rather than keeping a second copy that can drift.
    clangd_opts: ClangdOptions,
}

impl LspManager {
    pub fn new(notifier: Notifier) -> Self {
        let (events_tx, events_rx) = mpsc::channel();
        let (install_tx, install_rx) = mpsc::channel();
        Self {
            servers: HashMap::new(),
            docs_index: HashMap::new(),
            docs_ropes: HashMap::new(),
            events_rx,
            events_tx,
            notifier,
            next_server_id: 0,
            pull_deadlines: HashMap::new(),
            outbox: Vec::new(),
            restarts: HashMap::new(),
            install_attempted: std::collections::HashSet::new(),
            install_rx,
            install_tx,
            clangd_opts: ClangdOptions::default(),
        }
    }

    /// Open a document: ensure the right server exists for `(lang, root)` and send `didOpen`.
    /// For Rust the effective root is re-derived from the file (workspace-aware).
    pub fn open_doc(&mut self, lang: Lang, root: &Path, path: &Path, text: &str) {
        let Some(kind) = kind_for(lang) else { return };
        let root = match kind {
            ServerKind::RustAnalyzer => {
                discovery::rust_analyzer_root(path).unwrap_or_else(|| root.to_path_buf())
            }
            // clangd + the npm servers (pyright/tsserver/css/html) all take the workspace root
            // as-is — no per-language root discovery in v1.
            _ => root.to_path_buf(),
        };
        let key = (kind, root.clone());
        if !self.servers.contains_key(&key) {
            self.spawn_server(key.clone());
        }
        self.docs_ropes.insert(path.to_path_buf(), Rope::from_str(text));
        self.docs_index.insert(path.to_path_buf(), key.clone());
        let Some(server) = self.servers.get_mut(&key) else { return };
        server.docs.insert(path.to_path_buf(), DocState { version: 0 });
        if matches!(server.state, ServerState::Ready | ServerState::Indexing(_)) {
            send_did_open(server, path, text, 0);
        } // else: synthesized from docs_ropes when the handshake completes
    }

    /// Route one applied Transaction to the doc's server as an incremental `didChange`.
    /// `pre_rope` is the buffer BEFORE the transaction, `post_rope` after (both O(1) clones).
    pub fn did_change(&mut self, path: &Path, pre_rope: &Rope, post_rope: &Rope, tx: &Transaction) {
        self.docs_ropes.insert(path.to_path_buf(), post_rope.clone());
        let Some(key) = self.docs_index.get(path) else { return };
        let Some(server) = self.servers.get_mut(key) else { return };
        let Some(doc) = server.docs.get_mut(path) else { return };
        doc.version += 1;
        let version = doc.version;
        if !matches!(server.state, ServerState::Ready | ServerState::Indexing(_)) {
            return; // pre-handshake: the eventual didOpen carries this text at this version
        }
        let content_changes = match server.sync_kind {
            2 => txsync::changes_for_tx(pre_rope, tx, server.encoding),
            1 => txsync::full_text_change(post_rope),
            _ => return,
        };
        server.send(Outgoing::Notification {
            method: "textDocument/didChange",
            params: json!({
                "textDocument": {"uri": uri_of(path), "version": version},
                "contentChanges": content_changes,
            }),
        });
        if server.diag_provider {
            self.pull_deadlines.insert(path.to_path_buf(), Instant::now() + PULL_DEBOUNCE);
        }
    }

    /// `didSave` (no text payload): clangd re-checks deps, rust-analyzer triggers flycheck.
    pub fn did_save(&mut self, path: &Path) {
        if let Some(server) = self.server_for_mut(path) {
            server.send(Outgoing::Notification {
                method: "textDocument/didSave",
                params: json!({"textDocument": {"uri": uri_of(path)}}),
            });
        }
    }

    pub fn close_doc(&mut self, path: &Path) {
        if let Some(server) = self.server_for_mut(path) {
            server.docs.remove(path);
            server.send(Outgoing::Notification {
                method: "textDocument/didClose",
                params: json!({"textDocument": {"uri": uri_of(path)}}),
            });
        }
        self.docs_index.remove(path);
        self.docs_ropes.remove(path);
        self.pull_deadlines.remove(path);
    }

    pub fn request_hover(&mut self, path: &Path, rope: &Rope, byte: usize, generation: u64) {
        self.feature_request(path, rope, byte, "textDocument/hover", PendingKind::Hover {
            generation,
        });
    }

    /// Call hierarchy (incoming callers). Step 1: prepareCallHierarchy at the caret; pump
    /// chains callHierarchy/incomingCalls when the item comes back. The final callers arrive
    /// as [`LspEvent::IncomingCalls`].
    pub fn request_call_hierarchy(&mut self, path: &Path, rope: &Rope, byte: usize, generation: u64) {
        self.feature_request(
            path,
            rope,
            byte,
            "textDocument/prepareCallHierarchy",
            PendingKind::PrepareCallHierarchy { generation },
        );
    }

    /// Go to implementation (trait impls / virtual overrides). Response shape and app
    /// handling are identical to definition, so it reuses the Definition pending kind.
    pub fn request_implementation(&mut self, path: &Path, rope: &Rope, byte: usize, generation: u64) {
        self.feature_request(path, rope, byte, "textDocument/implementation", PendingKind::Definition {
            generation,
        });
    }

    pub fn request_definition(&mut self, path: &Path, rope: &Rope, byte: usize, generation: u64) {
        self.feature_request(path, rope, byte, "textDocument/definition", PendingKind::Definition {
            generation,
        });
    }

    /// `textDocument/rename`: the response's WorkspaceEdit renames every occurrence
    /// project-wide (arrives as [`LspEvent::RenameEdit`]).
    pub fn request_rename(
        &mut self,
        path: &Path,
        rope: &Rope,
        byte: usize,
        new_name: &str,
        generation: u64,
    ) {
        let Some(server) = self.server_for_mut(path) else { return };
        if !matches!(server.state, ServerState::Ready | ServerState::Indexing(_)) {
            return;
        }
        let pos = lsp_position(rope, byte, server.encoding);
        server.request(
            "textDocument/rename",
            json!({
                "textDocument": {"uri": uri_of(path)},
                "position": pos,
                "newName": new_name,
            }),
            PendingKind::Rename { generation },
        );
    }

    /// `textDocument/formatting`: reformat the whole document. The response's TextEdits arrive as
    /// [`LspEvent::Formatting`]. No-ops when the server doesn't advertise
    /// `documentFormattingProvider`. `tab_size`/`insert_spaces` are the editor's indent settings.
    pub fn request_formatting(
        &mut self,
        path: &Path,
        generation: u64,
        tab_size: u32,
        insert_spaces: bool,
    ) {
        let Some(server) = self.server_for_mut(path) else { return };
        if !matches!(server.state, ServerState::Ready | ServerState::Indexing(_)) {
            return;
        }
        let provides = server
            .caps
            .get("documentFormattingProvider")
            .map(|v| !v.is_null() && v.as_bool() != Some(false))
            .unwrap_or(false);
        if !provides {
            return;
        }
        server.request(
            "textDocument/formatting",
            json!({
                "textDocument": {"uri": uri_of(path)},
                "options": {
                    "tabSize": tab_size,
                    "insertSpaces": insert_spaces,
                    "trimTrailingWhitespace": true,
                    "insertFinalNewline": true,
                },
            }),
            PendingKind::Formatting { generation, path: path.to_path_buf() },
        );
    }

    /// `textDocument/references` (declaration included) → [`LspEvent::References`].
    pub fn request_references(&mut self, path: &Path, rope: &Rope, byte: usize, generation: u64) {
        let Some(server) = self.server_for_mut(path) else { return };
        if !matches!(server.state, ServerState::Ready | ServerState::Indexing(_)) {
            return;
        }
        let pos = lsp_position(rope, byte, server.encoding);
        server.request(
            "textDocument/references",
            json!({
                "textDocument": {"uri": uri_of(path)},
                "position": pos,
                "context": {"includeDeclaration": true},
            }),
            PendingKind::References { generation },
        );
    }

    /// `completionItem/resolve` for an accepted item — rust-analyzer / typescript-language-server
    /// / pyright only deliver auto-import additionalTextEdits on resolve. No-ops when the server
    /// doesn't advertise resolveProvider (clangd).
    pub fn resolve_completion(
        &mut self,
        path: &Path,
        item: &lsp_types::CompletionItem,
        generation: u64,
    ) {
        let Some(server) = self.server_for_mut(path) else { return };
        if !matches!(server.state, ServerState::Ready | ServerState::Indexing(_)) {
            return;
        }
        let resolves = server
            .caps
            .get("completionProvider")
            .and_then(|c| c.get("resolveProvider"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !resolves {
            return;
        }
        let Ok(params) = serde_json::to_value(item) else { return };
        server.request(
            "completionItem/resolve",
            params,
            PendingKind::ResolveCompletion { generation, path: path.to_path_buf() },
        );
    }

    pub fn request_signature_help(&mut self, path: &Path, rope: &Rope, byte: usize, generation: u64) {
        self.feature_request(
            path,
            rope,
            byte,
            "textDocument/signatureHelp",
            PendingKind::SignatureHelp { generation },
        );
    }

    /// clangd's `textDocument/switchSourceHeader` extension: given `foo.c`, answer `foo.h` (and
    /// vice versa). clangd resolves this through the COMPILATION DATABASE, so it finds the
    /// counterpart across dissimilar `src/` and `include/` trees, which a filename guess cannot.
    /// Answers as [`LspEvent::SwitchSourceHeader`]; a server without the extension simply errors
    /// and the app falls back.
    pub fn request_switch_source_header(&mut self, path: &Path) -> bool {
        // Clangd-only: the extension does not exist elsewhere, and an unknown method on another
        // server is just a wasted round trip ending in an error.
        if self.docs_index.get(path).map(|(k, _)| *k) != Some(ServerKind::Clangd) {
            return false;
        }
        let Some(server) = self.server_for_mut(path) else { return false };
        if !matches!(server.state, ServerState::Ready | ServerState::Indexing(_)) {
            return false;
        }
        server.request(
            "textDocument/switchSourceHeader",
            json!({"uri": uri_of(path)}),
            PendingKind::SwitchSourceHeader { from: path.to_path_buf() },
        );
        true
    }

    /// `textDocument/documentSymbol` — the whole file's symbol tree.
    pub fn request_document_symbols(&mut self, path: &Path, generation: u64) {
        let Some(server) = self.server_for_mut(path) else { return };
        if !matches!(server.state, ServerState::Ready | ServerState::Indexing(_)) {
            return;
        }
        server.request(
            "textDocument/documentSymbol",
            json!({"textDocument": {"uri": uri_of(path)}}),
            PendingKind::DocumentSymbols { generation, path: path.to_path_buf() },
        );
    }

    pub fn request_completion(&mut self, path: &Path, rope: &Rope, byte: usize, generation: u64) {
        self.feature_request(path, rope, byte, "textDocument/completion", PendingKind::Completion {
            generation,
        });
    }

    /// Quick fixes / refactorings for the byte `range` of `path` (right-click, lightbulb).
    /// The response arrives as [`LspEvent::CodeActions`] stamped with `generation` so the app
    /// can drop stale replies.
    ///
    /// `diags` must be the server's OWN diagnostic objects overlapping `range`. The spec makes
    /// `context.diagnostics` the client's job, and clangd in particular derives its quickfixes
    /// from exactly what it is handed — sending `[]` made it return no fixes at all.
    pub fn request_code_actions(
        &mut self,
        path: &Path,
        rope: &Rope,
        range: std::ops::Range<usize>,
        diags: &[lsp_types::Diagnostic],
        generation: u64,
    ) {
        self.request_code_actions_only(path, rope, range, &[], diags, generation);
    }

    /// As [`Self::request_code_actions`], but restricted to the given `CodeActionKind` prefixes
    /// (LSP matches by dotted prefix, so `"refactor"` also brings in `refactor.extract` etc.).
    /// An empty `only` asks for everything. This is what backs the Refactor This menu — without
    /// the filter it would be swamped by quick fixes for unrelated diagnostics in range.
    pub fn request_code_actions_only(
        &mut self,
        path: &Path,
        rope: &Rope,
        range: std::ops::Range<usize>,
        only: &[&str],
        diags: &[lsp_types::Diagnostic],
        generation: u64,
    ) {
        let Some(server) = self.server_for_mut(path) else { return };
        if !matches!(server.state, ServerState::Ready | ServerState::Indexing(_)) {
            return;
        }
        let start = lsp_position(rope, range.start, server.encoding);
        let end = lsp_position(rope, range.end, server.encoding);
        // triggerKind 1 = Invoked. `only` omitted entirely (not `[]`) means "every kind is
        // welcome" — an empty array would instead be read as "nothing qualifies".
        let mut context = json!({"diagnostics": diags, "triggerKind": 1});
        if !only.is_empty() {
            context["only"] = json!(only);
        }
        server.request(
            "textDocument/codeAction",
            json!({
                "textDocument": {"uri": uri_of(path)},
                "range": {"start": start, "end": end},
                "context": context,
            }),
            PendingKind::CodeAction { generation, path: path.to_path_buf() },
        );
    }

    /// Does `path`'s server fill in code actions lazily (`codeActionProvider.resolveProvider`)?
    /// When true, an action arriving with no `edit` is not empty — it is deferred, and must go
    /// through [`Self::resolve_code_action`] before it can be applied.
    pub fn code_action_resolves(&mut self, path: &Path) -> bool {
        let Some(server) = self.server_for_mut(path) else { return false };
        server
            .caps
            .get("codeActionProvider")
            .and_then(|c| c.get("resolveProvider"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    }

    /// `codeAction/resolve` — fetch the `edit`/`command` for an action the server deferred.
    /// The whole action (including its opaque `data`) must be echoed back verbatim; that blob is
    /// how the server recovers the refactor it planned. Answers as [`LspEvent::ResolvedCodeAction`].
    pub fn resolve_code_action(
        &mut self,
        path: &Path,
        action: &lsp_types::CodeAction,
        generation: u64,
    ) {
        let Some(server) = self.server_for_mut(path) else { return };
        if !matches!(server.state, ServerState::Ready | ServerState::Indexing(_)) {
            return;
        }
        let Ok(params) = serde_json::to_value(action) else { return };
        server.request(
            "codeAction/resolve",
            params,
            PendingKind::ResolveCodeAction { generation, path: path.to_path_buf() },
        );
    }

    /// Whole-document inlay hints (types, parameter names). Response arrives as
    /// [`LspEvent::InlayHints`] stamped with `generation` for stale-drop.
    pub fn request_inlay_hints(&mut self, path: &Path, rope: &Rope, generation: u64) {
        let Some(server) = self.server_for_mut(path) else { return };
        if !matches!(server.state, ServerState::Ready | ServerState::Indexing(_)) {
            return;
        }
        let end = lsp_position(rope, rope.len_bytes(), server.encoding);
        server.request(
            "textDocument/inlayHint",
            json!({
                "textDocument": {"uri": uri_of(path)},
                "range": {"start": {"line": 0, "character": 0}, "end": end},
            }),
            PendingKind::InlayHints { generation, path: path.to_path_buf() },
        );
    }

    /// Run a bare [`lsp_types::Command`] (a code action with no inline edit) on the server
    /// owning `path` via `workspace/executeCommand`. Any resulting edits arrive asynchronously
    /// as [`LspEvent::ApplyEdit`]; the direct response is acked and ignored.
    pub fn execute_command(&mut self, path: &Path, command: &lsp_types::Command) {
        let Some(server) = self.server_for_mut(path) else { return };
        if !matches!(server.state, ServerState::Ready | ServerState::Indexing(_)) {
            return;
        }
        server.request(
            "workspace/executeCommand",
            json!({
                "command": command.command,
                "arguments": command.arguments.clone().unwrap_or_default(),
            }),
            PendingKind::Generic,
        );
    }

    /// `workspace/symbol {query}` fanned out to every server that can answer it NOW — the
    /// request is not document-scoped, so it routes by server key, and each answering server
    /// produces its own [`LspEvent::WorkspaceSymbols`] (merge them app-side, all stamped with
    /// `generation`). A server participates only when [`workspace_symbols_ready`] says so:
    /// indexed/quiescent, not merely Ready — a half-indexed rust-analyzer parks the request
    /// until its index is built, which reads as a hang and trips the 20 s timeout sweep.
    /// Returns how many servers were asked (0 = nothing inflight, don't wait).
    pub fn request_workspace_symbols(&mut self, query: &str, generation: u64) -> usize {
        let mut asked = 0;
        for ((kind, _), server) in self.servers.iter_mut() {
            if !workspace_symbols_ready(*kind, &server.state, server.quiescent, &server.caps) {
                continue;
            }
            server.request(
                "workspace/symbol",
                json!({"query": query}),
                PendingKind::WorkspaceSymbols { generation },
            );
            asked += 1;
        }
        asked
    }

    fn feature_request(
        &mut self,
        path: &Path,
        rope: &Rope,
        byte: usize,
        method: &'static str,
        kind: PendingKind,
    ) {
        let Some(server) = self.server_for_mut(path) else { return };
        if !matches!(server.state, ServerState::Ready | ServerState::Indexing(_)) {
            return;
        }
        let pos = lsp_position(rope, byte, server.encoding);
        server.request(
            method,
            json!({
                "textDocument": {"uri": uri_of(path)},
                "position": pos,
            }),
            kind,
        );
    }

    /// Drain events + run timers. Call once per frame, first thing. Returns the public events
    /// and an optional "wake me again in" hint for `ctx.request_repaint_after`.
    pub fn pump(&mut self) -> (Vec<(ServerId, LspEvent)>, Option<Duration>) {
        let mut out: Vec<(ServerId, LspEvent)> = std::mem::take(&mut self.outbox);
        let now = Instant::now();

        while let Ok((sid, raw)) = self.events_rx.try_recv() {
            let Some(key) = self.key_of(sid) else { continue };
            match raw {
                Raw::InitResult(result) => {
                    let server = self.servers.get_mut(&key).unwrap();
                    server.finish_initialize(&result);
                    // A COMPLETED handshake retires the crash history. `restarts` only ever grew,
                    // so a server that crashed MAX_RESTARTS times over a long session was declared
                    // permanently dead even though every single respawn had succeeded — and the
                    // backoff kept climbing toward its 60s ceiling for the same reason. Crash
                    // counting is meant to catch a server that CANNOT start, not one that has
                    // hiccuped a few times across eight hours.
                    self.restarts.remove(&key);
                    out.push((sid, LspEvent::State(ServerState::Ready)));
                    // Synthesize didOpen for every tracked doc with its CURRENT text.
                    let docs: Vec<(PathBuf, i32)> =
                        server.docs.iter().map(|(p, d)| (p.clone(), d.version)).collect();
                    for (path, version) in docs {
                        if let Some(rope) = self.docs_ropes.get(&path) {
                            let text = rope.to_string();
                            let server = self.servers.get_mut(&key).unwrap();
                            send_did_open(server, &path, &text, version);
                        }
                    }
                }
                Raw::InitFailed(msg) => {
                    out.push((sid, LspEvent::Message(format!("language server init failed: {msg}"))));
                    self.mark_crashed(&key, now);
                    out.push((sid, LspEvent::Exited));
                }
                Raw::Progress { pct, done } => {
                    let server = self.servers.get_mut(&key).unwrap();
                    server.state = if done { ServerState::Ready } else { ServerState::Indexing(pct) };
                    out.push((sid, LspEvent::State(server.state.clone())));
                }
                Raw::ShutdownAck => {
                    if let Some(server) = self.servers.get_mut(&key) {
                        let _ = server.to_writer.send(Outgoing::Exit);
                    }
                }
                Raw::Eof => {
                    self.mark_crashed(&key, now);
                    out.push((sid, LspEvent::Exited));
                }
                Raw::Lsp(ev) => {
                    // Call-hierarchy step 2: the prepare result is consumed HERE (not shown to
                    // the app) — fire incomingCalls for the first item on this server. An empty
                    // result surfaces as an empty IncomingCalls so the app can say "no callers".
                    if let LspEvent::CallHierarchyItems { generation, items } = &ev {
                        match items.first() {
                            Some(item) => {
                                if let Some(server) = self.servers.get_mut(&key) {
                                    server.request(
                                        "callHierarchy/incomingCalls",
                                        json!({ "item": item }),
                                        PendingKind::IncomingCalls { generation: *generation },
                                    );
                                }
                            }
                            None => out.push((
                                sid,
                                LspEvent::IncomingCalls { generation: *generation, calls: Vec::new() },
                            )),
                        }
                        continue;
                    }
                    match &ev {
                        LspEvent::Quiescent => {
                            // Index is trustworthy now — unlocks workspace-wide queries
                            // (workspace/symbol readiness keys off this, not merely Ready).
                            if let Some(server) = self.servers.get_mut(&key) {
                                server.quiescent = true;
                            }
                            // Pull every doc of this server shortly after quiescent.
                            let paths: Vec<PathBuf> = self
                                .servers
                                .get(&key)
                                .map(|s| s.docs.keys().cloned().collect())
                                .unwrap_or_default();
                            for p in paths {
                                self.pull_deadlines.insert(p, now + QUIESCENT_PULL_DELAY);
                            }
                        }
                        LspEvent::PullAllDiagnostics => {
                            let paths: Vec<PathBuf> = self
                                .servers
                                .get(&key)
                                .map(|s| s.docs.keys().cloned().collect())
                                .unwrap_or_default();
                            for p in paths {
                                self.pull_deadlines.insert(p, now + PULL_DEBOUNCE);
                            }
                        }
                        _ => {}
                    }
                    out.push((sid, ev));
                }
            }
        }

        // --- timers -----------------------------------------------------------------------
        // rust-analyzer diagnostic pulls whose debounce elapsed.
        let due: Vec<PathBuf> = self
            .pull_deadlines
            .iter()
            .filter(|(_, t)| **t <= now)
            .map(|(p, _)| p.clone())
            .collect();
        for path in due {
            self.pull_deadlines.remove(&path);
            if let Some(server) = self.server_for_mut(&path) {
                if server.diag_provider
                    && matches!(server.state, ServerState::Ready | ServerState::Indexing(_))
                {
                    let version = server.docs.get(&path).map(|d| d.version).unwrap_or(0);
                    server.request(
                        "textDocument/diagnostic",
                        json!({"textDocument": {"uri": uri_of(&path)}}),
                        PendingKind::PullDiagnostics { path: path.clone(), version },
                    );
                }
            }
        }
        // Sweep stuck FEATURE requests. Lifecycle requests are exempt: a slow server (jdtls's
        // JVM startup, rust-analyzer priming a huge monorepo) can take far longer than 20s to
        // answer `initialize`, and sweeping it drops the pending entry so the eventual
        // InitResult is discarded and the handshake NEVER completes — the server wedges with no
        // features. Shutdown likewise must survive to complete the graceful-quit handshake.
        for server in self.servers.values() {
            server.pending.lock().unwrap_or_else(|p| p.into_inner()).retain(|_, p| {
                matches!(p.kind, PendingKind::Initialize | PendingKind::Shutdown)
                    || now.duration_since(p.sent) < REQUEST_TIMEOUT
            });
        }
        // Finished background installs: success respawns (fresh spawn finds the binary
        // via PATH or the per-user fallbacks), failure explains the manual path.
        while let Ok((kind, root, ok)) = self.install_rx.try_recv() {
            let msg = if ok {
                format!("{} installed — starting", kind.display_name())
            } else {
                format!(
                    "{} install failed — see assets/install-langservers.sh for the manual steps",
                    kind.display_name()
                )
            };
            self.outbox.push((ServerId(0), LspEvent::Message(msg)));
            if ok {
                let key = (kind, root);
                self.servers.remove(&key);
                self.spawn_server(key);
            }
        }
        // Respawn crashed servers whose backoff elapsed.
        let crashed: Vec<(ServerKind, PathBuf)> = self
            .servers
            .iter()
            .filter_map(|(k, s)| match s.state {
                ServerState::Crashed { restarts, next } if next <= now && restarts <= MAX_RESTARTS => {
                    Some(k.clone())
                }
                _ => None,
            })
            .collect();
        for key in crashed {
            // Carry doc versions over; texts come from docs_ropes at the new handshake.
            let old = self.servers.remove(&key);
            self.spawn_server(key.clone());
            if let Some(mut old) = old {
                // REAP the dead child: dropping a Child without wait() leaves a zombie in the
                // process table until the whole app exits — MAX_RESTARTS crashes = 5 zombies
                // per server kind. kill() first is a no-op on the already-dead process but
                // covers the force-restart path where it may still be running.
                let _ = old.child.kill();
                let _ = old.child.wait();
                if let Some(new) = self.servers.get_mut(&key) {
                    new.docs = std::mem::take(&mut old.docs);
                }
            }
        }

        // --- wake hint ------------------------------------------------------------------------
        let mut wake: Option<Instant> = self.pull_deadlines.values().min().copied();
        for s in self.servers.values() {
            if let ServerState::Crashed { restarts, next } = s.state {
                if restarts <= MAX_RESTARTS {
                    wake = Some(wake.map_or(next, |w| w.min(next)));
                }
            }
        }
        (out, wake.map(|t| t.saturating_duration_since(now)))
    }

    /// Negotiated position encoding of the server owning `path` (for diagnostic conversion).
    /// Force-restart every server of `kind`: kill the child; the reader hits EOF, the normal
    /// crash/respawn path re-runs discovery (picking up a freshly generated compile DB) and
    /// re-opens all docs. Used after dependency auto-resolution lands a better DB.
    pub fn clangd_options(&self) -> ClangdOptions {
        self.clangd_opts
    }

    /// Change clangd's CLI knobs. A real change bounces every clangd, since the flags are only
    /// read at spawn. A no-op change must NOT restart — the settings dialog writes on every
    /// keystroke elsewhere in the file, and bouncing the server on each one would be brutal.
    pub fn set_clangd_options(&mut self, opts: ClangdOptions) {
        if self.clangd_opts == opts {
            return;
        }
        self.clangd_opts = opts;
        self.restart_kind(ServerKind::Clangd);
    }

    pub fn restart_kind(&mut self, kind: ServerKind) {
        for ((k, _), s) in self.servers.iter_mut() {
            if *k == kind {
                let _ = s.child.kill();
            }
        }
    }

    pub fn encoding_for(&self, path: &Path) -> Option<Encoding> {
        let key = self.docs_index.get(path)?;
        self.servers.get(key).map(|s| s.encoding)
    }

    /// Best-effort encoding for a file that was never didOpen'ed (a project-wide rename's
    /// WorkspaceEdit routinely targets unopened files): the encoding of the server that WOULD
    /// own it — same ServerKind, preferring one whose root contains the path. `encoding_for`
    /// only knows opened docs, and defaulting the rest to UTF-16 mis-decoded the utf-8
    /// positions clangd/rust-analyzer actually negotiate, corrupting edits on any line with
    /// non-ASCII text before the edit column.
    pub fn encoding_for_unopened(&self, path: &Path) -> Option<Encoding> {
        let kind = kind_for(Lang::from_path(&path.to_string_lossy())?)?;
        let mut fallback = None;
        for ((k, root), s) in &self.servers {
            if *k == kind {
                if path.starts_with(root) {
                    return Some(s.encoding);
                }
                fallback = Some(s.encoding);
            }
        }
        fallback
    }

    /// Is a LIVE server attached to `path`'s document — spawned and not crashed? False when the
    /// doc was never registered (unsupported language), the spawn failed (binary not on PATH —
    /// the server never entered the registry), or the server is in Crashed backoff / permanently
    /// dead past MAX_RESTARTS. The app keys LSP-vs-native-index routing off this: LSP stays
    /// primary whenever a server could still answer (Spawning/Initializing count as live).
    pub fn has_live_server(&self, path: &Path) -> bool {
        self.docs_index
            .get(path)
            .and_then(|key| self.servers.get(key))
            .is_some_and(|s| !matches!(s.state, ServerState::Crashed { .. }))
    }

    /// Last didChange version sent for `path` — publishDiagnostics with an older version is stale.
    pub fn doc_version(&self, path: &Path) -> Option<i32> {
        let key = self.docs_index.get(path)?;
        self.servers.get(key)?.docs.get(path).map(|d| d.version)
    }

    /// Human status line for the status bar, e.g. "clangd ✓ · rust-analyzer indexing 43%".
    pub fn status_line(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        for ((kind, _), s) in &self.servers {
            let name = kind.display_name();
            let state = match &s.state {
                ServerState::Spawning | ServerState::Initializing => "starting…".to_string(),
                ServerState::Indexing(Some(p)) => format!("indexing {p}%"),
                ServerState::Indexing(None) => "indexing…".to_string(),
                ServerState::Ready => "✓".to_string(),
                ServerState::Crashed { restarts, .. } if *restarts > MAX_RESTARTS => "dead".to_string(),
                ServerState::Crashed { .. } => "restarting…".to_string(),
            };
            parts.push(format!("{name} {state}"));
        }
        parts.join(" · ")
    }

    /// Graceful shutdown of every server: `shutdown` → ack (or 1 s cap) → `exit` → kill.
    pub fn shutdown_all(&mut self) {
        for server in self.servers.values_mut() {
            server.start_shutdown();
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        // Wait for acks, converting them into Exit writes as they land.
        while Instant::now() < deadline {
            match self.events_rx.recv_timeout(Duration::from_millis(50)) {
                Ok((sid, Raw::ShutdownAck)) => {
                    if let Some(key) = self.key_of(sid) {
                        if let Some(s) = self.servers.get(&key) {
                            let _ = s.to_writer.send(Outgoing::Exit);
                        }
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        for server in self.servers.values_mut() {
            // Give the writer a beat to flush `exit`, then reap unconditionally.
            let (lock, cv) = &*server.exit_flushed;
            let flushed = lock.lock().unwrap_or_else(|p| p.into_inner());
            let _ = cv.wait_timeout_while(flushed, Duration::from_millis(300), |f| !*f);
            let _ = server.child.kill();
            let _ = server.child.wait();
        }
        self.servers.clear();
    }

    // ------------------------------------------------------------------------------------------

    fn spawn_server(&mut self, key: (ServerKind, PathBuf)) {
        self.next_server_id += 1;
        let id = ServerId(self.next_server_id);
        let (kind, root) = &key;
        let compile_db = match kind {
            ServerKind::Clangd => {
                let db = discovery::clangd_compile_db(root, None);
                if db.is_none() && !discovery::clangd_fallback(root) {
                    self.outbox.push((
                        id,
                        LspEvent::Degraded {
                            reason: "no compile_commands.json — clangd runs with defaults".into(),
                        },
                    ));
                }
                db
            }
            _ => None,
        };
        match LspServer::spawn(
            id,
            *kind,
            root,
            compile_db.as_ref(),
            self.clangd_opts,
            self.events_tx.clone(),
            Arc::clone(&self.notifier),
        ) {
            Ok(server) => {
                self.servers.insert(key, server);
            }
            Err(e) => {
                // AUTO-INSTALL: a missing binary we know how to fetch installs itself in the
                // background (once per kind per run); success respawns the server. Others
                // (clangd = distro package) just explain themselves.
                if e.kind() == std::io::ErrorKind::NotFound {
                    if let Some((prog, args)) = install_command(*kind) {
                        if self.install_attempted.insert(*kind) {
                            self.outbox.push((
                                id,
                                LspEvent::Message(format!(
                                    "{} not found — installing in the background ({prog})…",
                                    kind.display_name()
                                )),
                            ));
                            let tx = self.install_tx.clone();
                            let notifier = Arc::clone(&self.notifier);
                            let kind = *kind;
                            let root = root.clone();
                            let _ = std::thread::Builder::new().name("cauldron-lsp-install".into()).spawn(
                                move || {
                                    let ok = std::process::Command::new(prog)
                                        .args(&args)
                                        .output()
                                        .map(|o| o.status.success())
                                        .unwrap_or(false);
                                    let _ = tx.send((kind, root, ok));
                                    notifier();
                                },
                            );
                            return;
                        }
                    }
                }
                self.outbox.push((
                    id,
                    LspEvent::Message(format!("failed to spawn {}: {e}", kind.display_name())),
                ));
            }
        }
    }

    fn mark_crashed(&mut self, key: &(ServerKind, PathBuf), now: Instant) {
        let restarts = self.restarts.entry(key.clone()).or_insert(0);
        *restarts += 1;
        let backoff = Duration::from_secs((1u64 << (*restarts).min(6)).min(60));
        if let Some(server) = self.servers.get_mut(key) {
            server.state = ServerState::Crashed { restarts: *restarts, next: now + backoff };
        }
    }

    fn key_of(&self, sid: ServerId) -> Option<(ServerKind, PathBuf)> {
        self.servers.iter().find(|(_, s)| s.id == sid).map(|(k, _)| k.clone())
    }

    fn server_for_mut(&mut self, path: &Path) -> Option<&mut LspServer> {
        let key = self.docs_index.get(path)?.clone();
        self.servers.get_mut(&key)
    }
}

/// Servers installable unattended, and how. clangd (distro package, needs root) and
/// csharp-ls (dotnet tool; dotnet itself may be absent) are deliberately excluded — they
/// get a message, not a surprise sudo.
fn install_command(kind: ServerKind) -> Option<(&'static str, Vec<&'static str>)> {
    match kind {
        ServerKind::Pyright => Some(("npm", vec!["install", "-g", "pyright"])),
        ServerKind::TsServer => {
            Some(("npm", vec!["install", "-g", "typescript-language-server", "typescript"]))
        }
        ServerKind::CssLs | ServerKind::HtmlLs | ServerKind::JsonLs => {
            Some(("npm", vec!["install", "-g", "vscode-langservers-extracted"]))
        }
        ServerKind::YamlLs => Some(("npm", vec!["install", "-g", "yaml-language-server"])),
        ServerKind::RustAnalyzer => Some(("rustup", vec!["component", "add", "rust-analyzer"])),
        ServerKind::Clangd | ServerKind::CSharpLs | ServerKind::Jdtls => None,
    }
}

// -------------------------------------------------------------------------------------------

fn kind_for(lang: Lang) -> Option<ServerKind> {
    match lang {
        Lang::C | Lang::Cpp => Some(ServerKind::Clangd),
        Lang::Rust => Some(ServerKind::RustAnalyzer),
        Lang::Python => Some(ServerKind::Pyright),
        Lang::Js | Lang::Ts | Lang::Tsx => Some(ServerKind::TsServer),
        Lang::Css => Some(ServerKind::CssLs),
        Lang::Html => Some(ServerKind::HtmlLs),
        Lang::CSharp => Some(ServerKind::CSharpLs),
        Lang::Json => Some(ServerKind::JsonLs),
        Lang::Yaml => Some(ServerKind::YamlLs),
        Lang::Java => Some(ServerKind::Jdtls),
    }
}

fn uri_of(path: &Path) -> String {
    crate::capabilities::file_uri(path).to_string()
}

/// Can this server take a `workspace/symbol` query RIGHT NOW without stalling it?
/// Three gates: the server advertises `workspaceSymbolProvider` (bool or options object);
/// its state is Ready (Indexing means clangd's background index / r-a's cache priming is
/// still running — partial answers at best, parked requests at worst); and rust-analyzer
/// additionally must have reached `quiescent` (Ready alone arrives at handshake completion,
/// long before the index exists — a query then sits until the 20 s sweep eats it).
fn workspace_symbols_ready(
    kind: ServerKind,
    state: &ServerState,
    quiescent: bool,
    caps: &Value,
) -> bool {
    let provides = match caps.get("workspaceSymbolProvider") {
        Some(Value::Bool(b)) => *b,
        Some(Value::Object(_)) => true,
        _ => false,
    };
    provides
        && matches!(state, ServerState::Ready)
        && (kind != ServerKind::RustAnalyzer || quiescent)
}

fn language_id(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "rs" => "rust",
        "c" | "h" => "c",
        "py" => "python",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "ts" | "mts" => "typescript",
        "tsx" => "typescriptreact",
        "css" => "css",
        // css-ls speaks SCSS natively when told so — keep the id honest for .scss.
        "scss" => "scss",
        "html" | "htm" => "html",
        "cs" => "csharp",
        _ => "cpp",
    }
}

fn send_did_open(server: &mut LspServer, path: &Path, text: &str, version: i32) {
    server.send(Outgoing::Notification {
        method: "textDocument/didOpen",
        params: json!({
            "textDocument": {
                "uri": uri_of(path),
                "languageId": language_id(path),
                "version": version,
                "text": text,
            }
        }),
    });
}

/// Byte offset → LSP Position in the server's negotiated encoding.
fn lsp_position(rope: &Rope, byte: usize, enc: Encoding) -> Value {
    let p = match enc {
        Encoding::Utf8 => position::byte_to_point(rope, byte),
        Encoding::Utf16 => position::byte_to_utf16(rope, byte),
    };
    json!({"line": p.line, "character": p.col})
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Item 9 readiness gates: capability + Ready + (rust-analyzer only) quiescent. A
    /// half-indexed rust-analyzer — Ready but not quiescent — must be skipped for the query.
    #[test]
    fn workspace_symbols_readiness_gates() {
        let provider_bool = json!({"workspaceSymbolProvider": true});
        let provider_obj = json!({"workspaceSymbolProvider": {"resolveProvider": true}});
        let provider_off = json!({"workspaceSymbolProvider": false});
        let no_provider = json!({});
        let ready = ServerState::Ready;

        // clangd: Ready + capability suffices (bool or options-object form).
        assert!(workspace_symbols_ready(ServerKind::Clangd, &ready, false, &provider_bool));
        assert!(workspace_symbols_ready(ServerKind::Clangd, &ready, false, &provider_obj));
        // …but not while its background index is still running, or without the capability.
        assert!(!workspace_symbols_ready(
            ServerKind::Clangd,
            &ServerState::Indexing(Some(40)),
            false,
            &provider_bool
        ));
        assert!(!workspace_symbols_ready(ServerKind::Clangd, &ready, false, &provider_off));
        assert!(!workspace_symbols_ready(ServerKind::Clangd, &ready, false, &no_provider));

        // rust-analyzer: Ready is NOT enough — quiescent is the real signal.
        assert!(!workspace_symbols_ready(ServerKind::RustAnalyzer, &ready, false, &provider_bool));
        assert!(workspace_symbols_ready(ServerKind::RustAnalyzer, &ready, true, &provider_bool));
        // Crashed/pre-handshake states never take queries, quiescent or not.
        assert!(!workspace_symbols_ready(
            ServerKind::RustAnalyzer,
            &ServerState::Crashed { restarts: 1, next: Instant::now() },
            true,
            &provider_bool
        ));
        assert!(!workspace_symbols_ready(
            ServerKind::RustAnalyzer,
            &ServerState::Initializing,
            true,
            &provider_bool
        ));
    }
}
