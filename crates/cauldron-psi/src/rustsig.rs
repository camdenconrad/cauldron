//! Change Signature for Rust.
//!
//! # Why this is not an index
//!
//! The C side ([`crate::chsig`]) can key everything on a function's NAME, because in C a name
//! plus its linkage identifies a function. Rust is the opposite: `new`, `len`, `run`, `from`
//! appear hundreds of times across unrelated types and traits, and resolving `x.foo(a)` requires
//! knowing the *type* of `x` — inference no tree-sitter grammar can do. A name-keyed Rust index
//! would confidently rewrite the wrong `new`.
//!
//! So the work is split by what each tool is actually good at:
//!
//! - **rust-analyzer answers *where*.** `textDocument/references` is exact and type-aware; it
//!   already resolves methods, trait impls, and imports. It simply does not implement Change
//!   Signature as a refactoring.
//! - **tree-sitter answers *what spans*.** Given a reference position, this module parses that
//!   file and recovers the parameter list (at a definition) or the argument list (at a call).
//!
//! [`plan`] takes the reference set and produces the same [`Plan`] the C path does, so both
//! languages share the dialog, the preview, and the apply path.
//!
//! # The receiver problem
//!
//! `self` is not an ordinary parameter, and whether it occupies an argument slot depends on how
//! the call is spelled:
//!
//! | call | arguments | maps to |
//! |---|---|---|
//! | `x.m(a, b)` | `a, b` | params 0, 1 — the receiver is not an argument |
//! | `T::m(&x, a, b)` | `&x, a, b` | arg 0 IS the receiver; params start at arg 1 |
//!
//! Getting this wrong silently shifts every argument by one, so [`CallForm`] is recovered per
//! call site and the offset applied in [`arg_base`].
//!
//! # What it will not do
//!
//! `self` is never reordered, removed, or retyped — it is preserved verbatim. Changing a
//! receiver is a different refactoring with different rules (and often changes the trait
//! contract), so the dialog only ever edits the parameters after it.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};

use tree_sitter::{Node, Parser};

use crate::chsig::{render_args, Edit, FileEdits, ParamOp, Plan, PlanError, SignatureChange, Warning};

/// How a call is spelled, which decides whether argument 0 is the receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallForm {
    /// `free(a)` — a plain path callee.
    Free,
    /// `x.m(a)` — receiver is the `.` operand and occupies no argument slot.
    Method,
    /// `T::m(&x, a)` — UFCS; argument 0 is the receiver when the function takes `self`.
    Path,
}

/// A function's signature as spelled in source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustSignature {
    pub name: String,
    pub name_range: Range<usize>,
    /// The `parameters` node INCLUDING its parentheses.
    pub params_range: Range<usize>,
    /// One span per ordinary parameter, EXCLUDING any `self`.
    pub param_ranges: Vec<Range<usize>>,
    /// Span of the `self` parameter (`&self`, `&mut self`, `self`), when present.
    pub self_range: Option<Range<usize>>,
}

impl RustSignature {
    pub fn has_self(&self) -> bool {
        self.self_range.is_some()
    }
}

/// A call site's shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustCall {
    pub form: CallForm,
    /// The `arguments` node INCLUDING its parentheses.
    pub args_range: Range<usize>,
    pub arg_ranges: Vec<Range<usize>>,
}

fn parser() -> Option<Parser> {
    let mut p = Parser::new();
    p.set_language(&tree_sitter_rust::language()).ok()?;
    Some(p)
}

/// First argument index that corresponds to parameter 0.
///
/// Only UFCS on a method consumes an argument slot for the receiver; `x.m(a)` does not, and a
/// free function has no receiver at all.
fn arg_base(has_self: bool, form: CallForm) -> usize {
    usize::from(has_self && form == CallForm::Path)
}

/// Walk every node, depth-first, applying `f`. Explicit stack — the analyzer obeys the same
/// no-unbounded-recursion rule it enforces.
fn walk(root: Node, mut f: impl FnMut(Node) -> bool) {
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if !f(n) {
            continue;
        }
        for i in (0..n.child_count()).rev() {
            if let Some(c) = n.child(i) {
                stack.push(c);
            }
        }
    }
}

/// Parse `params` (a `parameters` node) into self + ordinary parameter spans.
fn split_params(params: Node) -> (Option<Range<usize>>, Vec<Range<usize>>) {
    let mut self_range = None;
    let mut out = Vec::new();
    for i in 0..params.named_child_count() {
        let Some(c) = params.named_child(i) else { continue };
        match c.kind() {
            "self_parameter" => self_range = Some(c.byte_range()),
            "line_comment" | "block_comment" | "attribute_item" => {}
            _ => out.push(c.byte_range()),
        }
    }
    (self_range, out)
}

