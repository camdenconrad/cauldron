//! cauldron-psi — the PSI-style project-wide semantic index (Phase-0 spike).
//!
//! Design of record: docs/psi-design.md. This crate is headless (no egui) and independently
//! useful: the same index serves the IDE, the NASA linter CLI, and (later) find-usages/rename.
//!
//! Phase-0 scope: [`collect`] extracts one [`collect::FileFacts`] per file (pure function of the
//! file text — ONE explicit-stack tree-sitter walk, never recursion: a Power-of-Ten tool must not
//! itself recurse); [`graph`] builds the linkage-keyed call graph and runs explicit-stack Tarjan
//! for the Rule-1 (no recursion) check, two-tiered:
//! - **Tier 1 (exact):** direct calls + macro-mined calls, name/linkage-resolved. A nontrivial
//!   SCC or self-loop here is a CONFIRMED finding with a witness cycle.
//! - **Tier 2 (sound over-approximation):** every indirect call site gets edges to the whole
//!   arity-filtered address-taken set. SCCs that close only through indirect edges are POSSIBLE
//!   findings naming the indirect edge they depend on.

pub mod chsig;
pub mod collect;
pub mod graph;
pub mod index;
pub mod invalidate;
pub mod project;
pub mod query;
pub mod tu;
