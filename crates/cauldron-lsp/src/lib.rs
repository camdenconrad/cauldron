//! cauldron-lsp — the LSP client behind the Cauldron IDE.
//!
//! Design of record: the P2 synthesis (threads-not-tokio; see docs/phase0.md Area 2).
//! Per server: a blocking READER thread frames Content-Length JSON-RPC off child stdout,
//! deserializes off the UI thread, resolves responses / auto-replies server requests / converts
//! notifications into [`LspEvent`]s pushed over `std::sync::mpsc`, then wakes the UI via the
//! injected [`Notifier`] closure (the cider PTY template). A dedicated WRITER thread owns stdin so
//! a wedged server can never stall a frame. The crate is egui-free: everything except the two
//! `#[ignore]`d live smokes runs headless.
//!
//! didChange is DERIVED FROM TRANSACTIONS ([`txsync`]): one `TextDocumentContentChangeEvent` per
//! `Change`, emitted in DESCENDING start order so every event's pre-edit range is still valid when
//! the server applies them sequentially — genuinely incremental, O(edit) not O(file).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

pub mod capabilities;
pub mod discovery;
pub mod jsonrpc;
pub mod manager;
pub mod server;
pub mod transport;
pub mod txsync;

pub use lsp_types;
pub use manager::LspManager;
pub use server::ClangdOptions;

/// Wakes the UI thread after an event lands on the queue (`ctx.request_repaint()` in the app;
/// a no-op closure in tests). Injected so this crate never depends on egui.
pub type Notifier = Arc<dyn Fn() + Send + Sync>;

/// Identity of one running language-server process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ServerId(pub u32);

/// Which language server binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ServerKind {
    Clangd,
    RustAnalyzer,
    /// `pyright-langserver --stdio` (npm: pyright).
    Pyright,
    /// `typescript-language-server --stdio` (npm: typescript-language-server + typescript).
    TsServer,
    /// `vscode-css-language-server --stdio` (npm: vscode-langservers-extracted).
    CssLs,
    /// `vscode-html-language-server --stdio` (npm: vscode-langservers-extracted).
    HtmlLs,
    /// `csharp-ls` (dotnet tool: `dotnet tool install -g csharp-ls`). Roslyn-based, speaks
    /// LSP over stdio with no flag.
    CSharpLs,
    /// `vscode-json-language-server --stdio` (npm: vscode-langservers-extracted).
    JsonLs,
    /// `yaml-language-server --stdio` (npm: yaml-language-server).
    YamlLs,
    /// `jdtls` — the Eclipse JDT Language Server wrapper (Java). Needs a JDK 17+ and the
    /// `jdtls` launcher on PATH; the launcher manages the per-project workspace data dir.
    Jdtls,
}

impl ServerKind {
    /// Short human name for the status bar and spawn-failure messages.
    pub fn display_name(self) -> &'static str {
        match self {
            ServerKind::Clangd => "clangd",
            ServerKind::RustAnalyzer => "rust-analyzer",
            ServerKind::Pyright => "pyright",
            ServerKind::TsServer => "tsserver",
            ServerKind::CssLs => "css-ls",
            ServerKind::HtmlLs => "html-ls",
            ServerKind::CSharpLs => "csharp-ls",
            ServerKind::JsonLs => "json-ls",
            ServerKind::YamlLs => "yaml-ls",
            ServerKind::Jdtls => "jdtls",
        }
    }
}

/// The negotiated position encoding. utf-8 collapses position math to "line + byte column";
/// utf-16 is the mandatory LSP-default fallback. Negotiation: `capabilities.positionEncoding`
/// → legacy `offsetEncoding` → Utf16.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    Utf16,
}

/// Lifecycle state of one server, surfaced in the status bar.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerState {
    Spawning,
    Initializing,
    /// Background indexing with an optional percentage (clangd backgroundIndexProgress,
    /// rust-analyzer cachePriming / Roots Scanned).
    Indexing(Option<u32>),
    Ready,
    Crashed { restarts: u32, next: Instant },
}