fn signature_of(node: Node, src: &str) -> Option<RustSignature> {
    let name_node = node.child_by_field_name("name")?;
    let params = node.child_by_field_name("parameters")?;
    let (self_range, param_ranges) = split_params(params);
    Some(RustSignature {
        name: src.get(name_node.byte_range())?.to_string(),
        name_range: name_node.byte_range(),
        params_range: params.byte_range(),
        param_ranges,
        self_range,
    })
}

/// One parsed file, so a plan over N references in the same file parses it ONCE rather than N
/// times. Re-parsing per reference turned a large refactor into minutes of CPU.
pub struct ParsedFile {
    tree: tree_sitter::Tree,
}

impl ParsedFile {
    pub fn new(src: &str) -> Option<Self> {
        let mut p = parser()?;
        Some(Self { tree: p.parse(src, None)? })
    }

    /// See [`signature_at_name`].
    pub fn signature_at_name(&self, src: &str, offset: usize) -> Option<RustSignature> {
        signature_at_name_in(self.tree.root_node(), src, offset)
    }

    /// See [`call_at_name`].
    pub fn call_at_name(&self, offset: usize) -> Option<RustCall> {
        call_at_name_in(self.tree.root_node(), offset)
    }

    /// See [`all_calls_named`].
    pub fn calls_named(&self, src: &str, name: &str) -> Vec<RustCall> {
        calls_named_in(self.tree.root_node(), src, name)
    }
}

fn signature_at_name_in(root: Node, src: &str, offset: usize) -> Option<RustSignature> {
    let mut found = None;
    walk(root, |n| {
        if found.is_some() || !n.byte_range().contains(&offset) {
            return false;
        }
        if n.kind() == "function_item" || n.kind() == "function_signature_item" {
            if let Some(sig) = signature_of(n, src) {
                if sig.name_range.contains(&offset) {
                    found = Some(sig);
                    return false;
                }
            }
        }
        true
    });
    found
}

fn call_at_name_in(root: Node, offset: usize) -> Option<RustCall> {
    let mut found = None;
    walk(root, |n| {
        if found.is_some() || !n.byte_range().contains(&offset) {
            return false;
        }
        if n.kind() == "call_expression" {
            if let (Some(func), Some(args)) =
                (n.child_by_field_name("function"), n.child_by_field_name("arguments"))
            {
                if let Some((form, name_range)) = call_form(func) {
                    if name_range.contains(&offset) {
                        found = Some(RustCall {
                            form,
                            args_range: args.byte_range(),
                            arg_ranges: arg_spans(args),
                        });
                        return false;
                    }
                }
            }
        }
        true
    });
    found
}

fn calls_named_in(root: Node, src: &str, name: &str) -> Vec<RustCall> {
    let mut out = Vec::new();
    walk(root, |n| {
        if n.kind() == "call_expression" {
            if let (Some(func), Some(args)) =
                (n.child_by_field_name("function"), n.child_by_field_name("arguments"))
            {
                if let Some((form, name_range)) = call_form(func) {
                    if src.get(name_range).is_some_and(|t| t == name) {
                        out.push(RustCall {
                            form,
                            args_range: args.byte_range(),
                            arg_ranges: arg_spans(args),
                        });
                    }
                }
            }
        }
        true
    });
    out
}

/// The signature of the function whose NAME token contains `offset`.
///
/// This is the definition-side test for a reference: rust-analyzer reports a declaration by the
/// position of its name, and both `fn` items and trait method signatures answer here — which is
/// what makes a trait method and all of its impls rewrite together.
pub fn signature_at_name(src: &str, offset: usize) -> Option<RustSignature> {
    let mut p = parser()?;
    let tree = p.parse(src, None)?;
    signature_at_name_in(tree.root_node(), src, offset)
}

/// The signature of the innermost function ENCLOSING `offset` — used to seed the dialog from a
/// caret anywhere in the body, not just on the name.
pub fn enclosing_signature(src: &str, offset: usize) -> Option<RustSignature> {
    let mut p = parser()?;
    let tree = p.parse(src, None)?;
    let mut best: Option<(usize, RustSignature)> = None;
    walk(tree.root_node(), |n| {
        if !n.byte_range().contains(&offset) {
            return false;
        }
        if n.kind() == "function_item" || n.kind() == "function_signature_item" {
            if let Some(sig) = signature_of(n, src) {
                let span = n.byte_range().end - n.byte_range().start;
                if best.as_ref().is_none_or(|(b, _)| span < *b) {
                    best = Some((span, sig));
                }
            }
        }
        true
    });
    best.map(|(_, s)| s)
}

