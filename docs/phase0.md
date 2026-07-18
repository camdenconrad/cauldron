> **Repo note (2026-07-11, supersedes workspace wiring below):** Cauldron lives in its OWN repo
> (`camdenconrad/cauldron`, ~/RustroverProjects/cauldron), not as a livewall-studio workspace member.
> Crate package names are `cauldron`, `cauldron-editor`, `cauldron-lint` (no `livewall-` prefix).
> Shared Rune pieces are vendored in-tree: `crates/livewall-uikit`, `crates/cider`, and
> `[patch.crates-io] egui-winit` → `vendor/egui-winit`. A fresh clone builds standalone.
> Scope of record (RustRover UX, Claude host, DAP, build/run, no plugins): [scope.md](scope.md).

# Cauldron Phase 0 — feasibility spike + design of record

**Verdict: TO BE PROVEN.** This doc is the pre-spike design and the **staged** GO/NO-GO gates. The single
bundled gate the first draft carried was wrong: it only fired *after* essentially the whole editor + LSP +
lint + cFS-build stack was built, so it de-risked nothing cheaply. This version splits the decision into
gates that can each independently kill or narrow the epic, ordered cheapest-first:

- **Gate A (Phase 0 exit, cheap):** can the editor hold its per-keystroke CPU budget, and does clangd index
  a locally-built cFS off one `compile_commands.json`? A NO here stops the epic before the editor is built
  for real.
- **Gate B (before Phases 4-5):** does the hybrid linter have *complementary* value — one defensible
  Power-of-Ten finding cFS's own checks miss? A NO here ships the editor+LSP and drops the NASA layer.
- **CLA track (parallel, external):** the NASA contribution paperwork. Human-latency; gates only the cFS
  PR-merge loop, **never** whether the IDE is "done."

The sequencing is fixed and now *consistent* with the gates: **editor + LSP daily-driver FIRST (Gate A →
Phases 1-3), then the NASA hybrid lint layer (Gate B → Phases 4-5).** Everything here is grounded in the
running codebase (redit, cider, uikit) and a tooling probe done 2026-07-11; the numbers are targets to hit,
not results yet.

Cauldron is a native Rune IDE (`crates/cauldron`, bin `cauldron`, app_id `com.coffee.cauldron`). It must
earn its keep as a **daily-driver** on its own merits — a rope+tree-sitter+LSP core that beats redit at cFS
scale. Its *distinctive* bet is the hybrid NASA/JPL lint layer; and the tool exists partly to feed a
compounding **cFS** contribution loop (point the linter at a locally-built cFS, turn genuine **Power-of-Ten**
observations into issues/PRs, feed false-positives back as regression fixtures). But that loop rides NASA's
external human process, so it is a separate track and is explicitly **not** an IDE-shippability gate.

---

## Gate A — the two questions Phase 0 answers cheaply

Phase 0 builds only throwaway skeletons (a headless `cauldron-editor` bench + a `cauldron --lsp-check`
harness), **not** Phases 1-5. It answers exactly two feasibility questions:

1. **Can a rope + incremental-tree-sitter + viewport-virtualized egui widget edit a real 5k-line cFS `.c`
   within its per-keystroke CPU budget?** redit's `TextEdit`-over-`String` re-lays the whole buffer **on
   every edit** — egui caches galleys by layout-job content hash, so *idle* frames are cheap, but each
   keystroke invalidates the cache and forces an O(file) relayout (`crates/editor/src/main.rs:825-829,
   841-849,1078-1110`), fatal at cFS scale. The whole editor thesis rests on replacing that with
   per-visible-line galleys. **Honest metric (see Area 1):** we measure the editor's *CPU* work
   (rope splice + `tree.edit` + incremental parse + viewport query + dirty-line layout + egui
   tessellation) in isolation and gate it at **≤8 ms p99** — half a 60 Hz frame. This is a headless proxy;
   it deliberately omits wgpu buffer upload, GPU present, vblank, and the rest of the app's per-frame work
   (file tree, embedded terminal, Problems panel — all re-run every frame in egui's single-threaded
   immediate mode). The true **end-to-end keystroke→paint ≤16 ms** is a *Phase 3* acceptance criterion,
   measured in the running app, not claimed from this bench.
2. **Does clangd actually index a locally-built cFS?** This has two sub-questions the first draft
   conflated: (a) **single-TU** — `didOpen` a cFS `.c` with the arch `compile_commands.json` wired →
   `publishDiagnostics` within **≤10 s**, generated headers resolving; and (b) **whole-project** —
   cross-module go-to-def (ES→SB→OSAL) needs clangd's `--background-index` **fully built over all of cFS**,
   which is *minutes* of indexing plus significant RAM and a large `.cache/clangd/index` on disk, not a 10 s
   operation. Both are gated; (b) records index wall-clock, peak clangd RSS, and index-dir size, and
   confirms the box has the headroom.

The position-desync correctness (byte↔UTF-16 round-trip) and the threads-not-tokio LSP transport are proven
inside Gate A too (Areas 1-2), but they are correctness properties, not the expensive risk.

**Deferred out of Phase 0** (they answer no feasibility question the skeletons can't, and inflate the
spike into the whole epic): the linter's *complementary value* proof is **Gate B**; the CLA is the **CLA
track**; and Phase-1 editor polish (multi-cursor/undo coalescing, tab hit-testing, incremental-highlight
correctness) plus Phase-4 lint internals (SARIF emit + schema-validation, waiver semantics, clang-tidy YAML
ingestion, cppcheck graceful degradation, dedupe) are built in their own phases.

---

## Crate family (design of record)

A three-crate family. The Rune norm is single-crate apps, so we split only where a piece is independently
useful or must run headless:

| Crate | Package | Artifacts | eframe? | Why the boundary |
|---|---|---|---|---|
| `crates/cauldron` | `livewall-cauldron` | `[[bin]] cauldron` | yes | The IDE app shell + LSP module. Depends on uikit, cider (lib), cauldron-editor, cauldron-lint. |
| `crates/cauldron-editor` | `livewall-cauldron-editor` | `lib` | **no** (egui only) | The rope/tree-sitter text engine = the single biggest technical risk. A library so the Gate-A spike, unit tests, benches, and property tests run **headless** with no app/LSP/terminal. |
| `crates/cauldron-lint` | `livewall-cauldron-lint` | `lib` + `[[bin]] cauldron-lint` | **no** | The flagship NASA engine. Headless in CI and the cFS PR loop; heavy deps (tree-sitter, YAML/XML, subprocess) stay out of the GUI tree. |

