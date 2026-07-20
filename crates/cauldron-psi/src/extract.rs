//! Extract Function for C: lift a run of statements into a new function and call it.
//!
//! The hard part is not the text motion, it is deciding what crosses the boundary. A selection
//! reads some variables that live outside it (those become parameters), writes some that outlive
//! it (one of those can become the return value), and declares some of its own (those move with
//! it). Get that wrong and the extracted code still compiles while computing something different —
//! the worst possible failure for a refactoring, because nothing tells you.
//!
//! So this engine is built to DECLINE. Every shape it cannot prove safe returns a
//! [`ExtractError`] naming the reason, and the caller shows that instead of editing. The refusals
//! are deliberate:
//!
//! * `return` / `break` / `continue` / `goto` inside the selection — control flow that escapes
//!   the extracted body would change meaning at the call site, and faking it needs a status
//!   protocol the user did not ask for.
//! * more than one value needing to escape — C returns one thing; the rest would need out-params,
//!   which changes the call site's shape in ways a v1 should not do silently.
//! * a variable whose declared type we cannot read — an invented parameter type compiles as often
//!   as it does not.
//!
//! Analysis is lexical over the tree-sitter tree, not a real dataflow: identifiers are resolved
//! against declarations visible in the enclosing function. That is sound for the shapes it
//! accepts because anything it cannot resolve makes it refuse.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use tree_sitter::Node;

/// A parameter of the extracted function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    pub name: String,
    /// Declared type, source text (`int`, `char *`).
    pub ty: String,
}

/// The edits that perform one extraction. Both must be applied as ONE transaction: the file does
/// not compile with only half of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractPlan {
    /// Where the new function's text goes (a byte offset, always the start of a line).
    pub insert_at: usize,
    /// The complete new function, including a trailing blank line.
    pub function_text: String,
    /// The selection's snapped span, to be replaced by `call_text`.
    pub replace: Range<usize>,
    /// The call that stands in for the extracted statements.
    pub call_text: String,
    /// Byte range of the new function's NAME inside `function_text`, so the caller can offer an
    /// immediate rename.
    pub name_in_function: Range<usize>,
    pub params: Vec<Param>,
    /// The returned variable, if the extraction produces a value.
    pub returns: Option<Param>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtractError {
    /// The selection is not inside a function body.
    NotInFunction,
    /// The selection does not line up with whole statements.
    NotStatements,
    /// Control flow would escape the new function.
    EscapingControlFlow(&'static str),
    /// More than one value would have to come back out.
    MultipleOutputs(Vec<String>),
    /// A variable's declared type could not be determined.
    UnknownType(String),
    /// The selection is empty, or the file does not parse as C.
    Empty,
    /// A name is declared more than once in this function (shadowing). The scope model is one
    /// flat map, so it cannot tell the two apart — and getting it wrong silently rebinds the
    /// extracted body to the wrong variable, or to a global of the same name.
    Shadowed(String),
    /// The selection (or the function) takes the address of a local. `&x` is how C passes
    /// out-parameters, and a value written only through its address looks exactly like a
    /// read-only use — so passing it by value would silently drop the write.
    AddressTaken(String),
    /// A declarator shape whose type cannot be reproduced faithfully: arrays, function pointers,
    /// anything where the type is not `base` plus stars.
    ComplexDeclarator(String),
    /// The enclosing function did not parse cleanly. Refusing is the only safe answer: every
    /// decision below reads types and names off the tree, and a misparse (C++ in a `.h`, an
    /// unexpanded macro, a syntax error mid-edit) makes all of them fiction. This was found by
    /// running the engine over real headers, where it cheerfully produced
    /// `virtual extracted_helper(void)` returning a struct TAG it had mistaken for a variable.
    Unparseable,
}

impl std::fmt::Display for ExtractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotInFunction => write!(f, "select statements inside a function body"),
            Self::NotStatements => write!(f, "select whole statements, not part of one"),
            Self::EscapingControlFlow(k) => {
                write!(f, "selection contains `{k}` — it would change meaning in a new function")
            }
            Self::MultipleOutputs(v) => {
                write!(f, "{} variables would need to come back out ({})", v.len(), v.join(", "))
            }
            Self::UnknownType(n) => write!(f, "cannot determine the type of `{n}`"),
            Self::Empty => write!(f, "nothing to extract"),
            Self::Shadowed(n) => {
                write!(f, "`{n}` is declared more than once here — rename one before extracting")
            }
            Self::AddressTaken(n) => {
                write!(f, "`&{n}` is taken — the value may be written through the pointer")
            }
            Self::ComplexDeclarator(n) => {
                write!(f, "cannot reproduce the declared type of `{n}` (array or function pointer)")
            }
            Self::Unparseable => write!(f, "this function does not parse as C — cannot extract safely"),
        }
    }
}