/// Classify the callee identifier of a `call_expression`, or `None` if `func` is not one of the
/// shapes we can rewrite (a closure call, an arbitrary expression callee, …).
fn call_form(func: Node) -> Option<(CallForm, Range<usize>)> {
    match func.kind() {
        "identifier" => Some((CallForm::Free, func.byte_range())),
        // `x.m(...)` — the receiver is the `.` operand.
        "field_expression" => {
            let f = func.child_by_field_name("field")?;
            Some((CallForm::Method, f.byte_range()))
        }
        // `T::m(...)`, `Self::m(...)`, `crate::a::b(...)`.
        "scoped_identifier" => {
            let n = func.child_by_field_name("name")?;
            Some((CallForm::Path, n.byte_range()))
        }
        // Turbofish: `foo::<T>(...)` wraps the real callee.
        "generic_function" => {
            let inner = func.child_by_field_name("function")?;
            call_form(inner).map(|(form, r)| {
                // A turbofish on a plain identifier is still a free call; on a scoped path it is
                // still a path call. Preserve whichever the inner callee was.
                (form, r)
            })
        }
        _ => None,
    }
}

/// The call whose CALLEE NAME token contains `offset`.
///
/// Anchoring on the callee name (not the whole call expression) is what makes nested calls
/// resolve to the right one: in `f(g(1), 2)`, an offset on `g` finds `g`'s call, not `f`'s.
pub fn call_at_name(src: &str, offset: usize) -> Option<RustCall> {
    let mut p = parser()?;
    let tree = p.parse(src, None)?;
    call_at_name_in(tree.root_node(), offset)
}

fn arg_spans(args: Node) -> Vec<Range<usize>> {
    (0..args.named_child_count())
        .filter_map(|i| args.named_child(i))
        .filter(|c| !matches!(c.kind(), "line_comment" | "block_comment"))
        .map(|c| c.byte_range())
        .collect()
}

/// Every call of the same callee name nested anywhere inside `src`, used to detect the
/// containing/contained overlap that nested calls produce.
fn all_calls_named(src: &str, name: &str) -> Vec<RustCall> {
    let Some(p) = ParsedFile::new(src) else { return Vec::new() };
    p.calls_named(src, name)
}

/// Render a Rust parameter list body (no parens), preserving `self` at the front.
///
/// Unlike C, an empty list is `()` — Rust has no `void` spelling, and `()` genuinely means
/// "no parameters".
fn render_rust_params(ops: &[ParamOp], old: &[&str], self_text: Option<&str>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(s) = self_text {
        parts.push(s.trim().to_string());
    }
    for op in ops {
        parts.push(match op {
            ParamOp::Keep { from, text } => text
                .clone()
                .unwrap_or_else(|| old.get(*from).copied().unwrap_or("").trim().to_string()),
            ParamOp::New { text, .. } => text.trim().to_string(),
        });
    }
    parts.join(", ")
}

/// A reference to the target function, as reported by rust-analyzer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub path: PathBuf,
    /// Byte offset of the reference's NAME token in the current text of `path`.
    pub offset: usize,
}

