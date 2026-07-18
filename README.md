# Cauldron 🜍

A native **Rust + egui/wgpu IDE** — JetBrains-style layout and flow, local-first AI, and a
NASA/JPL **Power-of-Ten** enforcement layer aimed at flight-software work such as
[NASA cFS](https://github.com/nasa/cFS). Raw code goes in; standards-compliant code comes out.

No JVM, no Electron, no account, no telemetry — a single native binary that starts instantly.

## Why it exists

Most IDEs are either heavyweight (JVM/Electron) or thin editors bolted to plugins. Cauldron aims
for the JetBrains *experience* on a native Rust stack, plus two things nothing else ships:

- **A coding-standards layer that actually enforces.** GSFC 582 / JPL Power-of-Ten rules run as a
  live analysis tier alongside the language server — unbounded loops, dynamic allocation, missing
  assertions, deep pointer indirection, recursion, `goto`, token pasting. Violations aren't just
  flagged: **"Show Hot Fix"** sends the offending span to an AI and applies the refactor back over
  it, undo-safely.
- **AI that runs on your machine.** Ghost-text completion and the assistant panel talk to a local
  [Ollama](https://ollama.com) model by default (fill-in-the-middle for completion), so it works
  offline with nothing leaving the box. A Claude backend is available too, and a failed cloud call
  degrades gracefully to the local model.

## Features

**Editing** — rope buffer with incremental tree-sitter, virtualized paint, multi-caret, column
select, folds, soft wrap (CJK/wide-char aware), rainbow brackets, snippets with tab-stops,
inline blame, auto-save.

**Language intelligence** — LSP client (clangd, rust-analyzer, pyright, TypeScript, CSS/HTML,
JSON, YAML, C#, Java/jdtls) with completion ranking, signature help, inlay hints, code actions,
rename, go-to-definition/implementation, call hierarchy, and a workspace symbol index. Missing
servers install themselves.

**Debugging** — DAP client (lldb-dap, debugpy, netcoredbg) with breakpoints and conditions,
exception breakpoints, threads view, watches, and inline variable values at the stop point.
Build-before-debug so you never debug a stale binary.

**Version control** — status/stage/commit, diff viewer, blame, log with cherry-pick/revert/
soft-reset, stashes, an in-editor merge-conflict resolver, PR review panel, and AI-drafted commit
messages.

**Navigation** — Search Everywhere (double-Shift), quick-open, symbol search, find/replace in
files, recent locations, structure view, bookmarks.

**Extras** — local history with labels (a safety net independent of git), integrated terminal,
run/debug configurations, test runner with coverage, markdown preview, and a live web preview
server with auto-reload for HTML/CSS work.

## Crates

| crate | what |
|---|---|
| `crates/cauldron` | the IDE application (eframe/egui) |
| `crates/cauldron-editor` | text engine: rope + incremental tree-sitter + virtualized egui widget |
| `crates/cauldron-lsp` | LSP client: threads-not-tokio, incremental `didChange` from transactions |
| `crates/cauldron-dap` | Debug Adapter Protocol client |
| `crates/cauldron-psi` | whole-program C index (call graph, Power-of-Ten Rule-1 analysis) |
| `crates/cauldron-lint` | Power-of-Ten analyzer + clang-tidy/cppcheck adapters; also a CLI |
| `crates/cider` | integrated terminal (PTY + VTE), vendored from the author's Rune desktop |
| `crates/livewall-uikit` | shared chrome/theme, vendored from the author's Rune desktop |
| `vendor/egui-winit` | patched `egui-winit` (clipboard under Wayland) — see its `VENDORED.md` |

## Building

Rust stable and a Linux desktop (Wayland or X11). The repository is self-contained:

```sh
cargo build --release
cargo test --workspace
```

The binary lands at `target/release/cauldron`. To install it with a desktop entry:

```sh
install -Dm755 target/release/cauldron ~/.local/bin/cauldron
```

System dependencies on a Debian/Ubuntu-like host:

```sh
sudo apt install libgtk-3-dev libxkbcommon-dev libwayland-dev \
  libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev
```

egui/eframe are pinned to **0.29.1** (wgpu 22.1.0); the vendored `egui-winit` patch is tied to
that line, so don't bump one without the other.

### Optional: local AI

Install [Ollama](https://ollama.com) and pull the default models:

```sh
ollama pull qwen2.5-coder:1.5b-base   # fill-in-the-middle ghost text
ollama pull qwen2.5-coder:7b          # assistant / refactoring
```

On a machine with an NVIDIA GPU, make sure you have the CUDA-enabled Ollama build (on Arch:
`ollama-cuda`, not `ollama`) or inference silently falls back to the CPU. Models and the server
URL are configurable in **Settings ▸ AI**.

## Status

Actively developed and used daily by the author. Linux-first; the Wayland/X11 feature set and the
window chrome assume a Linux desktop.

## License

MIT — see [LICENSE](LICENSE). The `vendor/` directory contains third-party code under its own
upstream licence.