/// Plan an extraction of `sel` from `src` into a function called `name`.
pub fn plan(src: &str, sel: Range<usize>, name: &str) -> Result<ExtractPlan, ExtractError> {
    if sel.start >= sel.end {
        return Err(ExtractError::Empty);
    }
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&tree_sitter_c::language()).map_err(|_| ExtractError::Empty)?;
    let tree = parser.parse(src, None).ok_or(ExtractError::Empty)?;
    let root = tree.root_node();

    // The function the selection lives in, and its body.
    let func = enclosing(root, sel.start, "function_definition").ok_or(ExtractError::NotInFunction)?;
    let body = func.child_by_field_name("body").ok_or(ExtractError::NotInFunction)?;
    if sel.end > body.end_byte() {
        return Err(ExtractError::NotInFunction);
    }
    // See ExtractError::Unparseable — nothing below can be trusted if the tree is a recovery.
    if func.has_error() {
        return Err(ExtractError::Unparseable);
    }

    // Snap to whole statements within the INNERMOST block containing the selection, so a run
    // inside a loop or an `if` body extracts as readily as one at the function's top level.
    let block = enclosing(root, sel.start, "compound_statement").unwrap_or(body);
    // A selection that starts inside an inner block and ends outside it used to extract only the
    // inner part, silently dropping the rest of what the user highlighted.
    if sel.end > block.end_byte() {
        return Err(ExtractError::NotStatements);
    }
    let stmts = statements_in(block, &sel);
    if stmts.is_empty() {
        return Err(ExtractError::NotStatements);
    }
    let span = stmts[0].start_byte()..stmts[stmts.len() - 1].end_byte();

    // Escaping control flow makes the extraction unsound.
    for s in &stmts {
        if let Some(kind) = escaping_control_flow(*s) {
            return Err(ExtractError::EscapingControlFlow(kind));
        }
    }

    // Declarations visible in the function: parameters, then every local declaration, each with
    // the byte at which it comes into scope.
    let mut scope: BTreeMap<String, (String, usize)> = BTreeMap::new();
    let mut duplicates: BTreeSet<String> = BTreeSet::new();
    let mut complex: BTreeSet<String> = BTreeSet::new();
    collect_params(func, src, &mut scope, &mut duplicates, &mut complex);
    collect_locals(body, src, &mut scope, &mut duplicates, &mut complex);
    // Shadowing anywhere in the function makes the flat scope map unreliable for EVERY name in
    // it (a later declaration overwrote an earlier one's type and position), so this refuses on
    // the function, not just on the shadowed name reaching the selection.
    if let Some(n) = duplicates.iter().next() {
        return Err(ExtractError::Shadowed(n.clone()));
    }
    // `&x` on anything declared in this function: the value may be written through the pointer,
    // and a read-only `&x` (printf("%p", &x)) is indistinguishable from a writing one, so both
    // are refused rather than one silently passed by value.
    if let Some(n) = address_taken_local(body, src, &scope) {
        return Err(ExtractError::AddressTaken(n));
    }

    // Identifiers the selection mentions, and which of them it assigns to. `used` keeps SOURCE
    // ORDER: a parameter list that reads in the order the body mentions them is what a human
    // would have written, and alphabetical order looks arbitrary at the call site.
    let mut used: Vec<String> = Vec::new();
    let mut assigned: BTreeSet<String> = BTreeSet::new();
    for s in &stmts {
        walk_idents(*s, src, &mut used, &mut assigned);
    }
    // Names DECLARED inside the selection move with it — they are not inputs.
    let mut declared_inside: BTreeSet<String> = BTreeSet::new();
    for s in &stmts {
        collect_declared_names(*s, src, &mut declared_inside);
    }

    // Parameters: mentioned inside, declared outside (and before) the selection.
    let mut params: Vec<Param> = Vec::new();
    for n in &used {
        if declared_inside.contains(n) {
            continue;
        }
        let Some((ty, at)) = scope.get(n) else {
            continue; // a global, a function, a macro — no parameter needed
        };
        if *at >= span.start {
            continue; // declared after the selection: cannot be an input
        }
        if ty.is_empty() {
            return Err(ExtractError::UnknownType(n.clone()));
        }
        if complex.contains(n) {
            return Err(ExtractError::ComplexDeclarator(n.clone()));
        }
        params.push(Param { name: n.clone(), ty: ty.clone() });
    }

    // Outputs: anything the selection writes (or declares) that the rest of the function still
    // reads afterwards.
    let after = span.end..body.end_byte();
    let mut live_after: BTreeSet<String> = BTreeSet::new();
    collect_idents_in_range(body, src, &after, &mut live_after);
    let mut outputs: Vec<Param> = Vec::new();
    for n in declared_inside.iter().chain(assigned.iter()) {
        if !live_after.contains(n) {
            continue;
        }
        if outputs.iter().any(|p| p.name == *n) {
            continue;
        }
        let Some((ty, _)) = scope.get(n) else { continue };
        if ty.is_empty() {
            return Err(ExtractError::UnknownType(n.clone()));
        }
        if complex.contains(n) {
            return Err(ExtractError::ComplexDeclarator(n.clone()));
        }
        // A variable that is BOTH an input and written is fine only if it is the single output;
        // otherwise it would need an out-param.
        outputs.push(Param { name: n.clone(), ty: ty.clone() });
    }
    if outputs.len() > 1 {
        return Err(ExtractError::MultipleOutputs(
            outputs.into_iter().map(|p| p.name).collect(),
        ));
    }
    let returns = outputs.into_iter().next();
    // A returned variable that was also an input stays a parameter (the new function updates and
    // returns it); one declared inside must NOT be a parameter.
    if let Some(r) = &returns {
        params.retain(|p| p.name != r.name || !declared_inside.contains(&r.name));
    }

    let indent = line_indent(src, span.start);
    // Match the file's line endings. `str::lines()` eats `\r`, so a CRLF file was getting
    // LF-only generated text spliced into it — mixed endings that git and every diff tool notice.
    let nl = if src.contains("\r\n") { "\r\n" } else { "\n" };
    let ret_ty = returns.as_ref().map_or("void", |r| r.ty.as_str());
    let sig_params = match params.is_empty() {
        true => "void".to_string(),
        false => params
            .iter()
            .map(|p| format!("{} {}", p.ty, p.name))
            .collect::<Vec<_>>()
            .join(", "),
    };

    // The body: the selected text, re-indented one level in from the new function's own column.
    let selected = &src[span.clone()];
    let body_text = reindent(selected, &indent, "    ", nl);
    let mut function_text = String::new();
    let name_start;
    function_text.push_str(&format!("{ret_ty} "));
    name_start = function_text.len();
    function_text.push_str(name);
    function_text.push_str(&format!("({sig_params}){nl}{{{nl}"));
    // A returned local is DECLARED inside the moved statements already; one that came in as a
    // parameter is not re-declared.
    function_text.push_str(&body_text);
    if !function_text.ends_with('\n') {
        function_text.push_str(nl);
    }
    if let Some(r) = &returns {
        function_text.push_str(&format!("    return {};{nl}", r.name));
    }
    function_text.push_str(&format!("}}{nl}{nl}"));

    let args = params.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ");
    let call = match &returns {
        // A local declared inside the selection needs its declaration at the call site.
        Some(r) if declared_inside.contains(&r.name) => {
            format!("{} {} = {name}({args});", r.ty, r.name)
        }
        Some(r) => format!("{} = {name}({args});", r.name),
        None => format!("{name}({args});"),
    };

    Ok(ExtractPlan {
        // Above the enclosing function, so the callee is declared before use with no prototype.
        insert_at: line_start(src, func.start_byte()),
        function_text,
        replace: span,
        call_text: call,
        name_in_function: name_start..name_start + name.len(),
        params,
        returns,
    })
}