The LSP client stays an **in-crate module** (`crates/cauldron/src/lsp/`), not a crate — it needs
`egui::Context` for repaint and is not independently useful yet; promotable later behind a clean seam.

Workspace wiring: add `"crates/cauldron"`, `"crates/cauldron-editor"`, `"crates/cauldron-lint"` to root
`Cargo.toml [workspace].members` (after `"crates/cider"`, before `"crates/watchtower"`). All three inherit
the root `[patch.crates-io]` (vendored egui-winit 0.29.1 + smithay forks) and `[workspace.dependencies]`
automatically.

### Module trees

```
crates/cauldron-editor/ (lib — egui, ropey, tree-sitter + 3 grammars, unicode-segmentation)
  buffer.rs      Buffer{rope,version,line_ending,path,max_display_width}; the single apply(Transaction) chokepoint; load/save (lift redit write_atomic)
  change.rs      Change{range:Range<byte>, insert}; Transaction (ordered Vec<Change> + inverse + Selections); ChangeSet position-mapping
  history.rs     undo/redo Vec<Transaction>; time+adjacency coalescing (type-a-word undo); restores carets from snapshot   [POLISH → Phase 1]
  selection.rs   Selection{anchor,head} bytes; Selections{ranges,primary} multi-cursor; GRAPHEME/word/line motion; overlap merge
  syntax.rs      Language{C,Cpp,Rust}→grammar+HIGHLIGHTS_QUERY; persistent Tree; edit()+incremental parse (parse_with old_tree); bounded reparse via ParseOptions progress callback (tree-sitter ≥0.25) — API pinned in Gate A
  highlight.rs   viewport-range QueryCursor → per-line spans; capture-name→Color32; span cache keyed (line, tree_version); stale-tree span-shift during reparse window
  layout.rs      per-line LayoutJob (spans + tab expansion + src↔display column map); maintains an incremental approximate max display-width for the virtual horizontal extent; leans on egui's galley cache
  render.rs      the virtualized painter: ScrollArea::show_viewport, visible-line loop, gutter/carets/selection/squiggles/folds (lift redit)
  input.rs       egui Event → Transaction/selection ops; hit-testing via galley pos_from_cursor (NOT col_w arithmetic); clipboard via arboard wlr-data-control + Shift+Insert
  position.rs    canonical byte model + ALL conversions (byte ↔ ts-Point ↔ LSP-Position) — the single home of the UTF-16 bug
  diagnostics.rs Diagnostic{range,severity,source:Lsp|Nasa|Spell,message} + per-line index for gutter/squiggle
  metrics.rs     row_h, mono advance, tab_width; ASCII advance is uniform (cFS is pure ASCII) — but caret x still comes from galley glyph runs so non-ASCII stays correct
  find.rs        rope-backed search (lift redit char_matches/find_match_byte)
  tests/,benches/  the Gate-A harness: headless Buffer edits with timers + explicit tessellation; position.rs property tests
```
```
crates/cauldron/ (bin — eframe 0.29.1, uikit, cider lib, cauldron-editor, cauldron-lint, ignore, lsp-types)
  main.rs        NativeOptions(app_id, decorations(false), resizable(true)); VULKAN backend (copy cider main.rs:37-57); --check + --lsp-check self-tests
  app.rs         eframe::App: panel composition + global key dispatch + palette/quick-open overlays
  workspace.rs   Workspace{root,tree,git,lsp_roots,nasa_cfg}; open(); recents
  tree.rs        lazy dir read via `ignore` (gitignore-aware); tree UI + git status tint
  git.rs         `git status --porcelain=v2` + branch (shell-out; no libgit2)
  editor/        thin host embedding cauldron-editor per tab + tab strip (reuse cider tab_chip/tab_bar)
  panels/        bottom dock: problems.rs (LSP+NASA), terminal.rs (cider Session), search.rs, output.rs
  lsp/           mod.rs manager.rs server.rs transport.rs dispatch.rs document.rs position.rs capabilities.rs discovery.rs events.rs
  lint.rs        adapter: call cauldron_lint::run_* on active file/workspace, fold into diagnostics   [Phase 4]
  palette.rs     Ctrl+Shift+P command palette + Ctrl+P quick-open (floating Areas)
  session_state.rs  ~/.local/state/cauldron/session.json (atomic tmp+rename); CAULDRON_STATE_DIR override for --check
  keys.rs        global keybinding table + dispatch

crates/cauldron-lint/ (lib+bin — tree-sitter, tree-sitter-c, serde_yml, quick-xml, toml, rayon; NO eframe)   [Phase 4, Gate-B skeleton only in Phase 0]
  lib.rs         pub Diagnostic/RuleId/Standard/Severity/Span/Fix/Waiver + analyze(files,cfg)->Vec<Diagnostic> (the API the GUI links)
  model.rs config.rs engine.rs report.rs (human/json/sarif) main.rs
  adapters/      clang_tidy.rs (--export-fixes YAML) · cppcheck.rs (--xml, feature-detected) · compiler.rs (-Wall, R10)
  pot/           parse.rs · callgraph.rs (Tarjan SCC → recursion) · queries.rs · metrics.rs
```

The Phase-0 `cauldron-lint` footprint is only what Gate B needs: `pot/queries.rs` running **one**
tree-sitter Power-of-Ten query. The adapters, unified model, waiver engine, and SARIF/report layers are
built in Phase 4.

---

## Area 1 — Text engine (`cauldron-editor`)

The bet: replace `TextEdit::multiline` with a `ropey::Rope` behind a single transactional chokepoint, a
persistent per-buffer tree-sitter tree that reparses incrementally, and a custom egui 0.29.1 widget that
paints **only the visible viewport** — one `LayoutJob`/galley per visible line (not one giant galley).

- **Buffer & edits.** Every mutation flows through `Buffer::apply(Transaction)`, which atomically: (1)
  splices the rope, (2) records a `tree_sitter::InputEdit` and reparses, (3) pushes the inverse onto undo,
  (4) maps every selection + diagnostic range through the `ChangeSet`, (5) bumps `version`. Multi-cursor =
  one `Transaction` with a `Change` per caret, applied **back-to-front** so earlier byte offsets stay
  valid; undo restores all carets at once. Line endings detected on load, normalized to `\n`, restored on
  save. (Multi-cursor + undo/adjacency coalescing are *built* in Phase 1; Phase 0 exercises single-caret
  edits only.)
