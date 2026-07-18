//! Change Signature: rewrite a C function's parameter list and every call site to match.
//!
//! This is the one refactoring no language server gives us — clangd does not implement it, and
//! it is exactly the operation that is miserable by hand in a codebase like cFS, where one
//! function has dozens of callers across dozens of files.
//!
//! PLANNING IS PURE. [`plan`] reads the index plus a text provider and returns [`Plan`] — a set
//! of byte edits and a set of warnings — without touching the filesystem. The app applies the
//! plan (and can show it first), so the risky part is testable in isolation and the same plan
//! can be previewed, applied, or discarded.
//!
//! ## What it refuses to do
//!
//! C has no overloading, so a name plus linkage identifies a function — but several things can
//! still make a mechanical rewrite wrong, and every one of them is reported as a [`Warning`]
//! rather than silently applied:
//!
//! - **Macro-mined call sites** carry offsets into a macro *body*, not a call expression
//!   (`collect::CallSite::args_range` is `None` there). Editing the macro would change every
//!   expansion, so they are listed for manual review.
//! - **Address-taken functions** are stored in function pointers whose *type* encodes the old
//!   signature. Changing the parameters breaks those assignments in ways no call-site rewrite
//!   can fix.
//! - **Arity mismatches** at a call site (fewer arguments than the declaration has parameters)
//!   mean the call went through a macro, a variadic, or a K&R declaration; remapping by position
//!   would scramble it.
//! - **Variadic functions** (`...`) are refused outright: the fixed parameters can move, but the
//!   argument-to-parameter correspondence past the ellipsis is not recoverable from syntax.
//!
//! ## Linkage
//!
//! A `static` function is private to its file, so a same-named `static` elsewhere is a DIFFERENT
//! function ([`docs/psi-design.md`] calls linkage-awareness the most load-bearing property of the
//! index). [`plan`] therefore scopes a static target to its own file and never rewrites calls in
//! other files — the alternative silently corrupts unrelated code.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};

use crate::collect::StubKind;
use crate::index::Index;

/// One position in the NEW parameter list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParamOp {
    /// Carry over the existing parameter at `from` (an index into the ORIGINAL list). Reordering
    /// is expressed by the order of these ops; `text` overrides the parameter's spelling when
    /// the user retyped or renamed it.
    Keep { from: usize, text: Option<String> },
    /// A brand-new parameter. `text` is the declaration (`int flags`); `default_arg` is what to
    /// pass at every existing call site, since those callers cannot know the new value.
    New { text: String, default_arg: String },
}

/// The requested signature change: a function name plus its new parameter list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureChange {
    pub function: String,
    pub params: Vec<ParamOp>,
}

impl SignatureChange {
    /// Highest original-parameter index this change refers to, if any — used to validate the
    /// request against the function's real arity before planning.
    fn max_kept(&self) -> Option<usize> {
        self.params
            .iter()
            .filter_map(|p| match p {
                ParamOp::Keep { from, .. } => Some(*from),
                ParamOp::New { .. } => None,
            })
            .max()
    }
}

/// A single byte-range replacement in one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub range: Range<usize>,
    pub text: String,
}

/// All edits for one file, sorted DESCENDING by start so sequential application never
/// invalidates a range it has not reached yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEdits {
    pub path: PathBuf,
    pub edits: Vec<Edit>,
}

/// Something the user must look at. A plan with warnings is still applicable — the warned sites
/// are simply left untouched — but applying one blind can leave the tree uncompilable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Warning {
    /// A call inside a macro body. `path`/`line` locate the macro definition, not the call.
    MacroCallSite { path: PathBuf, line: usize, macro_name: String },
    /// The function's address is taken, so some function pointer's type still names the old
    /// signature.
    AddressTaken { path: PathBuf },
    /// A call site whose argument count does not match the declared parameter count.
    ArityMismatch { path: PathBuf, offset: usize, found: usize, expected: usize },
    /// A declaration or definition whose parameter list could not be located (K&R form, or a
    /// declarator shape the extractor does not span). Left alone.
    UnspannedDeclaration { path: PathBuf, line: usize },
    /// A file named in the index whose text the provider could not supply.
    UnreadableFile { path: PathBuf },
}

impl Warning {
    /// One-line rendering for the preview panel.
    pub fn message(&self) -> String {
        match self {
            Self::MacroCallSite { path, line, macro_name } => format!(
                "{}:{}: call inside macro `{macro_name}` — rewrite by hand",
                path.display(),
                line + 1
            ),
            Self::AddressTaken { path } => format!(
                "{}: address of this function is taken — function-pointer types still use the old signature",
                path.display()
            ),
            Self::ArityMismatch { path, offset, found, expected } => format!(
                "{} @{offset}: call passes {found} argument(s), signature declares {expected} — skipped",
                path.display()
            ),
            Self::UnspannedDeclaration { path, line } => format!(
                "{}:{}: parameter list not recognized (K&R form?) — left unchanged",
                path.display(),
                line + 1
            ),
            Self::UnreadableFile { path } => format!("{}: could not read", path.display()),
        }
    }
}