/// Nearest ancestor of `byte` with the given kind.
fn enclosing<'t>(root: Node<'t>, byte: usize, kind: &str) -> Option<Node<'t>> {
    let mut cur = root.named_descendant_for_byte_range(byte, byte);
    while let Some(n) = cur {
        if n.kind() == kind {
            return Some(n);
        }
        cur = n.parent();
    }
    None
}

/// The top-level statements of `body` fully covered by `sel`.
fn statements_in<'t>(body: Node<'t>, sel: &Range<usize>) -> Vec<Node<'t>> {
    let mut cur = body.walk();
    body.named_children(&mut cur)
        .filter(|n| n.start_byte() >= sel.start && n.end_byte() <= sel.end)
        .collect()
}

/// Does this subtree contain control flow that would escape a new function body?
/// `break`/`continue` are only escaping when they are NOT inside a loop/switch that is itself
/// part of the selection — a self-contained `for` carries its own.
fn escaping_control_flow(n: Node) -> Option<&'static str> {
    fn walk(n: Node, loops: usize, breakables: usize, depth: usize) -> Option<&'static str> {
        if depth > MAX_DEPTH {
            return None;
        }
        let kind = n.kind();
        // `switch` catches `break` but NOT `continue` — a `continue` inside a switch belongs to
        // the enclosing LOOP, so a selection of just the switch does not carry it. Counting them
        // together generated a `continue` with no loop around it: code that does not compile.
        let is_loop = matches!(kind, "for_statement" | "while_statement" | "do_statement");
        let is_breakable = is_loop || kind == "switch_statement";
        match kind {
            "return_statement" => return Some("return"),
            "goto_statement" => return Some("goto"),
            // A label whose `goto` lives outside the selection would be moved away from it.
            "labeled_statement" => return Some("label"),
            "break_statement" if breakables == 0 => return Some("break"),
            "continue_statement" if loops == 0 => return Some("continue"),
            _ => {}
        }
        let mut cur = n.walk();
        for ch in n.named_children(&mut cur) {
            if let Some(k) = walk(
                ch,
                loops + usize::from(is_loop),
                breakables + usize::from(is_breakable),
                depth + 1,
            ) {
                return Some(k);
            }
        }
        None
    }
    walk(n, 0, 0, 0)
}

