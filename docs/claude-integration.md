# Cauldron ↔ Claude Code integration (research distilled, for the phase-0 doc)

Goal: run `claude` inside Cauldron's embedded PTY and be a first-class IDE host (like the
JetBrains/VS Code plugins). GOOD NEWS: no IDE allowlist, plain-token auth, protocol stable >12mo,
works against the official `claude` binary. A third party CAN implement this.

## Tier 0 — baseline (nearly free, ~2-3 days once terminal exists)
- Spawn `claude` in cider's PTY `Terminal`; wire I/O to a terminal/chat pane.
- Add a `notify`-crate file watcher on the workspace → when Claude edits files on disk, hot-reload
  open buffers live (this IS the "real-time" the user wants). Prompt on conflict if buffer dirty.
- WHAT'S LOST without Tier B: native diff review, auto selection/open-file context, diagnostics
  sharing, `/ide` auto-connect. Still usable.

## Tier B — IDE-integration server (~5-7 days) — makes Cauldron a real Claude host
Transport = **WebSocket, JSON-RPC 2.0**, bound to `127.0.0.1:0` (random port).

### Lockfile (VERIFIED via gh issue #14421)
Path: `~/.claude/ide/<port>.lock`  (dir mode 0700, file mode 0600)
```json
{
  "workspaceFolders": ["/abs/workspace"],
  "pid": 27256,
  "ideName": "Cauldron",
  "transport": "ws",
  "runningInWindows": false,
  "authToken": "<random base64, 80+ chars>"
}
```
- Write on startup, delete on shutdown, regenerate (new token+port) each session.
- `claude` scans `~/.claude/ide/*.lock`, validates pid alive (`ps -p`), prefers lockfile whose
  `workspaceFolders` contains cwd.

### Auth
- Client connects to `ws://127.0.0.1:<port>` with header
  `x-claude-code-ide-authorization: <authToken>`. IDE validates before accepting.

### Env vars injected into the PTY so `claude` auto-connects
- `ENABLE_IDE_INTEGRATION=true`
- `CLAUDE_CODE_SSE_PORT=<port>`  (INFERRED, not officially documented)
- also honor `autoConnectIde: true` in settings.json; `/ide` triggers manual discovery.

### MCP tools EXPOSED to the model (DOCUMENTED)
- `mcp__ide__getDiagnostics` — input `{ filePath?: string }` → `{ diagnostics: [{file,line,message,
  severity: "error"|"warning"|"info"}] }`.  **← we push BOTH clangd/LSP diags AND NASA-lint findings here.**
- `mcp__ide__executeCode` — Jupyter only; SKIP for Cauldron (no notebooks).

### Hidden JSON-RPC methods the CLI calls (INFERRED — names may drift, test against real `claude`)
- `openDiff(before, after, filePath)` — BLOCKING; open native side-by-side diff w/ Accept/Reject;
  capture user edits to the proposal; return accepted/rejected/modified. **This is where Cauldron
  shines — a native egui/wgpu diff widget.**
- `getCurrentSelection()` → `{file, range:[start,end], text}` (sent as context each prompt)
- `openFile`, `getActiveFile`, `getWorkspaceFolders`, `getOpenFiles`, `saveDocument`,
  `checkDocumentDirty`, `updateSelection`

### Third-party risks
- Hidden RPC method names are INFERRED (not in official docs); stable ~12mo but monitor releases.
- `openDiff` is blocking/modal — design the diff UX to be prominent.

## Alternative path (note, don't take for v1): Claude Agent SDK
- Embed SDK, render our own chat UI, intercept `Edit` via `PreToolUse` hook → our native diff
  approval. More control, more boilerplate, no built-in diff UX. Recommend CLI+MCP path for v1;
  keep SDK as a later option if we want a bespoke chat panel not tied to a pre-installed `claude`.

## Cited: code.claude.com/docs (vscode, jetbrains, env-vars, agent-sdk), gh issue #14421,
## gh discussion zed#58338, github.com/firish/claude_code_vs
