//! cauldron-editor — the text engine behind the Cauldron IDE.
//!
//! Three load-bearing pieces (see docs/phase0.md, Area 1):
//! - [`buffer`]: a `ropey::Rope` mutated ONLY through `Buffer::apply(Transaction)` — the single
//!   chokepoint that feeds undo/redo, tree-sitter edits, and LSP `didChange` alike.
//! - [`position`]: the single home of byte ↔ (line,col) ↔ LSP-UTF-16 conversions. Every position
//!   bug in an LSP client is a conversion bug; they all live here, property-tested.
//! - [`syntax`]: persistent per-buffer tree-sitter tree with incremental reparse.
//!
//! Gate A (phase-0) proves this stack holds an ≤8 ms p99 CPU budget (layout + tessellate,
//! headless) per keystroke on a real ~5k-line cFS `.c` file. `benches/keystroke.rs` measures it.

pub mod buffer;
pub mod highlight;
pub mod position;
pub mod selection;
pub mod snippet;
pub mod templates;
pub mod syntax;
pub mod theme;
pub mod view;

pub use buffer::{Buffer, Transaction};
pub use view::EditorView;