/// Why a signature change cannot be planned at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    NotFound(String),
    /// Variadic (`...`) or K&R `()` — positional remapping is not recoverable from syntax.
    UnknownArity(String),
    /// A `Keep { from }` past the end of the real parameter list.
    BadParamIndex { given: usize, arity: usize },
    /// Target names a macro or typedef, not a function.
    NotAFunction(String),
}

impl PlanError {
    pub fn message(&self) -> String {
        match self {
            Self::NotFound(n) => format!("no definition or declaration of `{n}` in the index"),
            Self::UnknownArity(n) => format!(
                "`{n}` is variadic or has an unspecified parameter list — Change Signature cannot map arguments positionally"
            ),
            Self::BadParamIndex { given, arity } => {
                format!("parameter {given} does not exist (the function has {arity})")
            }
            Self::NotAFunction(n) => format!("`{n}` is a macro or typedef, not a function"),
        }
    }
}

/// A planned Change Signature: edits to apply, plus everything the user should check first.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plan {
    pub files: Vec<FileEdits>,
    pub warnings: Vec<Warning>,
    /// Call sites that WILL be rewritten.
    pub call_sites_rewritten: usize,
    /// Declarations and definitions that will be rewritten.
    pub declarations_rewritten: usize,
    /// Index generation this was computed against — apply only while the index still matches,
    /// or the byte offsets refer to text that has since moved.
    pub generation: u64,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.files.iter().all(|f| f.edits.is_empty())
    }

    pub fn files_touched(&self) -> usize {
        self.files.iter().filter(|f| !f.edits.is_empty()).count()
    }
}

/// Plan a signature change. `text_of` supplies current file text (from the dirty buffer when a
/// file is open, otherwise from disk) — planning never reads the filesystem itself.
///
/// Returns `Err` only when the change is impossible in principle; anything merely *risky* comes
/// back as a [`Warning`] on an otherwise-applicable [`Plan`].
pub fn plan(
    index: &Index,
    change: &SignatureChange,
    text_of: impl FnMut(&Path) -> Option<String>,
) -> Result<Plan, PlanError> {
    plan_from(index, change, None, text_of)
}