- **Why ropey.** O(log n) splices, cheap Arc-shared snapshot clones, a chunk iterator that feeds
  tree-sitter's `parse_with` zero-copy, and — decisively — native `byte↔char↔line↔utf16_cu` conversions
  that make both the tree-sitter and LSP boundaries one-liners. This is the proven Helix stack. Rejected:
  `crop` (no UTF-16/line helpers → hand-roll the exact conversions that cause the bug), `xi-rope`
  (heavy/stale), `Vec<String>`/status-quo `String`.
- **Caret granularity is decided now (was an open question — it is load-bearing).** Caret positions are
  **Unicode grapheme-cluster boundaries** (via `unicode-segmentation`), stored internally as byte offsets,
  consistent across left/right/word/line motion **and** hit-testing. For pure-ASCII cFS this is identical
  to char boundaries; deciding it up front keeps the position model stable before the spike.
- **Caret/selection GEOMETRY comes from the galley, not arithmetic.** egui does per-glyph font *fallback*
  with non-uniform advances and no complex shaping, so "monospace ⇒ char↔pixel is pure arithmetic" holds
  **only** for glyphs the mono font actually renders at the mono advance (ASCII). CJK (double-width),
  emoji/astral, combining marks, and any fallback glyph in a comment/string would misplace a caret or
  selection rect. So x-positions are read from the laid-out galley's glyph runs
  (`Galley::pos_from_cursor` / per-glyph rects), never `col * advance`. cFS is pure ASCII so the fast path
  is exact there, but the renderer stays correct for the non-ASCII lines the position round-trip tests
  cover — Gate A verifies a caret lands correctly on a line containing a non-ASCII glyph.
- **Incremental syntax.** Persistent `Tree` per buffer; on each edit `tree.edit(&InputEdit)` then
  `parser.parse_with(rope-chunk callback, Some(&old_tree))`. Highlights are produced **per-viewport**: each
  frame a `QueryCursor` with `set_byte_range(first..last visible)` runs `HIGHLIGHTS_QUERY` over ~60-80
  lines regardless of file size; a `(line, tree_version)` span cache keeps scrolling free. **Gotcha #1:**
  tree-sitter `Point.column` is a UTF-8 **byte** column, not a char column — computed in `position.rs`.
- **Virtualized render.** `ScrollArea::both().show_viewport(|ui, viewport|)` with a virtual content size of
  `(total_lines*row_h, max_display_width)`; compute `first/last` visible and paint only those (egui's own
  `show_rows` idiom, ≤~80 galleys/frame). **Horizontal extent:** ropey exposes no max-line-length, and
  scanning `longest_line*advance` every frame is O(file); instead the buffer maintains an **approximate
  incremental `max_display_width`** (grown on insert, only *recomputed lazily* on deletes of the current
  longest line — accepting minor, self-correcting horizontal-scrollbar jitter rather than an O(file) scan).
  Tabs expand to a display string with a src↔display column map. No soft-wrap in code mode; horizontal
  scroll paints each line-galley clipped. Gutter, caret, selection rects, squiggles, and fold triangles are
  lifted/adapted from redit (`main.rs:868-915,917-935,1147-1165`).
- **Worst-case reparse — the escape hatch, fully specified.** Phase 0 stays synchronous if it measures
  under budget. If a pathological reparse (flipping a top-of-file block comment on 5k lines) ever blows the
  frame, we bound one reparse and move it off the frame thread. The subtlety the first draft glossed: when
  a bounded tree-sitter parse is **cancelled** (progress callback returns "stop"), it yields **`None`, not
  a partial tree** — so we still hold the *old* tree, whose byte offsets no longer match the edited rope.
  Painting that stale tree's highlight spans directly would be visibly wrong over the shifted text. So
  during the stale window we **shift the cached spans by the pending `InputEdit`** (a cheap byte-delta
  translation) for the dirty region, or drop highlighting for that region if the edit is structural; the
  full reparse is **re-queued on a worker thread** (with the accumulated edits applied to the tree) and its
  fresh tree is swapped in on completion, triggering a `request_repaint`. Wired only if Gate-A checks force
  it; note the exact bounding API depends on the tree-sitter version pinned in Gate A (the deprecated
  `set_timeout_micros` is replaced by `parse_with_options` + a `ParseOptions` progress callback as of
  0.25, removed in 0.26 — see the pin section).

**Latency budget (honest).** *Phase-0 gate:* editor **CPU layout+tessellate ≤ 8 ms p99** per keystroke on a
real ~5k-line cFS `.c`, measured headless with egui's tessellator run explicitly — this omits wgpu upload,
GPU present, vblank, and the rest of the app frame. *Phase-3 acceptance:* the real end-to-end
keystroke→paint **≤ 16 ms p99** measured in the running app, where the 8 ms editor budget shares one
immediate-mode frame with the file tree, embedded cider terminal, and Problems panel. Invariants that hold
the budget: never lay out/paint non-visible lines; incremental not full reparse; query only the viewport;
span/galley caching makes scroll & idle repaints free; worst-case reparse is the pre-designed escape hatch.

---

## Area 2 — LSP client (`crates/cauldron/src/lsp/`)

Threads, not tokio. Per language-server: a dedicated blocking **reader thread** frames Content-Length
JSON-RPC off the child's stdout, deserializes **off the UI thread** into typed `LspEvent`s, pushes them on
a `std::sync::mpsc` queue, and calls `ctx.request_repaint()` — the exact cider PTY template
(`pty.rs:60-82`). A dedicated **writer thread** owns stdin so a wedged server (mid-index, not draining its
pipe) can never stall a frame — the one place we go beyond cider, because `didChange` payloads are large.
`tokio` stays available (luna/ancs use it) but is unnecessary for 2-4 servers.

- **Lifecycle.** `initialize` (advertise `general.positionEncodings = ["utf-8","utf-16"]`) → store
  negotiated encoding + sync kind + trigger chars + provider flags → `initialized` → flush queued
  `didOpen`. One process per `(language, root)`. Crash = reader EOF / `try_wait()` flips an `exited`
  `AtomicBool` (cider pattern) → manager respawns with exponential backoff (capped N/5 min) and re-opens
  every tracked doc.
