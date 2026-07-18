# Vendored `egui-winit` 0.29.1 (patched)

Third-party code, vendored so this repository builds standalone.

- **Upstream:** [`egui-winit`](https://github.com/emilk/egui) 0.29.1 by Emil Ernerfeldt and the
  egui contributors.
- **Licence:** `MIT OR Apache-2.0` — unchanged from upstream. The MIT text is in
  [`LICENSE-MIT`](LICENSE-MIT); the Apache-2.0 option is available at
  <https://www.apache.org/licenses/LICENSE-2.0>.
- **Why it is patched:** clipboard handling under the Rune/Wayland compositor. It is wired in
  through `[patch.crates-io]` in the workspace `Cargo.toml`, so every crate that depends on
  `egui-winit` transparently gets this build.

This directory is **not** covered by the repository's own MIT licence grant — it keeps its
upstream licence and copyright.