/// As [`plan`], but anchored at `from_file` — the file the user invoked the refactoring in.
///
/// This matters only for `static` functions, where two files can define DIFFERENT functions with
/// the same name. Without an anchor the first definition in index order wins, which may not be
/// the one under the caret; the refactoring would then silently rewrite the wrong file.
pub fn plan_from(
    index: &Index,
    change: &SignatureChange,
    from_file: Option<&Path>,
    mut text_of: impl FnMut(&Path) -> Option<String>,
) -> Result<Plan, PlanError> {
    let name = change.function.as_str();
    let defs = index.defs_by_name(name);
    let decls = index.decls_by_name(name);
    if defs.is_empty() && decls.is_empty() {
        return Err(PlanError::NotFound(name.to_string()));
    }

    // Anchor on a definition when there is one — a prototype can be K&R while the definition is
    // fully spelled. Macros and typedefs are not functions and must not be rewritten.
    // Prefer a definition in the invoking file: with two same-named statics, that is the only
    // thing distinguishing the one the user meant from an unrelated function elsewhere.
    let anchor_fid = from_file.and_then(|p| index.file_id(p));
    let pick = |want: Option<crate::graph::FileId>| {
        defs.iter()
            .chain(decls.iter())
            .filter(|d| want.is_none_or(|w| d.file == w))
            .find_map(|&d| index.stub(d).map(|s| (d, s)))
    };
    let anchor = anchor_fid
        .and_then(|f| pick(Some(f)))
        .or_else(|| pick(None))
        .ok_or_else(|| PlanError::NotFound(name.to_string()))?;
    if !matches!(anchor.1.kind, StubKind::FnDef | StubKind::FnDecl) {
        return Err(PlanError::NotAFunction(name.to_string()));
    }
    let arity = anchor.1.arity.ok_or_else(|| PlanError::UnknownArity(name.to_string()))? as usize;
    if let Some(max) = change.max_kept() {
        if max >= arity {
            return Err(PlanError::BadParamIndex { given: max, arity });
        }
    }

    // Linkage: a static function is private to its file, so the rewrite is confined there.
    let is_static = anchor.1.is_static;
    let home_file = is_static.then(|| anchor.0.file);

    let mut plan = Plan { generation: index.generation(), ..Plan::default() };
    let mut per_file: HashMap<PathBuf, Vec<Edit>> = HashMap::new();
    let mut texts: HashMap<PathBuf, Option<String>> = HashMap::new();
    let mut text_for = |path: &Path, texts: &mut HashMap<PathBuf, Option<String>>| -> Option<String> {
        texts.entry(path.to_path_buf()).or_insert_with(|| text_of(path)).clone()
    };

    // ---- declarations + definitions -----------------------------------------------------------
    for &dref in defs.iter().chain(decls.iter()) {
        if home_file.is_some_and(|h| h != dref.file) {
            continue; // a same-named static in another file is a different function
        }
        let (Some(path), Some(stub)) = (index.path(dref.file), index.stub(dref)) else { continue };
        let path = path.to_path_buf();
        let (Some(params_range), true) =
            (stub.params_range.clone(), matches!(stub.kind, StubKind::FnDef | StubKind::FnDecl))
        else {
            plan.warnings.push(Warning::UnspannedDeclaration { path, line: stub.name_line });
            continue;
        };
        let Some(text) = text_for(&path, &mut texts) else {
            plan.warnings.push(Warning::UnreadableFile { path });
            continue;
        };
        // A prototype can legitimately disagree with the definition (K&R, or `()`); only rewrite
        // lists whose shape matches what we planned against.
        if stub.param_ranges.len() != arity {
            plan.warnings.push(Warning::UnspannedDeclaration { path, line: stub.name_line });
            continue;
        }
        let old: Vec<&str> =
            stub.param_ranges.iter().map(|r| text.get(r.clone()).unwrap_or("")).collect();
        let new_list = render_params(&change.params, &old);
        per_file
            .entry(path)
            .or_default()
            .push(Edit { range: params_range, text: format!("({new_list})") });
        plan.declarations_rewritten += 1;
    }

    // ---- call sites ---------------------------------------------------------------------------
    // Collect first, rewrite second. A call can NEST inside another call of the same function
    // (`f(f(1, 2), 3)`), and the outer call's args_range strictly CONTAINS the inner one's — two
    // overlapping edits that back-to-front application cannot reconcile. Instead only the
    // outermost call emits an edit, and its rendering splices in the already-rewritten text of
    // any nested calls (see `rewrite_nested`).
    let mut candidates: Vec<(PathBuf, Range<usize>, Vec<Range<usize>>)> = Vec::new();
    for (fid, call) in index.callers_of(name) {
        if home_file.is_some_and(|h| h != *fid) {
            continue;
        }
        let Some(path) = index.path(*fid) else { continue };
        let path = path.to_path_buf();
        if call.mined_from_macro || call.args_range.is_none() {
            let stub = index.facts(*fid).and_then(|f| f.stubs.get(call.caller_stub as usize));
            plan.warnings.push(Warning::MacroCallSite {
                path,
                line: stub.map_or(0, |s| s.name_line),
                macro_name: stub.map_or_else(String::new, |s| s.name.clone()),
            });
            continue;
        }
        // A call whose argument count disagrees with the signature did not come from this
        // declaration in the straightforward way; positional remapping would scramble it.
        if call.arg_ranges.len() != arity {
            plan.warnings.push(Warning::ArityMismatch {
                path,
                offset: call.offset,
                found: call.arg_ranges.len(),
                expected: arity,
            });
            continue;
        }
        let Some(args_range) = call.args_range.clone() else { continue };
        candidates.push((path, args_range, call.arg_ranges.clone()));
    }

    for (path, args_range, arg_ranges) in &candidates {
        // Nested calls are rewritten as part of their enclosing call's text, not on their own.
        let nested_in_another = candidates.iter().any(|(p, outer, _)| {
            p == path
                && outer != args_range
                && outer.start <= args_range.start
                && args_range.end <= outer.end
        });
        if nested_in_another {
            // Still rewritten, just as part of the enclosing call's replacement text.
            plan.call_sites_rewritten += 1;
            continue;
        }
        let Some(text) = text_for(path, &mut texts) else {
            plan.warnings.push(Warning::UnreadableFile { path: path.clone() });
            continue;
        };
        let old: Vec<String> = arg_ranges
            .iter()
            .map(|r| rewrite_nested(&text, r, &change.params, &candidates, path))
            .collect();
        let old_refs: Vec<&str> = old.iter().map(String::as_str).collect();
        let new_args = render_args(&change.params, &old_refs);
        per_file
            .entry(path.clone())
            .or_default()
            .push(Edit { range: args_range.clone(), text: format!("({new_args})") });
        plan.call_sites_rewritten += 1;
    }

    // ---- address-taken safety ------------------------------------------------------------------
    for (fid, _, facts) in index.files() {
        if home_file.is_some_and(|h| h != fid) {
            continue;
        }
        if facts.address_taken.iter().any(|(n, _)| n == name) {
            if let Some(path) = index.path(fid) {
                plan.warnings.push(Warning::AddressTaken { path: path.to_path_buf() });
            }
        }
    }

    // Descending per file (back-to-front application), files in a deterministic order.
    let mut files: Vec<FileEdits> = per_file
        .into_iter()
        .map(|(path, mut edits)| {
            edits.sort_by_key(|e| e.range.start);
            edits.reverse();
            FileEdits { path, edits }
        })
        .collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    plan.files = files;
    Ok(plan)
}