/// Plan a Rust signature change over the reference set rust-analyzer reported.
///
/// `references` must include the declaration (`includeDeclaration: true`) and, for a trait
/// method, r-a additionally reports every impl — which is what makes the trait and its
/// implementors move together.
///
/// Returns `Err` only when the change is impossible in principle; risky-but-doable situations
/// come back as [`Warning`]s on an applicable plan.
pub fn plan(
    references: &[Reference],
    change: &SignatureChange,
    mut text_of: impl FnMut(&Path) -> Option<String>,
) -> Result<Plan, PlanError> {
    if references.is_empty() {
        return Err(PlanError::NotFound(change.function.clone()));
    }
    let name = change.function.as_str();
    let mut texts: HashMap<PathBuf, String> = HashMap::new();
    let mut plan = Plan::default();

    // Group by file so each file is read and parsed once.
    let mut by_file: HashMap<PathBuf, Vec<usize>> = HashMap::new();
    for r in references {
        by_file.entry(r.path.clone()).or_default().push(r.offset);
    }

    // Pass 1: find the signature, so parameter count and `self`-ness are known before any call
    // site is interpreted — the receiver rule depends on it.
    let mut signature: Option<RustSignature> = None;
    for (path, offsets) in &by_file {
        let Some(text) = read(&mut texts, path, &mut text_of) else { continue };
        let Some(parsed) = ParsedFile::new(&text) else { continue };
        for &off in offsets {
            if let Some(sig) = parsed.signature_at_name(&text, off) {
                if sig.name == name {
                    signature = Some(sig);
                    break;
                }
            }
        }
        if signature.is_some() {
            break;
        }
    }
    let Some(signature) = signature else {
        return Err(PlanError::NotFound(name.to_string()));
    };
    let arity = signature.param_ranges.len();
    if let Some(max) = max_kept(change) {
        if max >= arity {
            return Err(PlanError::BadParamIndex { given: max, arity });
        }
    }
    let has_self = signature.has_self();

    let mut per_file: HashMap<PathBuf, Vec<Edit>> = HashMap::new();

    for (path, offsets) in &by_file {
        let Some(text) = read(&mut texts, path, &mut text_of) else {
            plan.warnings.push(Warning::UnreadableFile { path: path.clone() });
            continue;
        };
        // Parse ONCE per file: a hot function can have hundreds of references in one file, and
        // re-parsing per reference is quadratic in file size.
        let Some(parsed) = ParsedFile::new(&text) else {
            plan.warnings.push(Warning::UnreadableFile { path: path.clone() });
            continue;
        };
        // Every call of this name in this file — needed to spot nesting, which produces
        // containing/contained edits that cannot both be applied.
        let calls_here = parsed.calls_named(&text, name);

        for &off in offsets {
            // Definition side: a declaration, an inherent impl, or a trait method + its impls.
            if let Some(sig) = parsed.signature_at_name(&text, off) {
                if sig.name != name {
                    continue;
                }
                if sig.param_ranges.len() != arity {
                    // A different function with the same name reached us (or a shape we
                    // mis-parsed) — rewriting it would be a corruption, not a refactor.
                    plan.warnings
                        .push(Warning::UnresolvedReference { path: path.clone(), offset: off });
                    continue;
                }
                let old: Vec<&str> =
                    sig.param_ranges.iter().map(|r| text.get(r.clone()).unwrap_or("")).collect();
                let self_text =
                    sig.self_range.as_ref().and_then(|r| text.get(r.clone()));
                let body = render_rust_params(&change.params, &old, self_text);
                per_file.entry(path.clone()).or_default().push(Edit {
                    range: sig.params_range.clone(),
                    text: format!("({body})"),
                });
                plan.declarations_rewritten += 1;
                continue;
            }

            // Call side.
            let Some(call) = parsed.call_at_name(off) else {
                // A reference that is neither: `use` item, a re-export, or the function passed
                // by value. Distinguish the value case, which is a real hazard.
                let kind = if is_value_reference(&text, off, name) {
                    Warning::FunctionValueReference { path: path.clone(), offset: off }
                } else {
                    Warning::UnresolvedReference { path: path.clone(), offset: off }
                };
                plan.warnings.push(kind);
                continue;
            };
            let base = arg_base(has_self, call.form);
            if call.arg_ranges.len() != arity + base {
                plan.warnings.push(Warning::ArityMismatch {
                    path: path.clone(),
                    offset: off,
                    found: call.arg_ranges.len().saturating_sub(base),
                    expected: arity,
                });
                continue;
            }
            // Nested inside another call of the same function: rewritten as part of that call's
            // replacement text instead of as its own overlapping edit.
            let nested = calls_here.iter().any(|o| {
                o.args_range != call.args_range
                    && o.args_range.start <= call.args_range.start
                    && call.args_range.end <= o.args_range.end
            });
            if nested {
                plan.call_sites_rewritten += 1;
                continue;
            }
            let new_args =
                rewrite_call(&text, &call, &change.params, has_self, &calls_here, arity);
            per_file.entry(path.clone()).or_default().push(Edit {
                range: call.args_range.clone(),
                text: new_args,
            });
            plan.call_sites_rewritten += 1;
        }
    }

    let mut files: Vec<FileEdits> = per_file
        .into_iter()
        .map(|(path, mut edits)| {
            // A file can yield duplicate edits when r-a reports the same site twice.
            edits.sort_by_key(|e| e.range.start);
            edits.dedup_by(|a, b| a.range == b.range);
            edits.reverse();
            FileEdits { path, edits }
        })
        .collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    plan.files = files;
    Ok(plan)
}