- **Incremental didChange — Transaction-derived is primary.** Because `cauldron-editor` routes every edit
  through a `Transaction`, the LSP client derives **one `TextDocumentContentChangeEvent` per `Change`**
  directly from the Transaction — genuinely incremental, O(edit) not O(file). This matters: the naive
  "common-prefix/common-suffix diff of a shadow copy" collapses *N disjoint edits into a single
  file-spanning range*, so a 500-caret multi-cursor edit would re-send everything between the first and
  last caret on every keystroke — O(file) payload that defeats incremental sync. The shadow-copy
  prefix/suffix diff is therefore kept **only** as the correctness backstop for servers that negotiate
  `Full` sync (and as an editor-independent fallback), never the default. Flushes are debounced (~200 ms,
  redit's spell-debounce mechanism).
- **THE position problem.** LSP `Position.character` counts code units in the negotiated encoding
  (default UTF-16). We advertise `["utf-8","utf-16"]`; **both clangd ≥14 and rust-analyzer honor utf-8**
  (LSP 3.17), collapsing the common case to "line-start + byte column" arithmetic. The utf-16 fallback is
  handled by **ropey's native `char_to_utf16_cu`/`utf16_cu_to_char`** — so the rope owns positions and no
  separate `line-index` dependency is needed. All conversions live in `position.rs`, proven by a
  byte↔LSP↔byte round-trip property test over ASCII / BMP (`é`,`中`) / astral (emoji, `𝕏`). (These tests
  cover the *position math*, which is encoding-correct for all corpora; they are distinct from the
  *geometry* tests in Area 1 that read x from the galley — the renderer places carets for astral chars via
  the galley, not via these round-trips.)
- **Features → UI (v1).** publishDiagnostics (squiggles + gutter dots + Problems panel), completion
  (+resolve; floating Area, generation-stamped stale-drop), hover, signatureHelp, definition/declaration
  (F12/Ctrl+B/Ctrl+Click), references, documentSymbol (outline + quick-nav), rename (F2 → `WorkspaceEdit`
  applied end→start per file), formatting (off-by-default toggle). Every feature request is fire-and-forget
  and applied only if its stamped generation still matches.
- **clangd for cFS — single-TU vs whole-project.** Useless without a compilation database. `discovery.rs`
  resolves `compile_commands.json` by explicit override → search (`build/`, `.cauldron/build/`, `build/*/`,
  glob `cmake-build-*/`) → on-demand `cmake -DCMAKE_EXPORT_COMPILE_COMMANDS=ON` (configure only) →
  `.clangd`/`compile_flags.txt` fallback with a `Degraded` banner. Spawn with `--background-index
  --header-insertion=never --clang-tidy=false` (the NASA layer owns clang-tidy; running it inside clangd
  now would double-report). **Two distinct capabilities, gated separately (Gate A):** *single-TU*
  diagnostics arrive within ~10 s of `didOpen` once the TU's own flags + generated headers resolve;
  *cross-module* go-to-def (ES→SB→OSAL) only works after `--background-index` has walked **all** of cFS —
  minutes of CPU, non-trivial RAM, and a `.cache/clangd/index` directory on disk. We record the index
  wall-clock, peak clangd RSS, and index-dir size, and confirm the box has the headroom, rather than
  assuming go-to-def is a 10 s operation.

---

## Area 3 — NASA hybrid lint (`cauldron-lint`)  [Phase 4; Gate B proves the thesis first]

A **hybrid** engine: subprocess adapters wrap the mature semantic tools, a custom tree-sitter-C analyzer
covers the Power-of-Ten rules they cannot express, and everything normalizes into one `Diagnostic`. Only
the single tree-sitter PoT query needed for Gate B exists in Phase 0; the rest is Phase 4.

- **Dual view.** tree-sitter parses **raw pre-preprocess** source (mandatory for R8 directives; also runs
  on un-compilable IDE buffers). clang-tidy/cppcheck/compiler run **post-preprocess** for semantics. Each
  Diagnostic tags its view/provenance so the Problems panel never confuses them.
- **Interactive vs authoritative — R4 does not fire clang-tidy on save.** The live, per-keystroke path is
  **tree-sitter only** (function node span for R4, PoT queries for R2/R5/R8/R9) — it runs on the buffer's
  existing tree with no subprocess. The subprocess adapters (clang-tidy `readability-function-size`,
  cppcheck, compiler) are an explicit **"Run analysis"** action + a CI action, **not** fired on every
  Ctrl+S: parsing a full cFS TU with its generated-header include set is ~1-10 s, so on-save clang-tidy
  would lag saves by seconds. The tree-sitter result is authoritative interactively; clang-tidy reconciles
  on demand.
- **Unified `Diagnostic`** carries a byte range **and** char range **and** line/col (byte for fixes, char
  for the egui galley squiggle math redit already does, line/col for CLI + LSP + GitHub permalinks) plus a
  **line-independent `fingerprint`** (hashes rule + surrounding tokens, not the line number) so edits above
  a finding don't churn baselines. clang-tidy `--export-fixes` YAML gives byte `FileOffset` +
  `Replacements` → map via a per-file newline index (exactly redit's `spell.rs:104-126` byte→char forward
  walk). cppcheck via `--xml` (quick-xml). Adapters fan out with rayon over the tree. (All Phase 4.)
- **Waivers = compliance, not mute.** A finding is suppressed only by a waiver carrying a **non-empty
  justification** (inline `// cauldron:waive rule7 "reason, see req CFE-1234"` or a `[[waiver]]` block in
  `.cauldron/nasa.toml`). A bare `// cauldron:waive rule7` is itself reported as
  `WAIVER-MISSING-JUSTIFICATION` (Error). Waived findings are **retained** (`Severity::Waived`), counted,
  shown under `--show-waived`, and dumped by `cauldron-lint waive-audit`. This "justified-or-it's-an-error"
  behavior is the flagship differentiator over `NOLINT`. (Phase 4.)
- **cFS loop.** `cmake -DCMAKE_EXPORT_COMPILE_COMMANDS=ON` → `cauldron-lint --format sarif` → cluster
  same-rule findings per module into GitHub issues / draft PRs (`gh`) → SARIF 2.1.0 ingests into GitHub
  code scanning → false-positives (`cauldron-lint fp <fingerprint> --reason …`) become **regression
  fixtures** so any future engine change that re-introduces the FP fails CI. Positioned **complementary +
  stricter** (§Area 5), never a re-run of cFS's pipeline. (Phase 5.)
- **In-IDE co-existence.** NASA and LSP diagnostics are **two namespaced layers**, never set-unioned. LSP
  uses clangd's red/yellow; NASA uses uikit **ORANGE** squiggles + a `△JPL`/`△CERT` gutter glyph. The
  Rule-10 compiler adapter is **suppressed per-buffer while clangd is attached** (clangd already emits
  `-Wall`) and kept for CLI/CI. Build/lint output runs in an embedded cider `Session`. (Phase 4.)

---

## Area 4 — App shell, project model, deploy

- **App shell** = standard egui panel composition over `uikit::theme::apply` + `chrome::title_bar`
  (decorationless): left `SidePanel` file tree (Ctrl+B), `CentralPanel` tab strip + editor, bottom
  `TopBottomPanel` dock (Problems / Terminal / Search / Output, Ctrl+J), status bar (per-language LSP state,
  Ln/Col, active NASA standard + finding count, git branch), and floating `Area` overlays for Ctrl+P
  quick-open / Ctrl+Shift+P palette / unsaved-changes confirm.
- **Project model.** Open a folder → `Workspace.root`. Tree/quick-open/search all ride the `ignore` crate
  (ripgrep's gitignore-aware walker, pure-Rust, one index for three uses). Git status via
  `git status --porcelain=v2` + `git branch --show-current` (shell-out, house ethos, no libgit2). Buffers =
  open tabs (cider's tab reap/clamp reused). `.cauldron/nasa.toml` attaches to the workspace root. Session
  state at `~/.local/state/cauldron/session.json` (serde_json, atomic tmp+rename, `CAULDRON_STATE_DIR`
  override) persists workspace root + tab paths + per-tab cursor/scroll + panel layout + recents.
- **Embedded terminal.** Promote `livewall-cider` to a library (`crates/cider/src/lib.rs` re-exporting
  `Session`/`Terminal`/`Pty`; `main.rs` stays the thin bin, keeping cider's tests as the net) and add an
  **additive `Pty::spawn_in(cwd, rows, cols, ctx)`** (`pty.rs:43-44` currently hardcodes `cwd=$HOME`);
  `spawn` delegates to `spawn_in($HOME, …)` so cider's own behavior is unchanged. The render/input glue
  becomes a palette-parameterized `terminal_ui(ui, &mut Session, area, &palette)`. Low-risk fallback if the
  refactor is contentious: lift a thin copy of term/pty/session into Cauldron.
- **Deploy** (mirror the redit block in `crates/deploy/rune-shell.sh:182-202`): ship
  `assets/com.coffee.cauldron.svg` (the existing `crates/*/assets/com.coffee.*.svg` glob installs it, zero
  script change); add a `.desktop` heredoc `Exec=$BIN/cauldron %F`,
  `Categories=Development;IDE;TextEditor;`, `StartupWMClass=com.coffee.cauldron`; claim **code** mimetypes
  only (`text/x-csrc;text/x-chdr;text/x-c++src;text/x-c++hdr;text/rust;text/x-rust`) — **never `text/plain`**
  (redit stays the lightweight text default); add `com.coffee.cauldron` to `RDOCK_PINS`. No autostart
  (launched app, not a daemon). Ship `$BIN/cauldron` + `$BIN/cauldron-lint`.

---

## Area 5 — cFS integration + contribution strategy

cFS is **Apache-2.0** and stays a **user-local clone** (`~/src/cFS`), never vendored into this MIT
monorepo. It is a bundle of submodules (cFE / OSAL / PSP + lab apps + tools) with a **two-stage nested
CMake build**: `prep` runs mission-level CMake which spawns a per-CPU/arch sub-build. Two facts drive
everything:

1. The real FSW translation units (with the right flags) live in the **arch sub-build**, so the
   `compile_commands.json` we want is roughly `build-native_std/native/default_cpu1/compile_commands.json`,
   **not** the top-level mission one. Exact path/front-end is a Gate-A cFS-build check.
2. `prep` **generates config headers into the build tree** (`cfe_platform_cfg.h`, `*_msgids.h`, …). You
   **cannot index a fresh checkout** — clangd drowns in missing-include errors until `prep` has run. The
   generated `-I …/inc` flags are captured inside the arch DB, which is exactly why we key off it.

**Zero-patch DB generation:** CMake honors the *env var* since ≥3.17, so inject
`CMAKE_EXPORT_COMPILE_COMMANDS=1` into cFS's own `prep` — no edit to any cFS `CMakeLists`, tree stays
pristine and PR-ready. `bear` is **not needed**. Surface the arch DB to clangd via a repo-root symlink or a
`.clangd CompilationDatabase:` pointer. **One `compile_commands.json` feeds clangd, clang-tidy, and cppcheck
alike** — that single artifact is the linchpin of the whole integration.

**Complementary, not a reinvention.** cFS CI enforces **clang-format-19** (current `format-check.yml`
invokes `clang-format-19 -style=file … -i` over `*.[ch]`; the older clang-format-10 was Caelum-era) + a
**strict cppcheck** run (`cppcheck --std=c99 --enable=warning,performance,portability,style
--suppress=variableScope`) + CodeQL + gcov/MC-DC, but does **NOT** run clang-tidy and does **NOT** gate the
JPL Power-of-Ten rules or the CERT/JPL clang-tidy families. That gap is the differentiator. **Important
scoping of Gate B's "not flagged by CI" clause:** locally we only cheaply reproduce the *cppcheck leg* (and
clang-format); we do **not** stand up cFS's CodeQL DB or full gcov CI on this box. So Gate B's negative is
proven only against the strict cppcheck run — which is honest, because clang-tidy/PoT findings are
categorically outside cppcheck's and clang-format's scope anyway.

**Contribution playbook** (from cFS `CONTRIBUTING.md`): a signed **individual CLA** is a hard,
human-latency prerequisite — **start it early, in parallel; it never gates IDE-done.** There are **two
CLA forms** — one for the **Framework** repos (cFE / OSAL / PSP) and one for **Apps** — so pick the form for
the module actually targeted in Phase 5 (cFE work ⇒ the Framework form). Email the signed form to
`GSFC-SoftwareRelease@mail.nasa.gov` **and cc `cfs-program@lists.nasa.gov`**. Issue-first, branch
`fix-<ISSUE#>-<summary>`, PR title `Fix #XYZ, …`, draft PRs, weekly CCB, nominal + off-nominal unit tests.
Highest-merge-probability first PRs (seed the loop with wins before harder PoT discussions):
1. **clang-vs-gcc new warnings** — cFS CI compiles gcc; our clang toolchain flags what gcc misses. Best
   risk/reward.
2. **Test-coverage gaps** — cFS runs gcov and explicitly asks for off-nominal tests.
3. **Doc/comment drift** (e.g. cFE #275, a real `good first issue`).
4. **cppcheck delta** (`--enable=all`/MISRA the narrower cFS flag set misses).
5. **Power-of-Ten observations** (flagship, lower merge-rate — always issue-first; a reasoned rejection is a
   labeled false-positive that improves the ruleset).

Because this whole track rides NASA's CLA turnaround, weekly CCB, and PR-acceptance rates — none of which
the author controls — **Cauldron-the-IDE is declared done on Gate A + Phase 1-3 acceptance, and the NASA
lint layer on Gate B + Phase 4.** Merged cFS PRs are the *purpose* but not the *ship gate*.

---

## Power-of-Ten rule → checker mapping (authoritative)

"PoT view" = raw pre-preprocess source via tree-sitter; "semantic view" = post-preprocess via
clang-tidy/cppcheck/compiler. Rules **2, 5, 7, 8, 9** are the tree-sitter differentiators (off-the-shelf
tools express them poorly) and the best Gate-B demo material. The **interactive** path for every rule is
tree-sitter; clang-tidy/cppcheck are the on-demand "Run analysis" reconcilers, never fired on save.

| # | Rule (Holzmann intent) | Primary check | Backing tool(s) | Confidence / approximation |
|---|---|---|---|---|
| **R1** | No goto/setjmp/longjmp/recursion | tree-sitter query (goto/setjmp) **+** whole-repo call-graph → Tarjan SCC (recursion) | tree-sitter + clang-tidy `misc-no-recursion`, `cppcoreguidelines-avoid-goto`/`hicpp-avoid-goto` | goto/setjmp exact. Cross-TU recursion = name-resolved call graph (C has no overloading); indirect fn-ptr recursion invisible → paired with R9 flags |
| **R2** | Every loop a fixed provable upper bound | custom tree-sitter "bounded-loop" heuristic | tree-sitter only (no off-the-shelf check maps here) | Undecidable → **advisory**. PASS canonical `for(i=0;i<N;i++)` with literal/const/#define N; WARN `while(1)`/`for(;;)`. cFS main loops → allowlist |
| **R3** | No heap allocation after init | clang-tidy `cppcoreguidelines-no-malloc`/`hicpp-no-malloc` + tree-sitter alloc-call query (extra allocators) | clang-tidy + tree-sitter; cppcheck memleak/resourceLeak corroborate | "after init" not static → report all allocs, waive init modules via `init_allowlist` globs |
| **R4** | Function ≤ ~60 lines | **tree-sitter node span (authoritative, live)**; `readability-function-size` as an on-demand "Run analysis" reconciler (NOT on save) | tree-sitter + clang-tidy (`hicpp-`/`google-readability-function-size`) | Directly expressible in tree-sitter. High confidence; interactive with no subprocess |
| **R5** | ≥ 2 assertions per function | custom tree-sitter assertion-density | tree-sitter only (no tool computes this) | cFS uses events/status-returns not `assert()` → `assert_macros` **must be seeded per-project**; `exempt_under_lines`; **advisory** until vocab confirmed |
| **R6** | Declare data at smallest scope | cppcheck `variableScope` | **cppcheck (needs install)**; tree-sitter fallback heuristic | cFS CI *suppresses* variableScope → we re-enable. Fallback is syntactic/weaker |
| **R7** | Check every non-void return; validate params | clang-tidy `cert-err33-c` + `bugprone-unused-return-value` (+ tree-sitter for project status types); null-before-deref for params | clang-tidy + tree-sitter; cppcheck nullPointer/uninitvar corroborate | Returns = **warning**; broaden must-check list to `CFE_Status_t`/`int32`. Param validation = **advisory** (dataflow-lite) |
| **R8** | Preprocessor sparingly | custom tree-sitter over `preproc_*` (`##`, variadic `...`, recursive, `#ifdef` density) | tree-sitter (**authoritative — must see raw source**) + clang-tidy `cppcoreguidelines-macro-usage`, `bugprone-macro-parentheses` | Semantic tools run post-preprocess and can't see directives. High confidence on raw source |
| **R9** | ≤ 1 deref, no function pointers | custom tree-sitter (fn-ptr decls/typedefs/calls; `**` multi-deref, `>1 *` declarators) | tree-sitter + clang-tidy `cppcoreguidelines-pro-bounds-pointer-arithmetic` | cFS legitimately uses fn-ptr dispatch tables → **advisory + heavy waivers**; also marks where the R1 graph is incomplete |
| **R10** | All warnings on, zero warnings | compiler adapter `clang -fsyntax-only -Wall -Wextra -Wpedantic -std=c99` (flags from compile DB) | clang / `clang-diagnostic-*` | Build-config gate, not per-line. **Suppressed in-buffer when clangd live**, kept for CLI/CI |

Default severities (all overridable in `.cauldron/nasa.toml`): **R1/R3/R10 = Error; R2/R4/R7 = Warning;
R5/R6/R8/R9 = Advice.** Broader CERT-C net (surfaced under `Standard::Cert`, **not** PoT): `cert-flp30-c`
(no floating-point loop counters — a numeric-hygiene check, **not** evidence of R2's bounded-loop intent),
`cert-env33-c` (no `system()`), `cert-msc30/32-c` (no `rand`), `cert-err34-c` (no `atoi` misuse), etc.
clang-tidy 22.1.6 on this box exposes 604 checks; the engine builds `--checks=-*,<enabled>` from config.

---

## eframe/egui pin + Rune constraints (hard)

- **eframe = 0.29.1, egui = 0.29.1**, identical to cider/redit (`crates/cider/Cargo.toml:14-15`):
  `eframe = { version = "0.29.1", default-features = false, features = ["wgpu","default_fonts","wayland","x11"] }`.
  eframe 0.29.1 ⇒ wgpu 22.1.0. **No 0.30+.** `cauldron-editor` takes **egui only** (not eframe) so it benches
  headless; `cauldron-lint` takes **neither**.
- **VULKAN wgpu backend**, copied from cider `main.rs:37-57` with a `CAULDRON_BACKEND=gl` re-test knob — GL
  enumerates no adapter on this NVIDIA/EGL box and eframe has no fallback (it would abort).
- **Clipboard MUST go through arboard `wayland-data-control` + a Shift+Insert backstop**, never egui's
  smithay-clipboard path. This works because the workspace `[patch.crates-io]` vendors an **egui-winit
  0.29.1 fork** that disables smithay-clipboard so copy/paste falls through to arboard's wlr-data-control —
  the only path that works under Rune. `cauldron-editor`'s `input.rs` consumes clipboard text; the handle
  lives at the app/eframe layer (cider `app.rs:437-552,568-579`). **A fresh clone won't build without
  re-vendoring** egui-winit + smithay (both gitignored) — same as cider today.
- **tree-sitter version is a single workspace-wide pin** shared by `cauldron-editor` (highlight) and
  `cauldron-lint` (PoT). The design passes disagree on the number (0.24/0.25 vs ~0.22) — **the Gate-A ABI
  check resolves it**; bump the core and all three grammar crates as one set. **The bounded-reparse API
  hangs on this pin:** `Parser::set_timeout_micros` is deprecated as of tree-sitter 0.25 and removed in
  0.26, replaced by `parse_with_options` + a `ParseOptions` progress callback — so the async escape hatch
  (Area 1) must target whichever API the pinned version exposes. Vendor the grammars (matching the repo's
  `vendor/` pattern) if crates.io versions don't ABI-align.
- Toolchain probe (2026-07-11): rustc 1.95.0 stable; **clang-tidy + clangd 22.1.6 present**; rust-analyzer,
  cmake, gcc, make present; **cppcheck NOT installed** (`pacman -S cppcheck` — feature-detected, engine
  degrades gracefully); **bear NOT installed** (unneeded); **no cFS checkout** (spike must clone);
  `gh` authenticated as `camdenconradsms`.

---

## Gate A — editor + LSP feasibility (Phase 0 exit; cheap; blocks Phases 1-3)

> **GATE A GO** if, on this box, a throwaway `cauldron-editor` bench edits a real ~5k-line cFS `.c` within
> its per-keystroke **CPU** budget, **and** clangd — off one `compile_commands.json` from a `prep`-built
> cFS — gives single-TU diagnostics quickly and (after its background index completes) resolves
> cross-module go-to-def. A NO here stops the epic **before** the editor is built for real.

Component gates that must all pass:

- **Editor (CPU budget, headless):** **≤8 ms p99** for the full per-keystroke CPU pipeline (rope splice +
  `tree.edit` + incremental parse + viewport query + dirty-line layout + **explicit egui tessellation**) on
  a real ~5k-line cFS `.c` — this is a proxy that *omits* wgpu upload/present/vblank and the rest of the
  app frame, and it is **not** claimed as end-to-end keystroke→paint (that is measured in Phase 3). **No
  single reparse >33 ms** on a top-of-file block-comment flip (else the async escape hatch + span-shift
  degradation from Area 1 is required). Steady ≤8 ms scrolling a generated 200k-line file with bounded
  memory (only visible galleys/spans built).
- **Position & geometry:** byte↔LSP(utf16)↔byte and byte↔ts-Point(byte-col)↔byte = **0 mismatches** over
  ASCII / BMP / astral (position math); **and** a caret placed via the galley's glyph runs lands correctly
  on a line containing a non-ASCII glyph (geometry — proves we don't assume uniform advance).
- **LSP single-TU:** clangd `initialize` negotiates utf-8 (or utf-16) cleanly; `didOpen` a cFS `.c` yields
  `publishDiagnostics` **within ~10 s** with `cfe.h` resolving; a `->` completion returns an expected
  member; a live hover after a multibyte comment char lands on the right token under **both** encodings;
  killing clangd → reader EOF → respawn → docs re-open; 200 completions in a tight loop keep frame time flat
  (all parsing off the frame thread, `drain()` uses `try_recv` only).
- **LSP whole-project (measured, not assumed):** after `--background-index` completes over cFS,
  cross-module go-to-def (ES→SB→OSAL) resolves — and we **record** index wall-clock, peak clangd RSS, and
  `.cache/clangd/index` size, and confirm the box's RAM/disk headroom. If the index never completes or
  needs more resources than the box has, that is an A2-class failure for the whole-project thesis.

## Gate B — linter complementary value (blocks Phases 4-5; only after Gate A passes)

> **GATE B GO** if `cauldron-lint`'s tree-sitter Power-of-Ten analyzer surfaces **one** finding at a named
> `file:line` on real, locally-built cFS source that (a) the strict cppcheck leg (and clang-format) does not
> flag, and (b) carries a **written one-paragraph rationale the owner (@camdenconradsms) signs off** as
> something a cFS maintainer would plausibly accept an issue for. A NO here ships editor+LSP without the
> NASA layer.

This is deliberately objective — a specific rule, a specific location, a signed rationale — not "≥1 genuine
finding" by eyeball. It requires only a `prep`-built cFS + **one** tree-sitter query (not the SARIF/waiver/
adapter stack). **Honest expectation:** A3 (the whole differentiator is hollow) is *unlikely* to fully fire
— cFS CI runs no clang-tidy and no PoT rules, so almost any PoT/CERT hit is categorically novel to it. Gate
B's real job is therefore **calibration**: which PoT rules produce findings a maintainer would *accept*
(ship those first) vs which are noise on cFS's legitimate idioms (defer/waive). If most rules are noisy but
a defensible R1/R3/R4/R5/R10 finding exists, that is still a GO with a narrowed initial ruleset.

## CLA track (parallel, external, non-blocking)

Start the correct NASA CLA (Framework form for cFE/OSAL/PSP; App form otherwise) early and email it (To
`GSFC-SoftwareRelease@mail.nasa.gov`, cc `cfs-program@lists.nasa.gov`). This gates only the cFS PR-merge
loop. If the CLA is rejected/unworkable for the user (A5), Cauldron still ships as a personal IDE + linter;
only the merge loop is forfeit.

## ABORT criteria (staged)

*Under Gate A (stop before building the editor for real):*
- **A1 — no usable compile DB.** cFS's nested build cannot yield a `compile_commands.json` clangd consumes
  on this box, after trying: symlink the arch DB to root, `.clangd CompilationDatabase`, a single-arch
  target, and a `compdb` merge. Kills "index real cFS live."
- **A2 — clangd = noise, or the index won't complete.** Even with the DB + generated headers, clangd
  produces only unusable macro/header noise on cFS with no config fix, **or** the background index can't
  complete within the box's RAM/disk. The daily-driver thesis fails on the target that matters.
- **A4 — editor can't hit budget even async.** The rope + incremental-parse engine exceeds the CPU budget
  (and >33 ms per reparse) on a 5k-line cFS file even with the worker-thread swap + span-shift degradation.
  (Fallback of last resort: single-file `TextEdit` mode, which forfeits the cFS-scale thesis — reassess
  scope.)

*Under Gate B (stop before building the NASA lint layer):*
- **A3 — no complementary value.** No Power-of-Ten finding on real cFS is both novel to the cppcheck leg
  and owner-signed as maintainer-plausible. Unlikely to fire fully (see Gate B); if it *narrows*
  (some rules noisy, not all), ship the high-confidence **R1/R3/R4/R5/R10** set first and defer
  R2/R6/R7/R9.

*External track (never blocks IDE-done):*
- **A5 — contribution infeasible.** The NASA CLA is rejected/unworkable. Cauldron survives as a personal
  IDE + linter; the cFS PR loop can't close — reassess that track only.

---

## Concrete Phase-0 runbook (Gate A + the Gate-B proof)

**Gate A — editor spike** (`cargo bench -p livewall-cauldron-editor`, headless):
1. Stand up `cauldron-editor` skeleton: `ropey` buffer + `Buffer::apply(Transaction)` (single-caret) +
   persistent tree-sitter-c tree + a minimal `show_viewport` painter.
2. `position.rs` property tests: byte↔LSP↔byte and byte↔ts-Point↔byte over ASCII/BMP/astral = 0 mismatches;
   plus a **galley-geometry** test: a caret on a line with a non-ASCII glyph lands via `pos_from_cursor`,
   not `col*advance`.
3. 200k-line scroll: steady ≤8 ms, bounded memory (only visible galleys/spans built); confirm the
   incremental `max_display_width` keeps horizontal virtual-extent O(1) per frame.
4. **Headline:** open a real ~5k-line cFS `.c`, autorepeat a key at EOF, measure p50/p99 of the full
   **CPU** edit pipeline **with egui tessellation run explicitly**. Gate p99 ≤8 ms. Record what is omitted
   (upload/present/vblank) so Phase 3 can budget it.
5. Worst-case reparse: `/*` at top of the 5k-line file then delete — no single reparse >33 ms; if it blows,
   exercise the span-shift-during-stale-window + worker-reparse degradation and re-measure.
6. tree-sitter ABI/version alignment: core + tree-sitter-c/-cpp/-rust compile together, each
   `HIGHLIGHTS_QUERY` returns non-empty captures, and the pinned version's bounded-parse API
   (`parse_with_options` vs `set_timeout_micros`) is recorded. **This pin is shared with `cauldron-lint`.**
7. Clipboard-under-Rune (in-GUI, the one thing a headless probe can't prove): copy a multi-line selection,
   paste via Ctrl+V and Shift+Insert — both land.

**Gate A — LSP spike** (`cauldron --lsp-check <root> <file>`, mirrors redit's `--check`):
8. clangd framing/handshake; log negotiated `positionEncoding` + clangd `offsetEncoding`.
9. Wire the arch `compile_commands.json` (built in step 12) → `didOpen` a real cFS `.c` → **single-TU**
   `publishDiagnostics` within ~10 s (cfe.h resolves).
10. `->` member completion; multibyte-hover round-trip under both encodings; incremental `didChange`
    coherence (Transaction-derived events); kill/respawn; 200-completion no-frame-block stress.
11. **Whole-project:** let `--background-index` finish over cFS; `definition` on a `CFE_ES_*` call →
    cross-module header Location (ES→SB→OSAL). **Record** index wall-clock, peak clangd RSS, and
    `.cache/clangd/index` size; confirm box headroom.

**cFS build (feeds Gate A step 9/11 and Gate B):**
12. Clone nasa/cFS to `~/src/cFS`, `git submodule update`, run
    `CMAKE_EXPORT_COMPILE_COMMANDS=1 make native_std.prep` (or `SIMULATION=native prep`); record the
    front-end + build-dir name. Locate the **arch** DB (expected `…/native/default_cpu1/`), confirm it
    covers cFE+OSAL+PSP+sample_app **and** carries generated `-I …/inc` flags; wire it to clangd.

**Gate B — linter complementary-value proof** (only after Gate A passes; gates Phases 4-5):
13. Run **one** tree-sitter Power-of-Ten query (R2, R4, or R8 — the tree-sitter differentiators) over cFS.
14. Pick **one** finding at a specific `file:line`; write the one-paragraph rationale; confirm it is **not**
    flagged by the strict cppcheck leg (`cppcheck --std=c99
    --enable=warning,performance,portability,style --suppress=variableScope`) or clang-format-19; owner
    signs off that a cFS maintainer would plausibly accept it. This is the Gate-B GO record.
15. Draft (do **NOT** file) one cFS-house-style issue from that finding.

**CLA track (start early, parallel to everything):**
16. Download + sign the correct NASA CLA (Framework form for cFE/OSAL/PSP); email To
    `GSFC-SoftwareRelease@mail.nasa.gov`, cc `cfs-program@lists.nasa.gov`.

Everything else the first-draft runbook front-loaded is **deferred to its real phase**: clang-tidy
`--export-fixes` YAML ingestion + `FileOffset`→span mapping, cppcheck `--xml` parse + graceful degradation,
merge/dedupe of clang-tidy vs custom call-graph, SARIF 2.1.0 emit + offline schema-validate, and full
waiver semantics are **Phase 4**; multi-cursor/500-caret undo, tab hit-testing, and
incremental-highlight-correctness vs full-reparse baseline are **Phase 1**; rust-analyzer parity is **not a
gate** (Rust is not the PoT target).

---

## Post-spike backlog

- Extract `crates/cauldron-syntax` if a second consumer needs the shared tree-sitter layer (else keep it in
  `cauldron-editor`).
- Wire the worker-thread reparse swap (+ span-shift degradation) only if Gate-A step 5 forces it.
- Import cFS's existing cppcheck suppression list to guarantee zero duplicate noise; add the MISRA addon.
- Seed `assert_macros` for R5 by reading cFS's real defensive-check vocabulary.
- `cauldron-lint` as a GitHub Action against cFS PR branches (endgame of the loop).
- Follow-ons deferred out of Phase 0: IME/AccessKit, soft-wrap toggle, multi-root workspaces, snippet
  completion, DAP debugger.

## Open questions (resolve during the spike)

- Which cFS build front-end does current `main` use (`native_std.prep` → `build-native_std/` vs classic
  `SIMULATION=native prep` → `build/`), and is the arch DB exactly `native/default_cpu1/`? Does one arch DB
  cover all submodules or must they be `compdb`-merged?
- Exact tree-sitter core + grammar version set (0.22 vs 0.24/0.25) — pinned by the Gate-A ABI check, shared
  editor+lint; determines the bounded-parse API for the async escape hatch.
- What actual RAM/disk does a full cFS `--background-index` need, and does the box have the headroom
  (recorded in Gate A step 11)?
- Which cFS canonical "big C file" standardizes the editor benchmark.
- Format-on-save default (suggest off); snippet support (v1 plain-text inserts, `snippetSupport:false`).
- Does the exact clang-tidy 22.1.6 `--export-fixes` YAML schema (nesting under `DiagnosticMessage`, `Notes`)
  match the deserializer — confirm in Phase 4 (not a Phase-0 gate).
- Which CLA form applies given the user's employer status (individual vs corporate), and what's the GSFC
  turnaround (human-latency risk on the external track only).