/// Text of the argument at `span`, with any nested call of the same function already rewritten.
///
/// `f(f(1, 2), 3)` with a swap must produce `f(3, f(2, 1))`: the outer edit covers the inner one,
/// so the inner rewrite has to be materialized into the outer's replacement text rather than
/// emitted as its own (overlapping) edit.
fn rewrite_nested(
    text: &str,
    span: &Range<usize>,
    ops: &[ParamOp],
    candidates: &[(PathBuf, Range<usize>, Vec<Range<usize>>)],
    path: &Path,
) -> String {
    let mut out = text.get(span.clone()).unwrap_or("").to_string();
    // Inner calls whose argument list lies strictly within this argument, outermost first.
    let mut inner: Vec<&(PathBuf, Range<usize>, Vec<Range<usize>>)> = candidates
        .iter()
        .filter(|(p, r, _)| p == path && span.start <= r.start && r.end <= span.end && r != span)
        .collect();
    // Only the calls not themselves nested inside another inner call — deeper levels are handled
    // by the recursion below.
    inner.retain(|(_, r, _)| {
        !candidates.iter().any(|(p2, r2, _)| {
            p2 == path && r2 != r && span.start <= r2.start && r2.end <= span.end
                && r2.start <= r.start && r.end <= r2.end
        })
    });
    // Apply back-to-front so earlier offsets stay valid.
    inner.sort_by_key(|(_, r, _)| std::cmp::Reverse(r.start));
    for (_, r, args) in inner {
        let rewritten: Vec<String> = args
            .iter()
            .map(|a| rewrite_nested(text, a, ops, candidates, path))
            .collect();
        let refs: Vec<&str> = rewritten.iter().map(String::as_str).collect();
        let replacement = format!("({})", render_args(ops, &refs));
        let (lo, hi) = (r.start - span.start, r.end - span.start);
        if lo <= hi && hi <= out.len() {
            out.replace_range(lo..hi, &replacement);
        }
    }
    out
}

