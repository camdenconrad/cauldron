# Cauldron

A native Rust IDE, rendered with egui/wgpu, built to write flight-software-grade C and Rust — with a NASA/JPL Power-of-Ten rule checker running as live diagnostics instead of a separate lint pass.

Cauldron is a JetBrains-style editor (project tree, tabs, run configs, integrated terminal, git panel) with no JVM, no Electron shell, and no telemetry — a single native binary. Its point of difference is the lint layer: a hand-written tree-sitter-based analyzer walks C source for the classic Power-of-Ten violations (unbounded recursion, unbounded loops, dynamic allocation after init, missing assertions, deep pointer indirection, and more) and surfaces them as editor squiggles with the JPL rule citation attached, backed by a whole-program call-graph index for cross-file recursion detection. The target workload is code like NASA cFS — C written under hard static-analysis constraints.

## Features

- **Editing** — rope-based buffer (`ropey`), incremental tree-sitter parsing and highlighting, multi-caret selection, snippets/live templates, viewport-virtualized rendering for large files
- **NASA/JPL diagnostics** — Power-of-Ten rules 1–5, 8, 9 checked live via `cauldron-lint`, with stable fingerprints for baseline suppression and a witness chain for recursion cycles
- **Whole-program indexing** — `cauldron-psi` builds a cross-file call graph (including macro-textual vs. preprocessed vs. config-gated tiers) to catch recursion the file-local analyzer can't see
- **Language intelligence** — LSP client (`cauldron-lsp`) supporting clangd, rust-analyzer, pyright, TS/JS, CSS/HTML, JSON/YAML, C#, and jdtls
- **Debugging** — DAP client (`cauldron-dap`) for breakpoints, stepping, and inspection
- **Version control** — git panel: blame, diff view, conflict resolution, history
- **Integrated terminal** — a vendored terminal emulator (`cider`, on `alacritty_terminal`) with its own eframe/wgpu render loop
- **Local AI** — optional Ollama-backed ghost-text completion and refactor assistance, wired to send Power-of-Ten violation spans directly to the model for a "hot fix"

## Architecture

Cargo workspace of focused crates:

- `cauldron` — the app itself (egui/eframe UI, panels, project/workspace state)
- `cauldron-editor` — the text engine: buffer, syntax, selection, highlighting, the virtualized editor widget
- `cauldron-lsp` — LSP client with its own threaded stdio transport (no tokio)
- `cauldron-dap` — DAP client for debugger integration
- `cauldron-psi` — whole-program C/Rust index and call graph, used for cross-file lint and navigation
- `cauldron-lint` — the Power-of-Ten rule engine and standalone CLI
- `cider` — vendored terminal emulator, its own eframe/wgpu binary embedded via PTY
- `livewall-uikit` — shared theme/chrome used by `cauldron` and `cider`

Rendering goes through eframe's `wgpu` backend (pinned to Vulkan by default to avoid backend-probing overhead), not hand-rolled wgpu code — the app is built on egui's immediate-mode widget model, with `cauldron-editor`'s `EditorView` as the main custom widget.

The buffer only mutates through a single `Transaction` chokepoint, which feeds undo/redo, incremental tree-sitter reparsing, and LSP `didChange` notifications from one source of truth.

## Building

```sh
sudo apt install libgtk-3-dev libxkbcommon-dev libwayland-dev \
  libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev

cargo build --release
cargo test --workspace
```

Run with `cauldron [PATH]`, where `PATH` is a file or project directory (defaults to the current directory).

Optional local AI features require [Ollama](https://ollama.com):

```sh
ollama pull qwen2.5-coder:1.5b-base   # ghost-text completion
ollama pull qwen2.5-coder:7b          # assistant / refactor
```

## Status

Actively developed and used daily by the author. Linux-first (Wayland/X11). CI builds and tests the full workspace on every change; clippy runs advisory-only.

## License

MIT.