/// Everything the reader/dispatch layer reports up to the app. Drained via
/// [`LspManager::pump`] on the UI thread.
#[derive(Debug, Clone)]
pub enum LspEvent {
    /// `textDocument/publishDiagnostics` (push leg — clangd always, rust-analyzer flycheck).
    Diagnostics {
        path: PathBuf,
        /// Doc version the server computed against (stale-drop when != last sent).
        version: Option<i32>,
        diags: Vec<lsp_types::Diagnostic>,
    },
    /// Response to a `textDocument/diagnostic` pull (rust-analyzer's native channel).
    PullDiagnostics {
        path: PathBuf,
        /// Doc version stamped at request time.
        version: i32,
        diags: Vec<lsp_types::Diagnostic>,
    },
    /// clangd resolved a header/source counterpart. `to` is `None` when it knows of none —
    /// the app falls back to its own extension-swap guess.
    SwitchSourceHeader { from: PathBuf, to: Option<PathBuf> },
    /// Server asked every open doc to be re-pulled (`workspace/diagnostic/refresh`).
    PullAllDiagnostics,
    /// Lifecycle / indexing progress for the status bar.
    State(ServerState),
    /// rust-analyzer `experimental/serverStatus` reached quiescent — features are trustworthy.
    Quiescent,
    /// The server process exited (reader hit EOF). The manager schedules a respawn.
    Exited,
    /// Functional but impaired (e.g. clangd without a compile_commands.json).
    Degraded { reason: String },
    /// `window/logMessage` / `window/showMessage` worth surfacing.
    Message(String),
    /// `workspace/applyEdit` — the app applies it through normal Transactions.
    ApplyEdit(lsp_types::ApplyWorkspaceEditParams),
    /// Response to a hover request, stamped with the buffer generation at request time.
    Hover { generation: u64, contents: Option<lsp_types::Hover> },
    /// Response to a goto-definition request.
    Definition { generation: u64, locations: Vec<lsp_types::Location> },
    /// Response to a completion request.
    Completions { generation: u64, items: Vec<lsp_types::CompletionItem> },
    /// Response to a rename request: the multi-file edit to apply (None = server refused).
    RenameEdit { generation: u64, edit: Option<lsp_types::WorkspaceEdit> },
    /// Response to a `textDocument/formatting` request: whole-document text edits to apply.
    Formatting { generation: u64, path: PathBuf, edits: Vec<lsp_types::TextEdit> },
    /// Response to a find-references request.
    References { generation: u64, locations: Vec<lsp_types::Location> },
    /// `completionItem/resolve` result — carries lazily-computed additionalTextEdits
    /// (auto-imports) and docs for an accepted completion.
    ResolvedCompletion { generation: u64, path: PathBuf, item: lsp_types::CompletionItem },
    /// Parameter hints for the call under the caret.
    SignatureHelp { generation: u64, help: Option<lsp_types::SignatureHelp> },
    /// Whole-document inlay hints (types, parameter names), stamped with the buffer
    /// generation at request time.
    InlayHints { generation: u64, path: PathBuf, hints: Vec<lsp_types::InlayHint> },
    /// prepareCallHierarchy result — the manager chains incomingCalls internally; the app
    /// never sees this variant (it is consumed in pump).
    CallHierarchyItems { generation: u64, items: Vec<lsp_types::CallHierarchyItem> },
    /// Incoming callers of the prepared symbol: `(caller name, its call-site location)`.
    IncomingCalls { generation: u64, calls: Vec<(String, lsp_types::Location)> },
    /// The file's symbol tree (Structure/outline panel).
    DocumentSymbols {
        generation: u64,
        path: PathBuf,
        symbols: Option<lsp_types::DocumentSymbolResponse>,
    },
    /// Response to a code-action request for a range of `path` (quick fixes, refactorings).
    /// Apply a chosen action's `WorkspaceEdit` via [`txsync::workspace_edit_to_file_edits`];
    /// bare `Command`s go back through [`LspManager::execute_command`].
    CodeActions {
        generation: u64,
        path: PathBuf,
        actions: Vec<lsp_types::CodeActionOrCommand>,
    },
    /// A lazily-resolved code action come back with its `edit`/`command` filled in
    /// ([`LspManager::resolve_code_action`]). Apply it exactly like a resolved-on-arrival one.
    /// `action` is `None` when the server errored or sent something undecodable — the caller
    /// should tell the user the refactor failed rather than silently doing nothing.
    ResolvedCodeAction {
        generation: u64,
        path: PathBuf,
        action: Option<Box<lsp_types::CodeAction>>,
    },
    /// One server's answer to a `workspace/symbol` query ([`LspManager::request_workspace_symbols`]
    /// fans out to every indexed server, so several of these can arrive per query — merge them).
    /// 3.17 `WorkspaceSymbol` rows are normalized into flat `SymbolInformation` at decode time
    /// (a range-less location gets a zero range — the symbol is still openable at its file).
    WorkspaceSymbols {
        generation: u64,
        symbols: Vec<lsp_types::SymbolInformation>,
    },
}