/// Render the new parameter list body (no enclosing parens). An empty list becomes `void`,
/// which is C's spelling of "takes no arguments" — a bare `()` would instead mean "unspecified".
fn render_params(ops: &[ParamOp], old: &[&str]) -> String {
    if ops.is_empty() {
        return "void".to_string();
    }
    ops.iter()
        .map(|op| match op {
            ParamOp::Keep { from, text } => text
                .clone()
                .unwrap_or_else(|| old.get(*from).copied().unwrap_or("").trim().to_string()),
            ParamOp::New { text, .. } => text.trim().to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Render the new argument list body (no enclosing parens) for one call site.
fn render_args(ops: &[ParamOp], old: &[&str]) -> String {
    ops.iter()
        .map(|op| match op {
            ParamOp::Keep { from, .. } => old.get(*from).copied().unwrap_or("").trim().to_string(),
            ParamOp::New { default_arg, .. } => default_arg.trim().to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The function whose definition encloses `offset` in `path` — what "Change Signature" resolves
/// the caret to. Prefers the innermost stub, so a nested construct cannot shadow the function.
pub fn function_at(index: &Index, path: &Path, offset: usize) -> Option<String> {
    let fid = index.file_id(path)?;
    let facts = index.facts(fid)?;
    // A call site wins over the enclosing function: the caret sitting on `f(1, 2)` inside `u`'s
    // body means "change f", not "change u". Only when the caret is on no call does the
    // enclosing definition answer.
    let on_call = facts
        .calls
        .iter()
        .filter(|c| !c.mined_from_macro)
        .filter(|c| c.args_range.as_ref().is_some_and(|r| (c.offset..r.end).contains(&offset)))
        // Innermost call wins for nested calls.
        .min_by_key(|c| c.args_range.as_ref().map_or(usize::MAX, |r| r.end - c.offset))
        .map(|c| c.callee.clone());
    if on_call.is_some() {
        return on_call;
    }
    facts
        .stubs
        .iter()
        .filter(|s| matches!(s.kind, StubKind::FnDef | StubKind::FnDecl))
        .filter(|s| s.byte_range.contains(&offset))
        .min_by_key(|s| s.byte_range.end - s.byte_range.start)
        .map(|s| s.name.clone())
}

/// The current parameter list of `name`, as source text, for seeding the dialog.
pub fn current_params(index: &Index, name: &str, text: &str, path: &Path) -> Option<Vec<String>> {
    let fid = index.file_id(path)?;
    let facts = index.facts(fid)?;
    let stub = facts
        .stubs
        .iter()
        .find(|s| s.name == name && matches!(s.kind, StubKind::FnDef | StubKind::FnDecl))?;
    Some(
        stub.param_ranges
            .iter()
            .map(|r| text.get(r.clone()).unwrap_or("").trim().to_string())
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::file_facts;
    use crate::invalidate::replace_file_facts;

    /// Build a one-or-more file index from `(path, source)` pairs.
    fn index_of(files: &[(&str, &str)]) -> (Index, HashMap<PathBuf, String>) {
        let mut index = Index::default();
        let mut texts = HashMap::new();
        for (p, src) in files {
            let path = PathBuf::from(p);
            replace_file_facts(&mut index, path.clone(), std::sync::Arc::new(file_facts(src)));
            texts.insert(path, (*src).to_string());
        }
        (index, texts)
    }

    fn keep(from: usize) -> ParamOp {
        ParamOp::Keep { from, text: None }
    }

    /// Apply a plan to the given texts, back-to-front, and return the results.
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

    #[test]
    fn reorder_rewrites_definition_prototype_and_every_call() {
        let hdr = "int add(int a, char *b);\n";
        let src = "\
int add(int a, char *b) { return a; }
void use(void) { add(1, \"x\"); add(2, \"y\"); }
";
        let (index, texts) = index_of(&[("/p/a.h", hdr), ("/p/a.c", src)]);
        let change = SignatureChange {
            function: "add".into(),
            params: vec![keep(1), keep(0)],
        };
        let plan = plan(&index, &change, |p| texts.get(p).cloned()).unwrap();
        assert_eq!(plan.declarations_rewritten, 2, "definition + prototype");
        assert_eq!(plan.call_sites_rewritten, 2);
        let out = apply(&plan, &texts);
        assert_eq!(out[&PathBuf::from("/p/a.h")], "int add(char *b, int a);\n");
        assert!(out[&PathBuf::from("/p/a.c")].contains("int add(char *b, int a)"));
        assert!(out[&PathBuf::from("/p/a.c")].contains("add(\"x\", 1)"));
        assert!(out[&PathBuf::from("/p/a.c")].contains("add(\"y\", 2)"));
    }

    #[test]
    fn cross_file_extern_function_updates_header_definition_and_all_callers() {
        // The shape that actually matters in cFS: prototype in a header, definition in one .c,
        // callers spread across others. A non-static function has external linkage, so every
        // file participates.
        let hdr = "int CFE_Send(int msg, int len);\n";
        let def = "#include \"api.h\"\nint CFE_Send(int msg, int len) { return msg + len; }\n";
        let c1 = "#include \"api.h\"\nvoid a(void) { CFE_Send(1, 2); }\n";
        let c2 = "#include \"api.h\"\nvoid b(void) { CFE_Send(3, 4); CFE_Send(5, 6); }\n";
        let (index, texts) =
            index_of(&[("/p/api.h", hdr), ("/p/api.c", def), ("/p/a.c", c1), ("/p/b.c", c2)]);

        // Swap the parameters and add a third with a default at every existing call.
        let change = SignatureChange {
            function: "CFE_Send".into(),
            params: vec![
                keep(1),
                keep(0),
                ParamOp::New { text: "int flags".into(), default_arg: "0".into() },
            ],
        };
        let plan = plan(&index, &change, |p| texts.get(p).cloned()).unwrap();
        assert_eq!(plan.declarations_rewritten, 2, "header prototype + definition");
        assert_eq!(plan.call_sites_rewritten, 3);
        assert_eq!(plan.files_touched(), 4);
        assert!(plan.warnings.is_empty(), "unexpected warnings: {:?}", plan.warnings);

        let out = apply(&plan, &texts);
        assert_eq!(out[&PathBuf::from("/p/api.h")], "int CFE_Send(int len, int msg, int flags);\n");
        assert!(out[&PathBuf::from("/p/api.c")].contains("int CFE_Send(int len, int msg, int flags)"));
        assert!(out[&PathBuf::from("/p/a.c")].contains("CFE_Send(2, 1, 0)"));
        assert!(out[&PathBuf::from("/p/b.c")].contains("CFE_Send(4, 3, 0)"));
        assert!(out[&PathBuf::from("/p/b.c")].contains("CFE_Send(6, 5, 0)"));
    }

    #[test]
    fn added_parameter_uses_default_argument_at_existing_calls() {
        let src = "\
int f(int a) { return a; }
void u(void) { f(7); }
";
        let (index, texts) = index_of(&[("/p/a.c", src)]);
        let change = SignatureChange {
            function: "f".into(),
            params: vec![
                keep(0),
                ParamOp::New { text: "int flags".into(), default_arg: "0".into() },
            ],
        };
        let plan = plan(&index, &change, |p| texts.get(p).cloned()).unwrap();
        let out = apply(&plan, &texts);
        assert!(out[&PathBuf::from("/p/a.c")].contains("int f(int a, int flags)"));
        assert!(out[&PathBuf::from("/p/a.c")].contains("f(7, 0)"));
    }

    #[test]
    fn removing_every_parameter_yields_void_not_empty_parens() {
        // `f()` in C means "unspecified arguments", NOT "no arguments" — emitting it would
        // silently drop the compiler's arity checking on every future call.
        let src = "int f(int a) { return a; }\nvoid u(void) { f(1); }\n";
        let (index, texts) = index_of(&[("/p/a.c", src)]);
        let change = SignatureChange { function: "f".into(), params: vec![] };
        let plan = plan(&index, &change, |p| texts.get(p).cloned()).unwrap();
        let out = apply(&plan, &texts);
        assert!(out[&PathBuf::from("/p/a.c")].contains("int f(void)"));
        assert!(out[&PathBuf::from("/p/a.c")].contains("f()"));
    }

    #[test]
    fn static_function_never_escapes_its_own_file() {
        // Two files each define a static `helper`. Changing one must not touch the other —
        // they are different functions that happen to share a name.
        let a = "static int helper(int x, int y) { return x; }\nvoid ua(void) { helper(1, 2); }\n";
        let b = "static int helper(int x, int y) { return y; }\nvoid ub(void) { helper(3, 4); }\n";
        let (index, texts) = index_of(&[("/p/a.c", a), ("/p/b.c", b)]);
        let change = SignatureChange { function: "helper".into(), params: vec![keep(1), keep(0)] };
        let plan = plan(&index, &change, |p| texts.get(p).cloned()).unwrap();
        assert_eq!(plan.files_touched(), 1);
        let out = apply(&plan, &texts);
        assert_eq!(out[&PathBuf::from("/p/b.c")], b, "the other file's static is untouched");
        assert!(out[&PathBuf::from("/p/a.c")].contains("helper(int y, int x)"));
    }

    #[test]
    fn anchor_file_picks_the_right_static_of_two_with_the_same_name() {
        // Both files define a DIFFERENT static `helper`. Which one gets refactored must follow
        // the file the user invoked from, not index order.
        let a = "static int helper(int x, int y) { return x; }\nvoid ua(void) { helper(1, 2); }\n";
        let b = "static int helper(int x, int y) { return y; }\nvoid ub(void) { helper(3, 4); }\n";
        let (index, texts) = index_of(&[("/p/a.c", a), ("/p/b.c", b)]);
        let change = SignatureChange { function: "helper".into(), params: vec![keep(1), keep(0)] };

        let from_b =
            plan_from(&index, &change, Some(Path::new("/p/b.c")), |p| texts.get(p).cloned())
                .unwrap();
        assert_eq!(from_b.files.iter().filter(|f| !f.edits.is_empty()).count(), 1);
        let out = apply(&from_b, &texts);
        assert_eq!(out[&PathBuf::from("/p/a.c")], a, "a.c untouched when invoked from b.c");
        assert!(out[&PathBuf::from("/p/b.c")].contains("helper(int y, int x)"));

        let from_a =
            plan_from(&index, &change, Some(Path::new("/p/a.c")), |p| texts.get(p).cloned())
                .unwrap();
        let out = apply(&from_a, &texts);
        assert_eq!(out[&PathBuf::from("/p/b.c")], b, "b.c untouched when invoked from a.c");
        assert!(out[&PathBuf::from("/p/a.c")].contains("helper(int y, int x)"));
    }

    #[test]
    fn macro_call_sites_are_warned_not_rewritten() {
        let src = "\
#define CALL_F() f(1, 2)
int f(int a, int b) { return a; }
void u(void) { CALL_F(); }
";
        let (index, texts) = index_of(&[("/p/a.c", src)]);
        let change = SignatureChange { function: "f".into(), params: vec![keep(1), keep(0)] };
        let plan = plan(&index, &change, |p| texts.get(p).cloned()).unwrap();
        assert!(
            plan.warnings.iter().any(|w| matches!(w, Warning::MacroCallSite { .. })),
            "expected a macro warning, got {:?}",
            plan.warnings
        );
        let out = apply(&plan, &texts);
        // The macro body is untouched — rewriting it would change every expansion.
        assert!(out[&PathBuf::from("/p/a.c")].contains("#define CALL_F() f(1, 2)"));
    }

    #[test]
    fn address_taken_function_warns() {
        let src = "\
int f(int a, int b) { return a; }
int (*fp)(int, int) = &f;
void u(void) { f(1, 2); }
";
        let (index, texts) = index_of(&[("/p/a.c", src)]);
        let change = SignatureChange { function: "f".into(), params: vec![keep(1), keep(0)] };
        let plan = plan(&index, &change, |p| texts.get(p).cloned()).unwrap();
        assert!(
            plan.warnings.iter().any(|w| matches!(w, Warning::AddressTaken { .. })),
            "taking &f leaves a function-pointer type naming the old signature; got {:?}",
            plan.warnings
        );
    }

    #[test]
    fn variadic_function_is_refused() {
        let src = "int logf(const char *fmt, ...);\nvoid u(void) { logf(\"x\", 1); }\n";
        let (index, texts) = index_of(&[("/p/a.c", src)]);
        let change = SignatureChange { function: "logf".into(), params: vec![keep(0)] };
        assert_eq!(
            plan(&index, &change, |p| texts.get(p).cloned()),
            Err(PlanError::UnknownArity("logf".into()))
        );
    }

    #[test]
    fn keeping_a_nonexistent_parameter_is_refused() {
        let src = "int f(int a) { return a; }\n";
        let (index, texts) = index_of(&[("/p/a.c", src)]);
        let change = SignatureChange { function: "f".into(), params: vec![keep(0), keep(5)] };
        assert_eq!(
            plan(&index, &change, |p| texts.get(p).cloned()),
            Err(PlanError::BadParamIndex { given: 5, arity: 1 })
        );
    }

    #[test]
    fn unknown_function_is_refused() {
        let (index, texts) = index_of(&[("/p/a.c", "int f(void) { return 0; }\n")]);
        let change = SignatureChange { function: "nope".into(), params: vec![] };
        assert_eq!(
            plan(&index, &change, |p| texts.get(p).cloned()),
            Err(PlanError::NotFound("nope".into()))
        );
    }

    #[test]
    fn arity_mismatch_at_a_call_is_skipped_with_a_warning() {
        // The call passes one argument to a two-parameter function (it came through a macro or
        // is simply wrong). Remapping positionally would scramble it.
        let src = "\
int f(int a, int b) { return a; }
void u(void) { f(1); f(2, 3); }
";
        let (index, texts) = index_of(&[("/p/a.c", src)]);
        let change = SignatureChange { function: "f".into(), params: vec![keep(1), keep(0)] };
        let plan = plan(&index, &change, |p| texts.get(p).cloned()).unwrap();
        assert_eq!(plan.call_sites_rewritten, 1, "only the well-formed call");
        assert!(plan.warnings.iter().any(|w| matches!(w, Warning::ArityMismatch { .. })));
        let out = apply(&plan, &texts);
        assert!(out[&PathBuf::from("/p/a.c")].contains("f(1);"), "malformed call left alone");
        assert!(out[&PathBuf::from("/p/a.c")].contains("f(3, 2)"));
    }

    #[test]
    fn edits_are_descending_so_sequential_application_is_safe() {
        let src = "\
int f(int a, int b) { return a; }
void u(void) { f(1, 2); f(3, 4); f(5, 6); }
";
        let (index, texts) = index_of(&[("/p/a.c", src)]);
        let change = SignatureChange { function: "f".into(), params: vec![keep(1), keep(0)] };
        let plan = plan(&index, &change, |p| texts.get(p).cloned()).unwrap();
        for fe in &plan.files {
            for w in fe.edits.windows(2) {
                assert!(w[0].range.start > w[1].range.start, "edits must descend");
            }
            // And they must not overlap, or one would eat another's text.
            for w in fe.edits.windows(2) {
                assert!(w[1].range.end <= w[0].range.start, "edits must be disjoint");
            }
        }
    }

    #[test]
    fn nested_call_of_the_same_function_rewrites_both_levels() {
        let src = "\
int f(int a, int b) { return a; }
void u(void) { f(f(1, 2), 3); }
";
        let (index, texts) = index_of(&[("/p/a.c", src)]);
        let change = SignatureChange { function: "f".into(), params: vec![keep(1), keep(0)] };
        let plan = plan(&index, &change, |p| texts.get(p).cloned()).unwrap();
        assert_eq!(plan.call_sites_rewritten, 2);
        let out = apply(&plan, &texts);
        // Inner becomes f(2, 1); outer swaps its two arguments, carrying the rewritten inner.
        assert!(
            out[&PathBuf::from("/p/a.c")].contains("f(3, f(2, 1))"),
            "got {}",
            out[&PathBuf::from("/p/a.c")]
        );
    }

    #[test]
    fn three_level_nesting_rewrites_every_level() {
        let src = "\
int f(int a, int b) { return a; }
void u(void) { f(f(f(1, 2), 3), 4); }
";
        let (index, texts) = index_of(&[("/p/a.c", src)]);
        let change = SignatureChange { function: "f".into(), params: vec![keep(1), keep(0)] };
        let plan = plan(&index, &change, |p| texts.get(p).cloned()).unwrap();
        assert_eq!(plan.call_sites_rewritten, 3);
        let out = apply(&plan, &texts);
        // innermost f(1,2)->f(2,1); middle f(<inner>,3)->f(3,<inner>); outer f(<mid>,4)->f(4,<mid>)
        assert!(
            out[&PathBuf::from("/p/a.c")].contains("f(4, f(3, f(2, 1)))"),
            "got {}",
            out[&PathBuf::from("/p/a.c")]
        );
    }

    #[test]
    fn two_sibling_nested_calls_in_one_outer_call_both_rewrite() {
        let src = "\
int f(int a, int b) { return a; }
void u(void) { f(f(1, 2), f(3, 4)); }
";
        let (index, texts) = index_of(&[("/p/a.c", src)]);
        let change = SignatureChange { function: "f".into(), params: vec![keep(1), keep(0)] };
        let plan = plan(&index, &change, |p| texts.get(p).cloned()).unwrap();
        assert_eq!(plan.call_sites_rewritten, 3);
        let out = apply(&plan, &texts);
        assert!(
            out[&PathBuf::from("/p/a.c")].contains("f(f(4, 3), f(2, 1))"),
            "got {}",
            out[&PathBuf::from("/p/a.c")]
        );
    }

    #[test]
    fn a_nested_call_emits_no_overlapping_edit() {
        // The regression this guards: the outer call's args_range CONTAINS the inner one's, so
        // emitting both produced two overlapping edits and corrupted the file.
        let src = "int f(int a, int b) { return a; }\nvoid u(void) { f(f(1, 2), 3); }\n";
        let (index, texts) = index_of(&[("/p/a.c", src)]);
        let change = SignatureChange { function: "f".into(), params: vec![keep(1), keep(0)] };
        let plan = plan(&index, &change, |p| texts.get(p).cloned()).unwrap();
        for fe in &plan.files {
            for (i, a) in fe.edits.iter().enumerate() {
                for b in &fe.edits[i + 1..] {
                    let disjoint = a.range.end <= b.range.start || b.range.end <= a.range.start;
                    assert!(disjoint, "edits {:?} and {:?} overlap", a.range, b.range);
                }
            }
        }
    }

    #[test]
    fn renaming_a_parameter_changes_the_declaration_but_not_the_arguments() {
        let src = "int f(int a, int b) { return a; }\nvoid u(void) { f(x, y); }\n";
        let (index, texts) = index_of(&[("/p/a.c", src)]);
        let change = SignatureChange {
            function: "f".into(),
            params: vec![
                ParamOp::Keep { from: 0, text: Some("long a".into()) },
                keep(1),
            ],
        };
        let plan = plan(&index, &change, |p| texts.get(p).cloned()).unwrap();
        let out = apply(&plan, &texts);
        assert!(out[&PathBuf::from("/p/a.c")].contains("int f(long a, int b)"));
        // Call arguments are expressions in the CALLER's scope — retyping a parameter must not
        // touch them.
        assert!(out[&PathBuf::from("/p/a.c")].contains("f(x, y)"));
    }

    #[test]
    fn unreadable_file_warns_instead_of_planning_bad_edits() {
        let src = "int f(int a, int b) { return a; }\nvoid u(void) { f(1, 2); }\n";
        let (index, _texts) = index_of(&[("/p/a.c", src)]);
        let change = SignatureChange { function: "f".into(), params: vec![keep(1), keep(0)] };
        let plan = plan(&index, &change, |_| None).unwrap();
        assert!(plan.is_empty());
        assert!(plan.warnings.iter().any(|w| matches!(w, Warning::UnreadableFile { .. })));
    }

    #[test]
    fn function_at_resolves_from_body_and_from_a_call_site() {
        let src = "\
int f(int a, int b) { return a; }
void u(void) { f(1, 2); }
";
        let (index, _) = index_of(&[("/p/a.c", src)]);
        let path = PathBuf::from("/p/a.c");
        let in_body = src.find("return a").unwrap();
        assert_eq!(function_at(&index, &path, in_body).as_deref(), Some("f"));
        // Caret on the call resolves to the callee, so the refactor can start from a use site.
        let at_call = src.find("f(1, 2)").unwrap() + 2;
        assert_eq!(function_at(&index, &path, at_call).as_deref(), Some("f"));
    }
}
