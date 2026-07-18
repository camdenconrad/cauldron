# Layout persistence — design

**Date:** 2026-07-13
**Status:** approved, not yet implemented

## Problem

Cauldron forgets how you left it. Drag the project tree wider, pull the bottom dock taller, zoom
the editor font, resize the window — relaunch, and every one of those is back to a hardcoded
default. The layout you build up over a working session survives exactly as long as the process.

What already works: `state::Session` (`crates/cauldron/src/state.rs:15`) persists open tabs per
split, active tab, carets, pins, and the open/closed booleans for the tree, pins panel, terminal,
and bottom dock — one JSON per project root under `~/.local/share/cauldron/sessions/<hash>.json`,
written on a 20-second tick and on exit. So "what's open" is largely solved.

What does not work, and is the whole of this spec:

| Lost on restart | Why | Where |
|---|---|---|
| Every panel size | egui tracks dragged sizes in `Memory`, but `eframe` is built `default-features = false`, so its `persistence` feature is OFF and `Memory` is never flushed to disk | `Cargo.toml:21` |
| Window size / position | hardcoded `1280x860` | `main.rs:100` |
| Which bottom tab | `Session` stores a lossy `bottom_problems: bool` — Git/Debug/Tests/Checks all collapse to "not Problems" | `state.rs:30` |
| Which right tab | not persisted at all | `main.rs:432` |
| Editor font size (Ctrl+±) | not persisted at all | `main.rs:442` |
| NASA standards mode | not persisted at all | `main.rs:513` |

## Scope decision

Layout is **global**; tabs stay **per-project**. Panel widths and font size shouldn't depend on
which repo you happened to open; open files obviously should. This splits storage across two
files, which is the cost of getting the semantics right.

Explicitly **out of scope**: breakpoints (`main.rs:499`) — persisting those is a separate feature
with its own questions (what happens when the file changed underneath?), not layout.

## Architecture

Three storage layers, each already having a precedent in the codebase.

### 1. egui `Memory` → eframe persistence (new)

Panel geometry is not App state today — the sizes live in egui's `Memory`, re-seeded each frame
from hardcoded `default_width` / `default_height` calls:

- `main.rs:3861` project tree — `.resizable(true).default_width(260.0)`
- `main.rs:3648` bottom dock — `.resizable(true).default_height(260.0)`
- `main.rs:3710` pins panel — `.resizable(true).width_range(160.0..=460.0).default_width(240.0)`

Those `default_*` calls are **already fallbacks**: egui only applies them when it has no
remembered size for that panel id. So the fix is not to capture sizes by hand — it's to stop
throwing `Memory` away.

Changes:

- `Cargo.toml:21` — add `persistence` to eframe's feature list. (Keep `default-features = false`;
  add the one feature explicitly, so the wgpu/wayland/x11 set is untouched.)
- `main.rs` — implement `eframe::App::save(&mut self, storage: &mut dyn eframe::Storage)`. eframe
  serializes egui `Memory` itself under its own key; our `save()` only needs to exist (and will
  also carry the global settings, below).
- `main.rs:104` — set `NativeOptions { persist_window: true, .. }`.

This one change buys every panel size **and** window geometry, with no new fields.

**Risk — window decorations.** The viewport is built `.with_decorations(false)` (`main.rs:98`);
Cauldron draws its own chrome. `persist_window` restores inner size and position through the same
`ViewportBuilder`, so this should compose, but it is the one thing to verify on the real build
rather than assume.

**Risk — format churn.** eframe persists as RON keyed by egui's `Memory` layout, which is
version-sensitive across egui upgrades. A parse failure falls back to defaults; it does not crash.
Acceptable.

### 2. Global `settings.json` (new)

Two values egui has no concept of, so they cannot ride along with `Memory`:

```rust
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Settings {
    #[serde(default = "default_editor_font")]
    pub editor_font: f32,     // main.rs:442, Ctrl+± zoom, clamped 8..40 at main.rs:3463
    #[serde(default)]
    pub standards: Standards, // main.rs:66
}
```

Stored at `~/.local/share/cauldron/settings.json`, alongside the existing `sessions/` dir and
`last-project` pointer.