/// Replacement text (with parens) for one call, splicing in any nested calls of the same
/// function so a containing edit carries its contained rewrites.
fn rewrite_call(
    text: &str,
    call: &RustCall,
    ops: &[ParamOp],
    has_self: bool,
    all: &[RustCall],
    arity: usize,
) -> String {
    let base = arg_base(has_self, call.form);
    let receiver: Option<String> = (base == 1)
        .then(|| {
            call.arg_ranges
                .first()
                .and_then(|r| text.get(r.clone()))
                .map(|s| s.trim().to_string())
        })
        .flatten();
    let old: Vec<String> = call.arg_ranges[base.min(call.arg_ranges.len())..]
        .iter()
        .map(|r| rewrite_nested_arg(text, r, ops, has_self, all, arity))
        .collect();
    let refs: Vec<&str> = old.iter().map(String::as_str).collect();
    let body = render_args(ops, &refs);
    match receiver {
        // UFCS: the receiver keeps argument slot 0.
        Some(recv) if body.is_empty() => format!("({recv})"),
        Some(recv) => format!("({recv}, {body})"),
        None => format!("({body})"),
    }
}

/// Argument text with nested calls of the same function already rewritten.
fn rewrite_nested_arg(
    text: &str,
    span: &Range<usize>,
    ops: &[ParamOp],
    has_self: bool,
    all: &[RustCall],
    arity: usize,
) -> String {
    let mut out = text.get(span.clone()).unwrap_or("").to_string();
    let mut inner: Vec<&RustCall> = all
        .iter()
        .filter(|c| {
            span.start <= c.args_range.start
                && c.args_range.end <= span.end
                && c.arg_ranges.len() == arity + arg_base(has_self, c.form)
        })
        .collect();
    // Only the outermost nested calls; deeper ones are handled by the recursion.
    inner.retain(|c| {
        !all.iter().any(|o| {
            o.args_range != c.args_range
                && span.start <= o.args_range.start
                && o.args_range.end <= span.end
                && o.args_range.start <= c.args_range.start
                && c.args_range.end <= o.args_range.end
        })
    });
    inner.sort_by_key(|c| std::cmp::Reverse(c.args_range.start));
    for c in inner {
        let replacement = rewrite_call(text, c, ops, has_self, all, arity);
        let (lo, hi) = (c.args_range.start - span.start, c.args_range.end - span.start);
        if lo <= hi && hi <= out.len() {
            out.replace_range(lo..hi, &replacement);
        }
    }
    out
}

/// Is the identifier at `offset` used as a VALUE (assigned, passed as a callback) rather than
/// called? Heuristic: the name is present but not immediately followed by `(` or `::<`.
fn is_value_reference(text: &str, offset: usize, name: &str) -> bool {
    if text.get(offset..offset + name.len()) != Some(name) {
        return false;
    }
    let rest = text.get(offset + name.len()..).unwrap_or("");
    let next = rest.trim_start();
    !(next.starts_with('(') || next.starts_with("::<"))
}

fn max_kept(change: &SignatureChange) -> Option<usize> {
    change
        .params
        .iter()
        .filter_map(|p| match p {
            ParamOp::Keep { from, .. } => Some(*from),
            ParamOp::New { .. } => None,
        })
        .max()
}

fn read(
    cache: &mut HashMap<PathBuf, String>,
    path: &Path,
    text_of: &mut impl FnMut(&Path) -> Option<String>,
) -> Option<String> {
    if let Some(t) = cache.get(path) {
        return Some(t.clone());
    }
    let t = text_of(path)?;
    cache.insert(path.to_path_buf(), t.clone());
    Some(t)
}