/// Parameters of `func` into `scope`, in scope from the body's start.
fn collect_params(
    func: Node,
    src: &str,
    scope: &mut BTreeMap<String, (String, usize)>,
    duplicates: &mut BTreeSet<String>,
    complex: &mut BTreeSet<String>,
) {
    let Some(d) = func.child_by_field_name("declarator") else { return };
    let Some(params) = d.child_by_field_name("parameters") else { return };
    let at = func.start_byte();
    let mut cur = params.walk();
    for p in params.named_children(&mut cur) {
        if p.kind() != "parameter_declaration" {
            continue;
        }
        let Some(pd) = p.child_by_field_name("declarator") else { continue };
        if let Some((name, ty)) = decl_name_and_type(p, src) {
            if !faithful_declarator(pd) {
                complex.insert(name.clone());
            }
            if scope.insert(name.clone(), (ty, at)).is_some() {
                duplicates.insert(name);
            }
        }
    }
}

/// The first local whose address is taken anywhere in the function, if any.
fn address_taken_local(
    body: Node,
    src: &str,
    scope: &BTreeMap<String, (String, usize)>,
) -> Option<String> {
    fn walk(
        n: Node,
        src: &str,
        scope: &BTreeMap<String, (String, usize)>,
        depth: usize,
    ) -> Option<String> {
        if depth > MAX_DEPTH {
            return None;
        }
        if n.kind() == "pointer_expression" && src[n.byte_range()].starts_with('&') {
            if let Some(base) = n.named_child(0).and_then(|c| base_identifier(c, src)) {
                if scope.contains_key(&base) {
                    return Some(base);
                }
            }
        }
        let mut cur = n.walk();
        for ch in n.named_children(&mut cur) {
            if let Some(f) = walk(ch, src, scope, depth + 1) {
                return Some(f);
            }
        }
        None
    }
    walk(body, src, scope, 0)
}

/// Every local `declaration` anywhere in `body`, with the byte it becomes visible at.
///
/// The map is FLAT — one entry per name for the whole function. That is only sound because
/// [`plan`] refuses any function where a name is declared twice: with block scoping unmodelled, a
/// later sibling-block `int v` would overwrite an outer `long v`'s type AND its scope-entry byte,
/// which silently changed a parameter's type or dropped it entirely. Rather than model C's scopes,
/// the engine declines; `duplicates` is how it knows to.
fn collect_locals(
    body: Node,
    src: &str,
    scope: &mut BTreeMap<String, (String, usize)>,
    duplicates: &mut BTreeSet<String>,
    complex: &mut BTreeSet<String>,
) {
    fn walk(
        n: Node,
        src: &str,
        scope: &mut BTreeMap<String, (String, usize)>,
        duplicates: &mut BTreeSet<String>,
        complex: &mut BTreeSet<String>,
        depth: usize,
    ) {
        if depth > MAX_DEPTH {
            return;
        }
        if n.kind() == "declaration" {
            let ty = declared_type(n, src);
            let mut cur = n.walk();
            for d in n.children_by_field_name("declarator", &mut cur) {
                if let Some(name) = declarator_name(d, src) {
                    if !faithful_declarator(d) {
                        complex.insert(name.clone());
                    }
                    let full = format!("{ty}{}", pointer_suffix(d));
                    if scope.insert(name.clone(), (full, n.start_byte())).is_some() {
                        duplicates.insert(name);
                    }
                }
            }
        }
        let mut cur = n.walk();
        for ch in n.named_children(&mut cur) {
            walk(ch, src, scope, duplicates, complex, depth + 1);
        }
    }
    walk(body, src, scope, duplicates, complex, 0);
}