These **cannot** be persisted by deriving `Serialize` on `App`. `App` (`main.rs:376`) owns LSP and
DAP child processes, cider PTY handles, mpsc channels, `Arc<Mutex<..>>` caches, and egui
`TextureHandle`s — none of which are serializable, and several of which are live OS resources.
This is the same constraint `runconfig.rs` already solved, and we copy its solution: a separate
plain-data DTO written to disk (`StoreOnDisk`, `runconfig.rs:116`), never the live struct.

`Standards` (`main.rs:66`) needs `Serialize + Deserialize + Default` derives added.

`editor_font` must be **re-clamped to 8..=40 on load**, not trusted — a hand-edited or corrupt
settings file must not be able to set a 3000pt font and render the editor unusable, which would
be unrecoverable through the UI itself.

### 3. Per-project `Session` (extend existing)

In `state.rs:15`, replace the lossy bool with the real enum and add the right-panel tab:

```rust
-    /// True = Problems tab, false = Output tab in the dock's right slot.
-    pub bottom_problems: bool,
+    #[serde(default)]
+    pub bottom_tab: BottomTab,   // main.rs:343 — Output|Problems|Git|Usages|Debug|Tests|Checks
+    #[serde(default)]
+    pub right_tab: RightTab,     // main.rs:334 — Pinned|Structure|History|Ai
```

`BottomTab` and `RightTab` need `Serialize + Deserialize + Default` derives. Both are plain
C-like enums; serde emits them as strings, so the on-disk file stays readable.

**Migration.** Dropping `bottom_problems` means existing session files on disk carry a field the
new struct doesn't know, and lack the two it now wants. Serde ignores unknown fields by default,
and `#[serde(default)]` covers the missing ones — so old files load cleanly, landing on
`BottomTab::default()`. The old bool is **not** migrated into the new enum: it distinguishes only
Problems from Output, the information is nearly worthless, and a one-time reset of which bottom
tab was selected is not worth carrying a compatibility shim. Every new field on both structs gets
`#[serde(default)]` for the same reason.

## Data flow

**Load** (boot, `App::new` at `main.rs:519`):
1. eframe restores egui `Memory` (panel sizes) and window geometry before `App::new` runs — no
   code of ours involved.
2. `Settings::load()` → apply `editor_font` (clamped) and `standards` to `App`.
3. Existing `state::load(root)` → `App::restore_session` (`main.rs:1455`), now also restoring
   `bottom_tab` and `right_tab`.

**Save** — all three layers ride the **existing 20-second autosave tick** (`main.rs:3150`), not
`on_exit`. There is an explicit comment at `main.rs:4774` noting `on_exit` does not fire on
SIGTERM; the tick is what actually protects the state. `on_exit` and project-switch keep saving
too, as they do today. eframe's own `save()` runs on its own auto-save interval, which is
independent and fine.

## Testing

Follows the existing test style in `state.rs:145` — real temp dirs, `HOME` redirected so tests
never touch the user's real state.

1. `Session` round-trips `bottom_tab` / `right_tab` through save→load.
2. **A pre-existing session file** — literal old JSON with `bottom_problems` and no `bottom_tab` —
   loads without error and lands on the default tab. This is the migration guarantee; it must be a
   hardcoded old-format string, not a struct, or it tests nothing.
3. `Settings` round-trips.
4. `Settings::load` clamps an out-of-range `editor_font` (e.g. `3000.0` → `40.0`) and survives a
   corrupt/garbage file by returning defaults.

Panel-size and window persistence are eframe's behavior, not ours; they are verified by running
the app (resize → quit → relaunch), not by unit test.

## Fallback defaults (follow-up, not this change)

Once persistence lands, the user drags the layout to taste and Cauldron writes the exact pixel
values to disk. Those real numbers then get baked in as the new hardcoded `default_width` /
`default_height` / `inner_size` fallbacks, so a **fresh** project (no saved state) opens looking
right. This is deliberately a second commit — the values don't exist until the first one ships.

Note the current in-flight layout (the running process at the time of writing) is unrecoverable:
it lives only in that process's egui `Memory` and was never written anywhere. One-time loss.

## Failure posture

Every layer is best-effort, matching `state::save` (`state.rs:116`) and `runconfig::save`: a
missing, unreadable, or corrupt file falls back to defaults and never blocks boot, never panics,
and never surfaces an error dialog. Losing your panel widths is an annoyance; failing to start
the editor is not acceptable.