/// The current parameter list of the function whose name token is at `offset`, for seeding the
/// dialog. Excludes `self`.
pub fn current_params(text: &str, offset: usize) -> Option<(String, Vec<String>)> {
    let sig = signature_at_name(text, offset).or_else(|| enclosing_signature(text, offset))?;
    let params = sig
        .param_ranges
        .iter()
        .map(|r| text.get(r.clone()).unwrap_or("").trim().to_string())
        .collect();
    Some((sig.name, params))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keep(from: usize) -> ParamOp {
        ParamOp::Keep { from, text: None }
    }

    fn refs(items: &[(&str, usize)]) -> Vec<Reference> {
        items.iter().map(|(p, o)| Reference { path: PathBuf::from(p), offset: *o }).collect()
    }

    /// Byte offset of the `n`th occurrence of `needle`.
    fn nth(src: &str, needle: &str, n: usize) -> usize {
        src.match_indices(needle).nth(n).expect("occurrence exists").0
    }

    fn apply(plan: &Plan, texts: &HashMap<PathBuf, String>) -> HashMap<PathBuf, String> {
        let mut out = texts.clone();
        for fe in &plan.files {
            let s = out.get_mut(&fe.path).expect("planned file exists");
            for e in &fe.edits {
                s.replace_range(e.range.clone(), &e.text);
            }
        }
        out
    }

    fn texts(items: &[(&str, &str)]) -> HashMap<PathBuf, String> {
        items.iter().map(|(p, s)| (PathBuf::from(*p), (*s).to_string())).collect()
    }

    #[test]
    fn free_function_reorder_across_files() {
        let a = "pub fn send(msg: i32, len: usize) -> i32 { msg }\n";
        let b = "use crate::send;\nfn go() { send(1, 2); send(3, 4); }\n";
        let t = texts(&[("/s/a.rs", a), ("/s/b.rs", b)]);
        let rs = refs(&[
            ("/s/a.rs", nth(a, "send", 0)),
            ("/s/b.rs", nth(b, "send(1", 0)),
            ("/s/b.rs", nth(b, "send(3", 0)),
        ]);
        let change = SignatureChange { function: "send".into(), params: vec![keep(1), keep(0)] };
        let plan = plan(&rs, &change, |p| t.get(p).cloned()).unwrap();
        assert_eq!(plan.declarations_rewritten, 1);
        assert_eq!(plan.call_sites_rewritten, 2);
        let out = apply(&plan, &t);
        assert!(out[&PathBuf::from("/s/a.rs")].contains("fn send(len: usize, msg: i32)"));
        assert!(out[&PathBuf::from("/s/b.rs")].contains("send(2, 1)"));
        assert!(out[&PathBuf::from("/s/b.rs")].contains("send(4, 3)"));
    }

    #[test]
    fn method_call_receiver_is_not_an_argument() {
        let src = "\
struct T;
impl T {
    fn m(&self, a: i32, b: i32) -> i32 { a }
}
fn go(t: T) { t.m(1, 2); }
";
        let t = texts(&[("/s/a.rs", src)]);
        let rs = refs(&[("/s/a.rs", nth(src, "fn m", 0) + 3), ("/s/a.rs", nth(src, "t.m(", 0) + 2)]);
        let change = SignatureChange { function: "m".into(), params: vec![keep(1), keep(0)] };
        let plan = plan(&rs, &change, |p| t.get(p).cloned()).unwrap();
        let out = apply(&plan, &t);
        // `self` is preserved verbatim at the front and never reordered.
        assert!(out[&PathBuf::from("/s/a.rs")].contains("fn m(&self, b: i32, a: i32)"));
        assert!(out[&PathBuf::from("/s/a.rs")].contains("t.m(2, 1)"));
    }

    #[test]
    fn ufcs_call_keeps_the_receiver_in_argument_slot_zero() {
        // THE trap: `T::m(&x, a, b)` passes the receiver as argument 0, so parameters start at
        // argument 1. Treating it like a method call shifts every argument by one.
        let src = "\
struct T;
impl T {
    fn m(&self, a: i32, b: i32) -> i32 { a }
}
fn go(t: T) { T::m(&t, 1, 2); }
";
        let t = texts(&[("/s/a.rs", src)]);
        let rs = refs(&[
            ("/s/a.rs", nth(src, "fn m", 0) + 3),
            ("/s/a.rs", nth(src, "T::m(", 0) + 3),
        ]);
        let change = SignatureChange { function: "m".into(), params: vec![keep(1), keep(0)] };
        let plan = plan(&rs, &change, |p| t.get(p).cloned()).unwrap();
        let out = apply(&plan, &t);
        assert!(
            out[&PathBuf::from("/s/a.rs")].contains("T::m(&t, 2, 1)"),
            "receiver must stay first: {}",
            out[&PathBuf::from("/s/a.rs")]
        );
    }

    #[test]
    fn trait_method_and_every_impl_move_together() {
        // rust-analyzer reports the trait declaration AND each impl as references, so all of
        // them are definition-side rewrites.
        let src = "\
trait Tr {
    fn t(&self, a: i32, b: i32);
}
struct A;
impl Tr for A {
    fn t(&self, a: i32, b: i32) {}
}
fn go(x: A) { x.t(1, 2); }
";
        let t = texts(&[("/s/a.rs", src)]);
        let rs = refs(&[
            ("/s/a.rs", nth(src, "fn t", 0) + 3),
            ("/s/a.rs", nth(src, "fn t", 1) + 3),
            ("/s/a.rs", nth(src, "x.t(", 0) + 2),
        ]);
        let change = SignatureChange { function: "t".into(), params: vec![keep(1), keep(0)] };
        let plan = plan(&rs, &change, |p| t.get(p).cloned()).unwrap();
        assert_eq!(plan.declarations_rewritten, 2, "trait signature + impl");
        let out = apply(&plan, &t);
        let o = &out[&PathBuf::from("/s/a.rs")];
        assert_eq!(o.matches("fn t(&self, b: i32, a: i32)").count(), 2);
        assert!(o.contains("x.t(2, 1)"));
    }

    #[test]
    fn added_parameter_gets_its_default_at_every_call() {
        let src = "fn f(a: i32) -> i32 { a }\nfn go() { f(7); }\n";
        let t = texts(&[("/s/a.rs", src)]);
        let rs = refs(&[("/s/a.rs", nth(src, "fn f", 0) + 3), ("/s/a.rs", nth(src, "f(7)", 0))]);
        let change = SignatureChange {
            function: "f".into(),
            params: vec![
                keep(0),
                ParamOp::New { text: "flags: u8".into(), default_arg: "0".into() },
            ],
        };
        let plan = plan(&rs, &change, |p| t.get(p).cloned()).unwrap();
        let out = apply(&plan, &t);
        assert!(out[&PathBuf::from("/s/a.rs")].contains("fn f(a: i32, flags: u8)"));
        assert!(out[&PathBuf::from("/s/a.rs")].contains("f(7, 0)"));
    }

    #[test]
    fn removing_all_parameters_yields_empty_parens_and_keeps_self() {
        let src = "\
struct T;
impl T {
    fn m(&self, a: i32) {}
}
fn go(t: T) { t.m(1); }
";
        let t = texts(&[("/s/a.rs", src)]);
        let rs = refs(&[("/s/a.rs", nth(src, "fn m", 0) + 3), ("/s/a.rs", nth(src, "t.m(", 0) + 2)]);
        let change = SignatureChange { function: "m".into(), params: vec![] };
        let plan = plan(&rs, &change, |p| t.get(p).cloned()).unwrap();
        let out = apply(&plan, &t);
        // Rust has no `void`: an empty list is `()`, and `self` survives.
        assert!(out[&PathBuf::from("/s/a.rs")].contains("fn m(&self)"));
        assert!(out[&PathBuf::from("/s/a.rs")].contains("t.m()"));
    }

    #[test]
    fn nested_calls_do_not_produce_overlapping_edits() {
        let src = "fn f(a: i32, b: i32) -> i32 { a }\nfn go() { f(f(1, 2), 3); }\n";
        let t = texts(&[("/s/a.rs", src)]);
        let rs = refs(&[
            ("/s/a.rs", nth(src, "fn f", 0) + 3),
            ("/s/a.rs", nth(src, "f(f(1, 2), 3)", 0)),
            ("/s/a.rs", nth(src, "f(1, 2)", 0)),
        ]);
        let change = SignatureChange { function: "f".into(), params: vec![keep(1), keep(0)] };
        let plan = plan(&rs, &change, |p| t.get(p).cloned()).unwrap();
        for fe in &plan.files {
            for (i, a) in fe.edits.iter().enumerate() {
                for b in &fe.edits[i + 1..] {
                    assert!(
                        a.range.end <= b.range.start || b.range.end <= a.range.start,
                        "edits {:?} and {:?} overlap",
                        a.range,
                        b.range
                    );
                }
            }
        }
        let out = apply(&plan, &t);
        assert!(
            out[&PathBuf::from("/s/a.rs")].contains("f(3, f(2, 1))"),
            "got {}",
            out[&PathBuf::from("/s/a.rs")]
        );
    }

    #[test]
    fn turbofish_call_is_rewritten() {
        let src = "fn f(a: i32, b: i32) -> i32 { a }\nfn go() { f::<i32>(1, 2); }\n";
        let t = texts(&[("/s/a.rs", src)]);
        let rs = refs(&[("/s/a.rs", nth(src, "fn f", 0) + 3), ("/s/a.rs", nth(src, "f::<", 0))]);
        let change = SignatureChange { function: "f".into(), params: vec![keep(1), keep(0)] };
        let plan = plan(&rs, &change, |p| t.get(p).cloned()).unwrap();
        assert_eq!(plan.call_sites_rewritten, 1);
        let out = apply(&plan, &t);
        assert!(out[&PathBuf::from("/s/a.rs")].contains("f::<i32>(2, 1)"));
    }

    #[test]
    fn function_used_as_a_value_warns_instead_of_being_rewritten() {
        // `let g = f;` — the function's TYPE still names the old signature; no call-site rewrite
        // can fix that, so it must be surfaced.
        let src = "fn f(a: i32, b: i32) -> i32 { a }\nfn go() { let g = f; }\n";
        let t = texts(&[("/s/a.rs", src)]);
        let rs = refs(&[("/s/a.rs", nth(src, "fn f", 0) + 3), ("/s/a.rs", nth(src, "= f;", 0) + 2)]);
        let change = SignatureChange { function: "f".into(), params: vec![keep(1), keep(0)] };
        let plan = plan(&rs, &change, |p| t.get(p).cloned()).unwrap();
        assert!(
            plan.warnings.iter().any(|w| matches!(w, Warning::FunctionValueReference { .. })),
            "got {:?}",
            plan.warnings
        );
    }

    #[test]
    fn arity_mismatch_at_a_call_is_skipped() {
        let src = "fn f(a: i32, b: i32) -> i32 { a }\nfn go() { f(1); f(2, 3); }\n";
        let t = texts(&[("/s/a.rs", src)]);
        let rs = refs(&[
            ("/s/a.rs", nth(src, "fn f", 0) + 3),
            ("/s/a.rs", nth(src, "f(1)", 0)),
            ("/s/a.rs", nth(src, "f(2, 3)", 0)),
        ]);
        let change = SignatureChange { function: "f".into(), params: vec![keep(1), keep(0)] };
        let plan = plan(&rs, &change, |p| t.get(p).cloned()).unwrap();
        assert_eq!(plan.call_sites_rewritten, 1);
        assert!(plan.warnings.iter().any(|w| matches!(w, Warning::ArityMismatch { .. })));
        let out = apply(&plan, &t);
        assert!(out[&PathBuf::from("/s/a.rs")].contains("f(1);"));
        assert!(out[&PathBuf::from("/s/a.rs")].contains("f(3, 2)"));
    }

    #[test]
    fn keeping_a_nonexistent_parameter_is_refused() {
        let src = "fn f(a: i32) -> i32 { a }\n";
        let t = texts(&[("/s/a.rs", src)]);
        let rs = refs(&[("/s/a.rs", nth(src, "fn f", 0) + 3)]);
        let change = SignatureChange { function: "f".into(), params: vec![keep(0), keep(4)] };
        assert_eq!(
            plan(&rs, &change, |p| t.get(p).cloned()),
            Err(PlanError::BadParamIndex { given: 4, arity: 1 })
        );
    }

    #[test]
    fn no_references_is_refused() {
        let change = SignatureChange { function: "f".into(), params: vec![] };
        assert_eq!(plan(&[], &change, |_| None), Err(PlanError::NotFound("f".into())));
    }

    #[test]
    fn parameters_with_generics_and_lifetimes_keep_their_spelling() {
        let src = "\
fn f<'a, T: Clone>(items: &'a [T], count: usize) -> usize { count }
fn go(v: &[u8]) { f(v, 3); }
";
        let t = texts(&[("/s/a.rs", src)]);
        let rs = refs(&[("/s/a.rs", nth(src, "fn f", 0) + 3), ("/s/a.rs", nth(src, "f(v, 3)", 0))]);
        let change = SignatureChange { function: "f".into(), params: vec![keep(1), keep(0)] };
        let plan = plan(&rs, &change, |p| t.get(p).cloned()).unwrap();
        let out = apply(&plan, &t);
        // Commas inside the generic bounds must not be mistaken for parameter separators.
        assert!(
            out[&PathBuf::from("/s/a.rs")].contains("fn f<'a, T: Clone>(count: usize, items: &'a [T])"),
            "got {}",
            out[&PathBuf::from("/s/a.rs")]
        );
        assert!(out[&PathBuf::from("/s/a.rs")].contains("f(3, v)"));
    }

    #[test]
    fn current_params_seeds_the_dialog_without_self() {
        let src = "struct T;\nimpl T {\n    fn m(&self, a: i32, b: &str) {}\n}\n";
        let (name, params) = current_params(src, nth(src, "fn m", 0) + 3).unwrap();
        assert_eq!(name, "m");
        assert_eq!(params, ["a: i32", "b: &str"]);
    }
}