/// The full declared type of a `declaration` / `parameter_declaration`, INCLUDING qualifiers.
/// `const char *p` must not come back as `char *`: dropping `const` changes the contract, and the
/// generated function would then fail to compile against a const argument.
fn declared_type(n: Node, src: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut cur = n.walk();
    for ch in n.named_children(&mut cur) {
        match ch.kind() {
            "type_qualifier" => parts.push(node_text(ch, src)),
            _ if Some(ch) == n.child_by_field_name("type") => parts.push(node_text(ch, src)),
            _ => {}
        }
    }
    parts.join(" ")
}

/// Can [`pointer_suffix`] reproduce this declarator's type exactly? Only `base` + stars. An array
/// (`int a[10]`) decays to a pointer when passed, and a function pointer's type cannot be written
/// as a suffix at all — both silently change the parameter's meaning.
fn faithful_declarator(d: Node) -> bool {
    let mut cur = d;
    loop {
        match cur.kind() {
            "identifier" => return true,
            "array_declarator" | "function_declarator" | "parenthesized_declarator" => return false,
            "pointer_declarator" | "init_declarator" | "attributed_declarator" => {
                match cur.child_by_field_name("declarator") {
                    Some(next) => cur = next,
                    None => return false,
                }
            }
            _ => return false,
        }
    }
}

/// Guard on every recursive walk. Ordinary generated C (deeply nested initializers, long
/// else-if chains) can nest thousands deep, and an unbounded walk ABORTS the process on stack
/// overflow rather than panicking — an editor must not die because a file is unusual.
const MAX_DEPTH: usize = 400;

/// Names declared by `declaration` nodes within this subtree.
fn collect_declared_names(n: Node, src: &str, out: &mut BTreeSet<String>) {
    if n.kind() == "declaration" {
        let mut cur = n.walk();
        for d in n.children_by_field_name("declarator", &mut cur) {
            if let Some(name) = declarator_name(d, src) {
                out.insert(name);
            }
        }
    }
    let mut cur = n.walk();
    for ch in n.named_children(&mut cur) {
        collect_declared_names(ch, src, out);
    }
}

/// Identifiers mentioned in the subtree, and separately those written to.
fn walk_idents(n: Node, src: &str, used: &mut Vec<String>, assigned: &mut BTreeSet<String>) {
    match n.kind() {
        "identifier" => {
            // A call's callee is not a variable.
            let is_callee = n
                .parent()
                .is_some_and(|p| p.kind() == "call_expression" && p.child_by_field_name("function") == Some(n));
            if !is_callee {
                let name = node_text(n, src);
                if !used.contains(&name) {
                    used.push(name);
                }
            }
        }
        "assignment_expression" => {
            if let Some(l) = n.child_by_field_name("left") {
                if let Some(name) = base_identifier(l, src) {
                    assigned.insert(name);
                }
            }
        }
        "update_expression" => {
            if let Some(a) = n.child_by_field_name("argument") {
                if let Some(name) = base_identifier(a, src) {
                    assigned.insert(name);
                }
            }
        }
        _ => {}
    }
    let mut cur = n.walk();
    for ch in n.named_children(&mut cur) {
        walk_idents(ch, src, used, assigned);
    }
}

/// Identifiers appearing within `range` anywhere under `n`.
fn collect_idents_in_range(n: Node, src: &str, range: &Range<usize>, out: &mut BTreeSet<String>) {
    if n.end_byte() <= range.start || n.start_byte() >= range.end {
        return;
    }
    if n.kind() == "identifier" && n.start_byte() >= range.start && n.end_byte() <= range.end {
        out.insert(node_text(n, src));
    }
    let mut cur = n.walk();
    for ch in n.named_children(&mut cur) {
        collect_idents_in_range(ch, src, range, out);
    }
}

/// The variable an lvalue ultimately names (`x`, `x[i]`, `*p`, `s.f` -> `x`, `x`, `p`, `s`).
fn base_identifier(n: Node, src: &str) -> Option<String> {
    match n.kind() {
        "identifier" => Some(node_text(n, src)),
        "subscript_expression" | "field_expression" => {
            base_identifier(n.child_by_field_name("argument")?, src)
        }
        "pointer_expression" | "parenthesized_expression" => {
            base_identifier(n.named_child(0)?, src)
        }
        _ => None,
    }
}

