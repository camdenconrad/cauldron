# Cauldron — scope of record (v1)

Cauldron is a native Rust + egui/wgpu IDE for the Rune desktop, modeled on IntelliJ RustRover's
layout and flow, daily-driven for C/Rust work, hosting Claude Code as a first-class integration,
and differentiated by a NASA/JPL Power-of-Ten enforcement layer aimed at contributing to NASA cFS.

Core design + staged feasibility gates: see [phase0.md](phase0.md) (Gate A = editor CPU budget +
clangd-on-cFS; Gate B = one owner-signed Power-of-Ten finding cFS's own checks miss).
Claude-host protocol details: [claude-integration.md](claude-integration.md).

## Stack (decided)

- **egui 0.29.1 + eframe 0.29.1 (wgpu 22.1.0), NO Tauri.** Matches the whole Rune app family;
  the IDE's heart (text widget, diff widget) is native egui painting. A `wry` webview is a
  keep-in-pocket option later for an HTML/CSS/JS live-preview *pane* only.
- Self-contained repo. The shared Rune pieces are vendored in-tree: `crates/livewall-uikit`
  (theme/chrome), `crates/cider` (terminal), and the patched `egui-winit` clipboard fork at
  `vendor/egui-winit` (`[patch.crates-io]`). A fresh clone builds with no sibling checkouts.
- Crates: `cauldron` (bin, eframe app), `cauldron-editor` (egui widget lib: rope + tree-sitter +
  virtualized viewport), `cauldron-lint` (lib + CLI, no eframe).
- Rune-native app wiring: app_id `com.coffee.cauldron`, custom titlebar (no SSD), occult/autumn
  uikit theme (rust/bone/ember).

## Languages

- **v1: C + Rust.** clangd + rust-analyzer (LSP), lldb-dap (debugging, both languages),
  tree-sitter-c / -rust / -cpp (highlighting). NASA enforcement is C-first (cFS is pure C).
- **Later: Python / JS / TS / CSS / HTML** (user has WebStorm meanwhile). The LSP+DAP+grammar
  registries are TOML-config-driven, so adding a language is declarative, not architectural.

## UX: RustRover layout & flow

- Tool-window shell: left Project tree, center tabbed editors, bottom dockable tool windows
  (Terminal / Problems / Run / Debug / VCS) with IntelliJ-style stripe toggle buttons.
- Top toolbar: Run Configuration dropdown + run/debug/stop (top-right).
- Editor: gutter run arrows, click-to-toggle breakpoints, right-hand error-stripe column.
- Search Everywhere (double-Shift), Find in Files (Ctrl+Shift+F), quick-open (Ctrl+P),
  Find Action / palette (Ctrl+Shift+A), Recent Files (Ctrl+E), Settings (Ctrl+Alt+S).
- Status bar: LSP status, Ln/Col, encoding, active NASA standard, and the **Claude usage meter
  pinned bottom-right** (parse `~/.claude/projects/**/*.jsonl` for session tokens/cost; OTEL
  counters as an alternative source).

## Claude Code host (needed for work — lands right after the shell)

- Tier 0: `claude` runs in the embedded cider-derived terminal; a `notify` file-watcher hot-reloads
  open buffers the instant Claude edits files (prompt on dirty-buffer conflict).
- Tier B: implement the IDE-integration server — `~/.claude/ide/<port>.lock` lockfile +
  loopback WebSocket (JSON-RPC 2.0) + `x-claude-code-ide-authorization` token; inject
  `ENABLE_IDE_INTEGRATION` / `CLAUDE_CODE_SSE_PORT` into the PTY; expose
  `mcp__ide__getDiagnostics` (LSP **and** NASA-lint findings — Claude sees Power-of-Ten
  violations live) and the internal RPCs (`openDiff` native egui diff w/ accept/reject/modify,
  `getCurrentSelection`, `openFile`, `getOpenFiles`, `saveDocument`, …). Details in
  claude-integration.md; internal RPC names are inferred — validate against the real `claude`.

## Phases

- **P0** feasibility spike (Gate A) — throwaway benches, see phase0.md
- **P1** editor foundation (`cauldron-editor`)
- **P2** LSP client (clangd + rust-analyzer, utf-8 negotiation, Transaction-derived didChange)
- **P3** RustRover-style shell (tool windows, tabs, tree, terminal, search, palette, settings,
  session restore, spellcheck via `spellbook` for comments/strings) — **IDE ships here**
- **P4** Claude host (Tier 0 + Tier B) + usage meter
- **P5** build/run: run configurations (cargo / CMake / make / custom), output parsing →
  clickable diagnostics, gutter run arrows
- **P6** live debugging via DAP (`lldb-dap`): breakpoints, step, variables, call stack, watches
- **P7** NASA hybrid lint (Gate B first): clang-tidy + cppcheck adapters + tree-sitter PoT
  analyzer, waivers-or-error, SARIF — the flagship
- **P8** cFS contribution loop (external track, never blocks IDE-done)
- **P9** multi-language (Py/JS/TS/CSS/HTML), later

## Out of scope (v1)

- **Plugins** — no plugin API, no WASM VM, no marketplace (user decision 2026-07-11). TOML
  registries for servers/grammars/themes/keymaps are config, not a plugin system.
- Deep refactoring engine beyond LSP rename/code actions; multi-root workspaces; IME/RTL;
  soft-wrap in code mode; remote/LAN features; non-C Power-of-Ten parity.