fn decl_name_and_type(p: Node, src: &str) -> Option<(String, String)> {
    let ty = p.child_by_field_name("type").map(|t| node_text(t, src))?;
    let d = p.child_by_field_name("declarator")?;
    let name = declarator_name(d, src)?;
    Some((name, format!("{ty}{}", pointer_suffix(d))))
}

fn declarator_name(d: Node, src: &str) -> Option<String> {
    let mut cur = d;
    loop {
        match cur.kind() {
            "identifier" => return Some(node_text(cur, src)),
            "pointer_declarator" | "array_declarator" | "init_declarator" | "function_declarator"
            | "attributed_declarator" => {
                cur = cur.child_by_field_name("declarator")?;
            }
            "parenthesized_declarator" => cur = cur.named_child(0)?,
            _ => return None,
        }
    }
}

/// ` *` for each pointer level a declarator adds, so `char *s` keeps its type.
fn pointer_suffix(d: Node) -> String {
    let mut out = String::new();
    let mut cur = d;
    loop {
        match cur.kind() {
            "pointer_declarator" => {
                out.push_str(" *");
                let Some(next) = cur.child_by_field_name("declarator") else { break };
                cur = next;
            }
            "init_declarator" | "array_declarator" | "attributed_declarator" => {
                let Some(next) = cur.child_by_field_name("declarator") else { break };
                cur = next;
            }
            _ => break,
        }
    }
    out
}

fn node_text(n: Node, src: &str) -> String {
    src[n.byte_range()].split_whitespace().collect::<Vec<_>>().join(" ")
}

fn line_start(src: &str, byte: usize) -> usize {
    src[..byte].rfind('\n').map_or(0, |i| i + 1)
}

fn line_indent(src: &str, byte: usize) -> String {
    let ls = line_start(src, byte);
    src[ls..].chars().take_while(|c| *c == ' ' || *c == '\t').collect()
}

/// Re-indent a block that currently sits at `from` to sit at `to`.
fn reindent(text: &str, from: &str, to: &str, nl: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut continued = false;
    for l in text.split('\n') {
        let l = l.strip_suffix('\r').unwrap_or(l);
        // A backslash-continued line's SUCCESSOR is part of a token (a string literal, a macro
        // body). Re-indenting it changes the value, so continuation lines pass through verbatim.
        let next_continued = l.ends_with('\\');
        if continued {
            out.push(l.to_string());
        } else if let Some(rest) = l.strip_prefix(from) {
            out.push(format!("{to}{rest}"));
        } else if l.trim().is_empty() {
            out.push(String::new());
        } else {
            out.push(format!("{to}{}", l.trim_start()));
        }
        continued = next_continued;
    }
    out.join(nl)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FN: &str = "int run(int n)\n{\n    int total = 0;\n    total = total + n;\n    return total;\n}\n";

    fn span(src: &str, needle: &str) -> Range<usize> {
        let s = src.find(needle).expect("fixture");
        s..s + needle.len()
    }

    // --- refusals found by adversarial review; each one was silently wrong code -------------

    #[test]
    fn refuses_when_a_later_sibling_block_shadows_a_name() {
        // Was: the inner `int v` overwrote the outer entry INCLUDING its scope byte, so `v` was
        // dropped as a parameter and the moved body rebound to the file-scope `v` — different
        // value, compiles clean.
        let src = "int v = 99;\nvoid run(void)\n{\n    int v = 1;\n    use(v);\n    {\n        int v = 2;\n        use(v);\n    }\n}\n";
        let s = src.find("    use(v);").unwrap();
        assert_eq!(
            plan(src, s..s + "    use(v);".len(), "h").unwrap_err(),
            ExtractError::Shadowed("v".into())
        );
    }

    #[test]
    fn refuses_when_shadowing_would_give_the_wrong_type() {
        // Was: `char v` overwrote `long v`, so the parameter and return became char and the
        // arithmetic silently truncated.
        let src = "void run(void)\n{\n    long v = 200;\n    {\n        char v = 'a';\n        use(v);\n    }\n    v = v + 100;\n    print(v);\n}\n";
        let s = src.find("    v = v + 100;").unwrap();
        assert!(matches!(
            plan(src, s..s + "    v = v + 100;".len(), "h"),
            Err(ExtractError::Shadowed(_))
        ));
    }

    #[test]
    fn refuses_when_an_address_is_taken() {
        // Was: `x` looked like a pure read, so it was passed BY VALUE and scanf wrote the
        // callee's copy. This is every out-parameter idiom in C.
        let src = "void run(void)\n{\n    int x = 0;\n    scanf(\"%d\", &x);\n    print(x);\n}\n";
        let s = src.find("    scanf").unwrap();
        let e = src[s..].find('\n').unwrap() + s;
        assert_eq!(plan(src, s..e, "h").unwrap_err(), ExtractError::AddressTaken("x".into()));
    }

    #[test]
    fn refuses_an_array_local_that_would_decay() {
        // Was: `int a[10]` became parameter `int a`, silently changing the type.
        let src = "void run(void)\n{\n    int a[10];\n    use(a);\n}\n";
        let s = src.find("    use(a);").unwrap();
        assert_eq!(
            plan(src, s..s + "    use(a);".len(), "h").unwrap_err(),
            ExtractError::ComplexDeclarator("a".into())
        );
    }

    #[test]
    fn continue_inside_a_switch_still_belongs_to_the_loop() {
        // Was: `switch` was counted as carrying `continue`, so selecting just the switch produced
        // a `continue` with no loop around it — code that does not compile.
        let src = "void run(void)\n{\n    for (int i = 0; i < 3; i++) {\n        switch (i) {\n        case 1:\n            continue;\n        }\n    }\n}\n";
        let needle = "switch (i) {\n        case 1:\n            continue;\n        }";
        let s = src.find(needle).unwrap();
        assert_eq!(
            plan(src, s..s + needle.len(), "h").unwrap_err(),
            ExtractError::EscapingControlFlow("continue")
        );
    }

    #[test]
    fn refuses_a_selection_that_escapes_its_block() {
        // Was: only the inner part was extracted, silently dropping the rest of the highlight.
        let src = "void run(int x)\n{\n    if (x) {\n        a();\n    }\n    b();\n}\n";
        let s = src.find("        a();").unwrap();
        let e = src.find("    b();").unwrap() + "    b();".len();
        assert_eq!(plan(src, s..e, "h").unwrap_err(), ExtractError::NotStatements);
    }

    #[test]
    fn qualifiers_survive_into_the_parameter_type() {
        // Was: `const` was dropped, so the generated function would not compile against a
        // const argument.
        let src = "void run(void)\n{\n    const int k = 1;\n    use(k);\n}\n";
        let s = src.find("    use(k);").unwrap();
        let p = plan(src, s..s + "    use(k);".len(), "h").unwrap();
        assert_eq!(p.params[0].ty, "const int", "{:?}", p.params);
    }

    #[test]
    fn crlf_files_get_crlf_output() {
        // Was: str::lines() ate the \r, splicing LF-only text into a CRLF file.
        let src = "void run(void)\r\n{\r\n    int a = 0;\r\n    use(a);\r\n}\r\n";
        let s = src.find("    use(a);").unwrap();
        let p = plan(src, s..s + "    use(a);".len(), "h").unwrap();
        assert!(p.function_text.contains("\r\n"), "{:?}", p.function_text);
        assert!(!p.function_text.replace("\r\n", "").contains('\n'), "no bare LF: {:?}", p.function_text);
    }

    #[test]
    fn deep_nesting_does_not_blow_the_stack() {
        // Unbounded recursion ABORTS the process rather than panicking; an editor must not die
        // because a generated file is 2000 blocks deep.
        let mut src = String::from("void run(void)\n{\n");
        for _ in 0..2000 {
            src.push_str("    {\n");
        }
        src.push_str("    use(1);\n");
        for _ in 0..2000 {
            src.push_str("    }\n");
        }
        src.push_str("}\n");
        let s = src.find("    use(1);").unwrap();
        let _ = plan(&src, s..s + 11, "h"); // must return, not abort
    }

    #[test]
    fn extracts_a_void_body_with_one_parameter() {
        let src = "void run(int n)\n{\n    int a = n;\n    use(a);\n}\n";
        let p = plan(src, span(src, "    use(a);"), "helper").unwrap();
        assert_eq!(p.params, vec![Param { name: "a".into(), ty: "int".into() }]);
        assert!(p.returns.is_none());
        assert_eq!(p.call_text, "helper(a);");
        assert!(p.function_text.starts_with("void helper(int a)\n{\n"), "{}", p.function_text);
        assert_eq!(&p.function_text[p.name_in_function.clone()], "helper");
    }

    #[test]
    fn a_value_used_afterwards_becomes_the_return() {
        let p = plan(FN, span(FN, "    total = total + n;"), "step").unwrap();
        let r = p.returns.clone().expect("total is read after the selection");
        assert_eq!(r.name, "total");
        assert_eq!(r.ty, "int");
        assert_eq!(p.call_text, "total = step(total, n);");
        assert!(p.function_text.contains("return total;"), "{}", p.function_text);
    }

    #[test]
    fn a_local_declared_inside_is_declared_at_the_call_site() {
        let src = "void run(int n)\n{\n    int a = n * 2;\n    use(a);\n}\n";
        let p = plan(src, span(src, "    int a = n * 2;"), "mk").unwrap();
        assert_eq!(p.returns.clone().unwrap().name, "a");
        assert_eq!(p.call_text, "int a = mk(n);", "the declaration moves to the call");
        assert!(!p.params.iter().any(|x| x.name == "a"), "a is not an input");
    }

    #[test]
    fn pointer_parameters_keep_their_type() {
        let src = "void run(char *s)\n{\n    int n = 0;\n    use(s, n);\n}\n";
        let p = plan(src, span(src, "    use(s, n);"), "h").unwrap();
        let s_param = p.params.iter().find(|x| x.name == "s").expect("s is an input");
        assert_eq!(s_param.ty, "char *", "a pointer parameter must not become a `char`");
    }

    #[test]
    fn refuses_a_selection_containing_return() {
        let err = plan(FN, span(FN, "    return total;"), "x").unwrap_err();
        assert_eq!(err, ExtractError::EscapingControlFlow("return"));
    }

    #[test]
    fn refuses_a_bare_break_but_allows_a_self_contained_loop() {
        let src = "void run(void)\n{\n    while (1) {\n        break;\n    }\n}\n";
        // The whole loop carries its own break — extractable.
        let p = plan(src, span(src, "    while (1) {\n        break;\n    }"), "loop_body");
        assert!(p.is_ok(), "a self-contained loop must be extractable: {p:?}");
        // The bare break alone is not.
        let inner = "        break;";
        let err = plan(src, span(src, inner), "x").unwrap_err();
        assert_eq!(err, ExtractError::EscapingControlFlow("break"));
    }

    #[test]
    fn refuses_when_two_values_must_escape() {
        let src = "void run(void)\n{\n    int a = 0;\n    int b = 0;\n    a = 1;\n    b = 2;\n    use(a, b);\n}\n";
        let sel = span(src, "    a = 1;\n    b = 2;");
        match plan(src, sel, "x").unwrap_err() {
            ExtractError::MultipleOutputs(v) => {
                assert_eq!(v.len(), 2, "{v:?}");
                assert!(v.contains(&"a".to_string()) && v.contains(&"b".to_string()));
            }
            other => panic!("expected MultipleOutputs, got {other:?}"),
        }
    }

    #[test]
    fn refuses_a_selection_outside_any_function() {
        let src = "int g = 1;\nvoid f(void) { }\n";
        assert_eq!(plan(src, span(src, "int g = 1;"), "x").unwrap_err(), ExtractError::NotInFunction);
    }

    #[test]
    fn refuses_a_partial_statement() {
        let src = "void run(void)\n{\n    int a = compute(1, 2);\n}\n";
        // Half of an expression is not a statement run.
        assert_eq!(plan(src, span(src, "compute(1,"), "x").unwrap_err(), ExtractError::NotStatements);
    }

    #[test]
    fn globals_and_callees_are_not_parameters() {
        let src = "int g;\nvoid run(void)\n{\n    int a = 0;\n    g = helper(a);\n}\n";
        let p = plan(src, span(src, "    g = helper(a);"), "x").unwrap();
        let names: Vec<&str> = p.params.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, vec!["a"], "a global and a callee are not inputs: {names:?}");
    }

    #[test]
    fn the_new_function_goes_above_its_caller() {
        let src = "void run(void)\n{\n    int a = 0;\n    use(a);\n}\n";
        let p = plan(src, span(src, "    use(a);"), "h").unwrap();
        assert_eq!(p.insert_at, 0, "declared before use, so no prototype is needed");
    }

    #[test]
    fn body_is_reindented_to_the_new_function() {
        let src = "void run(void)\n{\n    if (1) {\n        work();\n    }\n}\n";
        let p = plan(src, span(src, "    if (1) {\n        work();\n    }"), "h").unwrap();
        assert!(p.function_text.contains("\n    if (1) {\n"), "{}", p.function_text);
        assert!(p.function_text.contains("\n        work();\n"), "{}", p.function_text);
    }
}
