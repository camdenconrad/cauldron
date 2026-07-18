//! Per-file fact extraction — the pure half of the index (docs/psi-design.md, "Indexing pipeline").
//!
//! [`file_facts`] is a pure function of the file text: ONE explicit-stack tree-sitter walk (never
//! recursion — a Power-of-Ten tool must not itself recurse) produces the complete [`FileFacts`] a
//! file contributes. Macro replacement lists are opaque `preproc_arg` tokens in tree-sitter-c, so
//! each macro body is re-parsed as its own C fragment and mined for direct calls (one level —
//! macro-calls-macro chains resolve transitively through the graph). All `#if` branches are walked
//! (the sound union over configurations). ERROR-tolerant by design: extract whatever parsed, never
//! bail on a file, and report `error_bytes` as index-health telemetry.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::ops::Range;

use tree_sitter::{Node, Parser};

/// Every tree-sitter-c node-kind string the walk matches on, in ONE place — the audit point for
/// grammar bumps (docs/psi-design.md, risk 3: re-run the fixture suite on any pin change).
mod kind {
    pub const FUNCTION_DEFINITION: &str = "function_definition";
    pub const DECLARATION: &str = "declaration";
    pub const TYPE_DEFINITION: &str = "type_definition";
    pub const PREPROC_FUNCTION_DEF: &str = "preproc_function_def";
    pub const PREPROC_DEF: &str = "preproc_def";
    pub const CALL_EXPRESSION: &str = "call_expression";
    pub const IDENTIFIER: &str = "identifier";
    pub const TYPE_IDENTIFIER: &str = "type_identifier";
    pub const FUNCTION_DECLARATOR: &str = "function_declarator";
    pub const POINTER_DECLARATOR: &str = "pointer_declarator";
    pub const PARENTHESIZED_DECLARATOR: &str = "parenthesized_declarator";
    pub const ARRAY_DECLARATOR: &str = "array_declarator";
    pub const ATTRIBUTED_DECLARATOR: &str = "attributed_declarator";
    pub const INIT_DECLARATOR: &str = "init_declarator";
    pub const STORAGE_CLASS_SPECIFIER: &str = "storage_class_specifier";
    pub const PARAMETER_DECLARATION: &str = "parameter_declaration";
    pub const VARIADIC_PARAMETER: &str = "variadic_parameter";
    pub const COMMENT: &str = "comment";
    pub const INITIALIZER_LIST: &str = "initializer_list";
    pub const INITIALIZER_PAIR: &str = "initializer_pair";
    pub const POINTER_EXPRESSION: &str = "pointer_expression";
    pub const ASSIGNMENT_EXPRESSION: &str = "assignment_expression";
    pub const ARGUMENT_LIST: &str = "argument_list";
    pub const AMPERSAND: &str = "&";
    pub const ELLIPSIS: &str = "...";
}

/// `caller_stub` value for call/indirect sites outside any function body.
pub const TOP_LEVEL: u32 = u32::MAX;

/// Address-taken context bits (merged per name per file).
pub const CTX_INIT_LIST: u8 = 1;
/// Operand of unary `&`.
pub const CTX_ADDR_OF: u8 = 2;
/// Right-hand side of an assignment (or scalar initializer).
pub const CTX_ASSIGN_RHS: u8 = 4;
/// Argument position — passing a callback means external code may call it back.
pub const CTX_CALL_ARG: u8 = 8;

/// What kind of top-level thing a [`Stub`] names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum StubKind {
    FnDef,
    FnDecl,
    MacroFn,
    MacroObj,
    Typedef,
}

/// One top-level named entity. Plain data — name + kind + spans, never a node handle
/// (docs/psi-design.md deliberately skips IntelliJ's stub<->AST dual binding).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stub {
    pub name: String,
    pub kind: StubKind,
    pub is_static: bool,
    pub byte_range: Range<usize>,
    pub name_range: Range<usize>,
    /// 0-based row of the NAME token (tree-sitter `Point` at extraction time) — consumers like
    /// goto-symbol jump straight here without re-reading the file to count newlines. For
    /// overlay-derived facts this is a buffer coordinate, same policy as the byte ranges.
    pub name_line: usize,
    /// Parameter count; `None` when variadic or syntactically unknowable (K&R `()`).
    pub arity: Option<u8>,
}

/// One direct call site: `callee` is a plain identifier at the call position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallSite {
    /// Index into [`FileFacts::stubs`]; [`TOP_LEVEL`] outside any function. Macro-mined calls
    /// point at the macro's stub.
    pub caller_stub: u32,
    pub callee: String,
    pub offset: usize,
    pub mined_from_macro: bool,
}

/// Everything one file contributes to the index — a pure function of the file text.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileFacts {
    pub stubs: Vec<Stub>,
    pub calls: Vec<CallSite>,
    /// `(name, merged ctx bits)` — deduped per file, sorted for determinism.
    pub address_taken: Vec<(String, u8)>,
    /// `(caller_stub or TOP_LEVEL, byte offset, best-effort argument count)`.
    pub indirect_sites: Vec<(u32, usize, Option<u8>)>,
    /// Total bytes covered by (outermost) ERROR nodes — index-health telemetry.
    pub error_bytes: u32,
    /// Hash of the sorted exported surface (stub name/kind/static/arity) — the invalidation key
    /// other files depend on.
    pub interface_hash: u64,
    /// Hash of the sorted call-site + address-taken lists — offsets excluded so body-only edits
    /// that move bytes without changing calls stay free.
    pub body_hash: u64,
}

/// Extract [`FileFacts`] from one file's text. Pure: same text -> identical facts (incl. hashes).
pub fn file_facts(text: &str) -> FileFacts {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_c::language())
        .expect("tree-sitter-c grammar/ABI mismatch");

    let mut stubs: Vec<Stub> = Vec::new();
    let mut calls: Vec<CallSite> = Vec::new();
    let mut taken: HashMap<String, u8> = HashMap::new();
    let mut indirect: Vec<(u32, usize, Option<u8>)> = Vec::new();
    let mut macro_bodies: Vec<(u32, Range<usize>)> = Vec::new();
    let mut error_bytes: u64 = 0;

    if let Some(tree) = parser.parse(text, None) {
        // Byte-driven scopes popped as the pre-order walk moves past their end.
        let mut fn_stack: Vec<(u32, usize)> = Vec::new(); // (FnDef stub idx, end byte)
        let mut err_stack: Vec<usize> = Vec::new(); // ERROR end bytes (nesting guard)

        for_each_preorder(tree.root_node(), &mut |node| {
            let start = node.start_byte();
            while fn_stack.last().is_some_and(|&(_, end)| end <= start) {
                fn_stack.pop();
            }
            while err_stack.last().is_some_and(|&end| end <= start) {
                err_stack.pop();
            }
            if node.is_error() {
                if err_stack.is_empty() {
                    error_bytes += (node.end_byte() - start) as u64;
                }
                err_stack.push(node.end_byte());
            }

            match node.kind() {
                kind::FUNCTION_DEFINITION => {
                    if let Some(d) = node.child_by_field_name("declarator") {
                        let info = declarator_info(d);
                        if let Some(name_node) = info.name {
                            let idx = stubs.len() as u32;
                            stubs.push(Stub {
                                name: text[name_node.byte_range()].to_string(),
                                kind: StubKind::FnDef,
                                is_static: has_static(node, text),
                                byte_range: node.byte_range(),
                                name_range: name_node.byte_range(),
                                name_line: name_node.start_position().row,
                                arity: info.params.and_then(|p| params_arity(p, text)),
                            });
                            fn_stack.push((idx, node.end_byte()));
                        }
                    }
                }
                kind::DECLARATION => {
                    let is_static = has_static(node, text);
                    let mut cur = node.walk();
                    let decls: Vec<Node> =
                        node.children_by_field_name("declarator", &mut cur).collect();
                    for d in decls {
                        let info = declarator_info(d);
                        if !info.is_function {
                            continue; // fn-pointer variables etc. are not declarations of functions
                        }
                        if let Some(name_node) = info.name {
                            stubs.push(Stub {
                                name: text[name_node.byte_range()].to_string(),
                                kind: StubKind::FnDecl,
                                is_static,
                                byte_range: node.byte_range(),
                                name_range: name_node.byte_range(),
                                name_line: name_node.start_position().row,
                                arity: info.params.and_then(|p| params_arity(p, text)),
                            });
                        }
                    }
                }
                kind::TYPE_DEFINITION => {
                    let mut cur = node.walk();
                    let decls: Vec<Node> =
                        node.children_by_field_name("declarator", &mut cur).collect();
                    for d in decls {
                        if let Some(name_node) = declarator_info(d).name {
                            stubs.push(Stub {
                                name: text[name_node.byte_range()].to_string(),
                                kind: StubKind::Typedef,
                                is_static: false,
                                byte_range: node.byte_range(),
                                name_range: name_node.byte_range(),
                                name_line: name_node.start_position().row,
                                arity: None,
                            });
                        }
                    }
                }
                kind::PREPROC_FUNCTION_DEF => {
                    if let Some(name_node) = node.child_by_field_name("name") {
                        let idx = stubs.len() as u32;
                        stubs.push(Stub {
                            name: text[name_node.byte_range()].to_string(),
                            kind: StubKind::MacroFn,
                            is_static: false,
                            byte_range: node.byte_range(),
                            name_range: name_node.byte_range(),
                            name_line: name_node.start_position().row,
                            arity: node
                                .child_by_field_name("parameters")
                                .and_then(macro_params_arity),
                        });
                        if let Some(v) = node.child_by_field_name("value") {
                            macro_bodies.push((idx, v.byte_range()));
                        }
                    }
                }
                kind::PREPROC_DEF => {
                    if let Some(name_node) = node.child_by_field_name("name") {
                        let idx = stubs.len() as u32;
                        stubs.push(Stub {
                            name: text[name_node.byte_range()].to_string(),
                            kind: StubKind::MacroObj,
                            is_static: false,
                            byte_range: node.byte_range(),
                            name_range: name_node.byte_range(),
                            name_line: name_node.start_position().row,
                            arity: None,
                        });
                        if let Some(v) = node.child_by_field_name("value") {
                            macro_bodies.push((idx, v.byte_range()));
                        }
                    }
                }
                kind::CALL_EXPRESSION => {
                    let caller = fn_stack.last().map_or(TOP_LEVEL, |&(i, _)| i);
                    if let Some(f) = node.child_by_field_name("function") {
                        if f.kind() == kind::IDENTIFIER {
                            calls.push(CallSite {
                                caller_stub: caller,
                                callee: text[f.byte_range()].to_string(),
                                offset: node.start_byte(),
                                mined_from_macro: false,
                            });
                        } else {
                            // Indirect: field/pointer/array/paren callee. Best-effort arity for
                            // the Tier-2 filter.
                            let arity =
                                node.child_by_field_name("arguments").and_then(args_arity);
                            indirect.push((caller, node.start_byte(), arity));
                        }
                    }
                }
                kind::IDENTIFIER => {
                    // Address-taken harvest: identifiers in NON-callee escape positions. Callee
                    // identifiers became CallSites above; declarator/field/label names never
                    // match a ctx bit (field names are `field_identifier` nodes in this grammar).
                    let is_callee = node.parent().is_some_and(|p| {
                        p.kind() == kind::CALL_EXPRESSION && field_is(p, "function", node)
                    });
                    if !is_callee {
                        let ctx = ident_ctx(node);
                        if ctx != 0 {
                            *taken.entry(text[node.byte_range()].to_string()).or_insert(0) |= ctx;
                        }
                    }
                }
                _ => {}
            }
        });
    }

    // Macro mining: each replacement list re-parsed as its own C fragment (tolerate ERROR).
    for (stub_idx, value) in macro_bodies {
        mine_macro_body(&mut parser, text, stub_idx, value, &mut calls);
    }

    finish(stubs, calls, taken, indirect, error_bytes)
}

/// Assemble the facts: sort the deduped address-taken set and compute the two invalidation
/// hashes (docs/psi-design.md, "two-hash invalidation").
fn finish(
    stubs: Vec<Stub>,
    calls: Vec<CallSite>,
    taken: HashMap<String, u8>,
    indirect_sites: Vec<(u32, usize, Option<u8>)>,
    error_bytes: u64,
) -> FileFacts {
    let mut address_taken: Vec<(String, u8)> = taken.into_iter().collect();
    address_taken.sort();

    let mut iface: Vec<(&str, u8, bool, Option<u8>)> = stubs
        .iter()
        .map(|s| (s.name.as_str(), s.kind as u8, s.is_static, s.arity))
        .collect();
    iface.sort();
    let mut h = DefaultHasher::new();
    for e in &iface {
        e.hash(&mut h);
    }
    let interface_hash = h.finish();

    let mut body: Vec<(&str, u32, bool)> = calls
        .iter()
        .map(|c| (c.callee.as_str(), c.caller_stub, c.mined_from_macro))
        .collect();
    body.sort();
    let mut h = DefaultHasher::new();
    for e in &body {
        e.hash(&mut h);
    }
    for e in &address_taken {
        e.hash(&mut h);
    }
    let body_hash = h.finish();

    FileFacts {
        stubs,
        calls,
        address_taken,
        indirect_sites,
        error_bytes: error_bytes.min(u32::MAX as u64) as u32,
        interface_hash,
        body_hash,
    }
}

/// Iterative pre-order traversal via TreeCursor — the explicit-stack walk. Nothing here recurses.
fn for_each_preorder<'t>(root: Node<'t>, visit: &mut dyn FnMut(Node<'t>)) {
    let mut cursor = root.walk();
    'down: loop {
        visit(cursor.node());
        if cursor.goto_first_child() {
            continue 'down;
        }
        loop {
            if cursor.goto_next_sibling() {
                continue 'down;
            }
            if !cursor.goto_parent() {
                return;
            }
        }
    }
}

/// Result of the declarator descent (the cauldron-lint `function_name` pattern, extended to
/// classify function declarations vs fn-pointer variables and to surface the parameter list).
struct DeclInfo<'t> {
    name: Option<Node<'t>>,
    params: Option<Node<'t>>,
    /// True when this declarator declares a FUNCTION: the descent hits a `function_declarator`
    /// and reaches the name without passing a pointer AFTER it (`int (*fp)(void)` is a variable;
    /// `int *f(void)` and `int (f)(void)` are functions).
    is_function: bool,
}

/// Iterative declarator descent through pointer/function/array/paren declarators to the name.
fn declarator_info(root: Node) -> DeclInfo {
    let mut d = root;
    let mut params: Option<Node> = None;
    let mut seen_fn = false;
    let mut pointer_after_fn = false;
    loop {
        match d.kind() {
            kind::IDENTIFIER | kind::TYPE_IDENTIFIER => {
                return DeclInfo { name: Some(d), params, is_function: seen_fn && !pointer_after_fn };
            }
            kind::FUNCTION_DECLARATOR => {
                if !seen_fn {
                    seen_fn = true;
                    params = d.child_by_field_name("parameters");
                }
                match d.child_by_field_name("declarator") {
                    Some(n) => d = n,
                    None => break,
                }
            }
            kind::POINTER_DECLARATOR => {
                if seen_fn {
                    pointer_after_fn = true;
                }
                match d.child_by_field_name("declarator") {
                    Some(n) => d = n,
                    None => break,
                }
            }
            kind::ARRAY_DECLARATOR | kind::INIT_DECLARATOR => {
                match d.child_by_field_name("declarator") {
                    Some(n) => d = n,
                    None => break,
                }
            }
            kind::PARENTHESIZED_DECLARATOR | kind::ATTRIBUTED_DECLARATOR => {
                let inner = (0..d.named_child_count())
                    .filter_map(|i| d.named_child(i))
                    .find(|c| c.kind() != kind::COMMENT);
                match inner {
                    Some(n) => d = n,
                    None => break,
                }
            }
            _ => break,
        }
    }
    DeclInfo { name: None, params: None, is_function: false }
}

/// Does a definition/declaration carry `static` storage?
fn has_static(node: Node, text: &str) -> bool {
    (0..node.named_child_count())
        .filter_map(|i| node.named_child(i))
        .any(|c| c.kind() == kind::STORAGE_CLASS_SPECIFIER && &text[c.byte_range()] == "static")
}

/// Parameter count for a `parameter_list`. `None` = variadic or unknowable (K&R `()`).
fn params_arity(params: Node, text: &str) -> Option<u8> {
    let mut named: Vec<Node> = Vec::new();
    for i in 0..params.named_child_count() {
        let Some(c) = params.named_child(i) else { continue };
        match c.kind() {
            kind::COMMENT => {}
            kind::VARIADIC_PARAMETER => return None,
            _ => named.push(c),
        }
    }
    if named.is_empty() {
        return None; // `()` — unspecified arguments in C
    }
    if named.len() == 1
        && named[0].kind() == kind::PARAMETER_DECLARATION
        && text[named[0].byte_range()].trim() == "void"
    {
        return Some(0);
    }
    u8::try_from(named.len()).ok()
}

/// Parameter count for a `preproc_params`. `None` = variadic macro.
fn macro_params_arity(params: Node) -> Option<u8> {
    let mut n = 0usize;
    let mut cur = params.walk();
    if cur.goto_first_child() {
        loop {
            match cur.node().kind() {
                kind::ELLIPSIS => return None,
                kind::IDENTIFIER => n += 1,
                _ => {}
            }
            if !cur.goto_next_sibling() {
                break;
            }
        }
    }
    u8::try_from(n).ok()
}

/// Best-effort argument count at a call site.
fn args_arity(args: Node) -> Option<u8> {
    let n = (0..args.named_child_count())
        .filter_map(|i| args.named_child(i))
        .filter(|c| c.kind() != kind::COMMENT)
        .count();
    u8::try_from(n).ok()
}

/// Is `node` the child at `field` of `parent`?
fn field_is(parent: Node, field: &str, node: Node) -> bool {
    parent.child_by_field_name(field).map(|c| c.id()) == Some(node.id())
}

/// Address-taken context bits for an identifier by its syntactic position (0 = not a candidate:
/// declarator names, field names, plain reads etc. never escape a function's address).
fn ident_ctx(node: Node) -> u8 {
    let Some(parent) = node.parent() else { return 0 };
    match parent.kind() {
        kind::INITIALIZER_LIST => CTX_INIT_LIST,
        kind::INITIALIZER_PAIR if field_is(parent, "value", node) => CTX_INIT_LIST,
        kind::POINTER_EXPRESSION => {
            let is_addr = parent
                .child_by_field_name("operator")
                .is_some_and(|o| o.kind() == kind::AMPERSAND);
            if is_addr {
                CTX_ADDR_OF
            } else {
                0
            }
        }
        kind::ASSIGNMENT_EXPRESSION if field_is(parent, "right", node) => CTX_ASSIGN_RHS,
        kind::INIT_DECLARATOR if field_is(parent, "value", node) => CTX_ASSIGN_RHS,
        kind::ARGUMENT_LIST => CTX_CALL_ARG,
        _ => 0,
    }
}

/// Re-parse one macro replacement list as a raw C fragment and mine identifier-callee calls.
/// If the bare fragment yields nothing (ERROR recovery can drop expression-position calls), retry
/// once in statement position and shift offsets back — offsets always index the ORIGINAL text.
fn mine_macro_body(
    parser: &mut Parser,
    file_text: &str,
    stub: u32,
    value: Range<usize>,
    out: &mut Vec<CallSite>,
) {
    let body = &file_text[value.clone()];
    let mut found = mine_fragment(parser, body);
    if found.is_empty() && body.contains('(') {
        const PRE: &str = "void __cauldron_frag(void){";
        let wrapped = format!("{PRE}{body}\n;}}");
        found = mine_fragment(parser, &wrapped)
            .into_iter()
            .filter_map(|(name, off)| {
                off.checked_sub(PRE.len()).filter(|o| *o < body.len()).map(|o| (name, o))
            })
            .collect();
    }
    for (callee, off) in found {
        out.push(CallSite {
            caller_stub: stub,
            callee,
            offset: value.start + off,
            mined_from_macro: true,
        });
    }
}

/// All `call_expression`s with identifier callees in a fragment, as (name, fragment offset).
fn mine_fragment(parser: &mut Parser, fragment: &str) -> Vec<(String, usize)> {
    let Some(tree) = parser.parse(fragment, None) else { return Vec::new() };
    let mut found: Vec<(String, usize)> = Vec::new();
    for_each_preorder(tree.root_node(), &mut |node| {
        if node.kind() == kind::CALL_EXPRESSION {
            if let Some(f) = node.child_by_field_name("function") {
                if f.kind() == kind::IDENTIFIER {
                    found.push((fragment[f.byte_range()].to_string(), node.start_byte()));
                }
            }
        }
    });
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stub_names(f: &FileFacts, k: StubKind) -> Vec<&str> {
        f.stubs.iter().filter(|s| s.kind == k).map(|s| s.name.as_str()).collect()
    }

    #[test]
    fn static_vs_extern_def() {
        let f = file_facts("static void a(void) { }\nvoid b(int x) { }\n");
        assert_eq!(stub_names(&f, StubKind::FnDef), vec!["a", "b"]);
        let a = &f.stubs[0];
        let b = &f.stubs[1];
        assert!(a.is_static && a.arity == Some(0));
        assert!(!b.is_static && b.arity == Some(1));
        // name_range points at the identifier itself
        let src = "static void a(void) { }\nvoid b(int x) { }\n";
        assert_eq!(&src[a.name_range.clone()], "a");
        assert_eq!(&src[b.name_range.clone()], "b");
    }

    #[test]
    fn stub_name_line_is_the_names_zero_based_row() {
        let src = "#define A 1\nvoid f(void) { }\nstatic int\nmulti(void)\n{ return 0; }\ntypedef int t_t;\n";
        let f = file_facts(src);
        let get = |name: &str| f.stubs.iter().find(|s| s.name == name).expect(name);
        assert_eq!(get("A").name_line, 0);
        assert_eq!(get("f").name_line, 1);
        assert_eq!(get("multi").name_line, 3, "row of the NAME, not the return type's line");
        assert_eq!(get("t_t").name_line, 5);
    }

    #[test]
    fn decl_vs_def() {
        let f = file_facts("int f(int);\nint f(int x) { return x; }\n");
        assert_eq!(stub_names(&f, StubKind::FnDecl), vec!["f"]);
        assert_eq!(stub_names(&f, StubKind::FnDef), vec!["f"]);
    }

    #[test]
    fn fn_pointer_variable_is_not_a_decl() {
        let f = file_facts("int (*fp)(void);\nint *g(void);\n");
        // `fp` is a variable; `g` is a real function declaration returning a pointer.
        assert_eq!(stub_names(&f, StubKind::FnDecl), vec!["g"]);
    }

    #[test]
    fn macro_wrapping_a_call_is_mined() {
        // The exact CFE_ES_PerfLogEntry shape from cfe_es.h.
        let src = "#define CFE_ES_PerfLogEntry(id) (CFE_ES_PerfLogAdd(id, 0))\n";
        let f = file_facts(src);
        assert_eq!(stub_names(&f, StubKind::MacroFn), vec!["CFE_ES_PerfLogEntry"]);
        assert_eq!(f.stubs[0].arity, Some(1));
        let mined: Vec<_> = f.calls.iter().filter(|c| c.mined_from_macro).collect();
        assert_eq!(mined.len(), 1);
        assert_eq!(mined[0].callee, "CFE_ES_PerfLogAdd");
        assert_eq!(mined[0].caller_stub, 0);
        assert!(src[mined[0].offset..].starts_with("CFE_ES_PerfLogAdd("));
    }

    #[test]
    fn dispatch_table_initializer_is_address_taken() {
        let f = file_facts("static const Handler_t T[] = { Foo, Bar };\n");
        assert!(f.address_taken.contains(&("Foo".into(), CTX_INIT_LIST)));
        assert!(f.address_taken.contains(&("Bar".into(), CTX_INIT_LIST)));
        // The table itself is a variable, not a function declaration.
        assert!(stub_names(&f, StubKind::FnDecl).is_empty());
    }

    #[test]
    fn addr_of_and_callback_argument_ctx() {
        let f = file_facts(
            "void u(void) { void *p; p = &f; OS_TaskCreate(cb); h = g2; }\n",
        );
        let get = |n: &str| f.address_taken.iter().find(|(s, _)| s == n).map(|(_, c)| *c);
        assert_eq!(get("f"), Some(CTX_ADDR_OF));
        assert_eq!(get("cb"), Some(CTX_CALL_ARG));
        assert_eq!(get("g2"), Some(CTX_ASSIGN_RHS));
        // OS_TaskCreate is the callee, never address-taken.
        assert_eq!(get("OS_TaskCreate"), None);
        assert!(f.calls.iter().any(|c| c.callee == "OS_TaskCreate" && !c.mined_from_macro));
    }

    #[test]
    fn indirect_sites_with_arity() {
        let src = "void v(void) { table[i](x); p->fn(a, b); (*q.cb)(); }\n";
        let f = file_facts(src);
        assert_eq!(f.indirect_sites.len(), 3);
        for (caller, _, _) in &f.indirect_sites {
            assert_eq!(*caller, 0, "indirect sites attribute to the enclosing FnDef");
        }
        let arities: Vec<Option<u8>> = f.indirect_sites.iter().map(|s| s.2).collect();
        assert_eq!(arities, vec![Some(1), Some(2), Some(0)]);
        // No direct CallSites for the indirect callees.
        assert!(f.calls.is_empty());
    }

    #[test]
    fn error_tolerance() {
        let src = "void ok1(void) { f(); }\n@#$%^&* !!! ;;;\nvoid ok2(void) { g(); }\n";
        let f = file_facts(src);
        let defs = stub_names(&f, StubKind::FnDef);
        assert!(defs.contains(&"ok1") && defs.contains(&"ok2"), "defs = {defs:?}");
        assert!(f.error_bytes > 0);
        assert!(f.calls.iter().any(|c| c.callee == "f"));
        assert!(f.calls.iter().any(|c| c.callee == "g"));
    }

    #[test]
    fn purity_same_text_identical_facts() {
        let src = "#define M(x) (helper(x))\nstatic int helper(int v) { return v; }\nint use(int v) { return M(v) + helper(v); }\n";
        assert_eq!(file_facts(src), file_facts(src));
    }

    #[test]
    fn body_only_edit_changes_body_hash_not_interface_hash() {
        let base = file_facts("void w(void) {\n    f();\n}\n");
        let more = file_facts("void w(void) {\n    f();\n    g();\n}\n");
        assert_eq!(base.interface_hash, more.interface_hash);
        assert_ne!(base.body_hash, more.body_hash);
        // Comment-only edit: BOTH hashes stable — zero index work.
        let comment = file_facts("void w(void) {\n    /* note */ f();\n}\n");
        assert_eq!(base.interface_hash, comment.interface_hash);
        assert_eq!(base.body_hash, comment.body_hash);
    }

    #[test]
    fn typedef_and_macro_obj_stubs() {
        let f = file_facts("typedef void (*Handler)(int);\n#define LIMIT 32\n#define KICK do_kick()\n");
        assert_eq!(stub_names(&f, StubKind::Typedef), vec!["Handler"]);
        let objs = stub_names(&f, StubKind::MacroObj);
        assert!(objs.contains(&"LIMIT") && objs.contains(&"KICK"));
        // Object-like macro bodies are mined too.
        assert!(f
            .calls
            .iter()
            .any(|c| c.callee == "do_kick" && c.mined_from_macro));
    }

    #[test]
    fn variadic_and_unknown_arity() {
        let f = file_facts("int printfish(const char *fmt, ...);\nint old_style();\n");
        assert_eq!(f.stubs[0].arity, None, "variadic");
        assert_eq!(f.stubs[1].arity, None, "K&R ()");
    }
}

/// Recursion-guard recognition: if the byte at `offset` (a witness call site) sits inside an
/// `if`/`while` whose condition reads like a re-entry guard, return the condition text.
///
/// This is the pragmatic tier — a name-pattern match on the dominating condition, not a
/// dataflow proof. It exists so cycles that flight code deliberately bounds (cFE's
/// `CFE_SB_RequestToSendEvent(...) == CFE_SB_GRANTED` gate over `StopRecurseFlags`) read as
/// "guarded recursion" instead of implying an unbounded landmine.
pub fn guard_condition_at(text: &str, offset: usize) -> Option<String> {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_c::language()).ok()?;
    let tree = parser.parse(text, None)?;
    let offset = offset.min(text.len().saturating_sub(1));
    let mut node = tree.root_node().named_descendant_for_byte_range(offset, offset)?;
    loop {
        if matches!(node.kind(), "if_statement" | "while_statement" | "do_statement") {
            if let Some(cond) = node.child_by_field_name("condition") {
                let cond_text = &text[cond.start_byte()..cond.end_byte().min(text.len())];
                if is_guardish(cond_text) {
                    let one_line: String = cond_text.split_whitespace().collect::<Vec<_>>().join(" ");
                    let mut snip: String = one_line.chars().take(90).collect();
                    if one_line.chars().count() > 90 {
                        snip.push('…');
                    }
                    return Some(snip);
                }
            }
        }
        // Stop at the function boundary — a guard outside the function doesn't dominate this call.
        if node.kind() == "function_definition" {
            return None;
        }
        node = node.parent()?;
    }
}

/// Condition-name heuristics for "this bounds re-entry": recursion/reentrancy vocabulary,
/// request-grant gates, depth counters, in-progress latches.
fn is_guardish(cond: &str) -> bool {
    let lower = cond.to_lowercase();
    [
        "recurs", "reentr", "re_entr", "nesting", "nestlevel", "nest_level", "depth",
        "granted", "denied", "requesttosend", "request_to_send", "inprogress", "in_progress",
        "busy", "already", "sending", "active_count",
    ]
    .iter()
    .any(|k| lower.contains(k))
}

#[cfg(test)]
mod guard_tests {
    use super::*;

    #[test]
    fn cfe_style_grant_gate_is_guarded() {
        let src = r#"
void Report(int TskId, int Bit) {
    if (CFE_SB_RequestToSendEvent(TskId, Bit) == CFE_SB_GRANTED)
    {
        CFE_EVS_SendEventWithAppID(1, 2, 3, "x");
        CFE_SB_FinishSendEvent(TskId, Bit);
    }
}
"#;
        let call = src.find("CFE_EVS_SendEventWithAppID").unwrap();
        let g = guard_condition_at(src, call).expect("grant gate recognized");
        assert!(g.contains("RequestToSendEvent"));
    }

    #[test]
    fn depth_counter_is_guarded() {
        let src = "void f(void) {
    if (recursion_depth < 4) { f(); }
}
";
        let call = src.rfind("f()").unwrap();
        assert!(guard_condition_at(src, call).is_some());
    }

    #[test]
    fn plain_condition_is_not_guarded() {
        let src = "void f(int x) {
    if (x > 0) { f(x - 1); }
}
";
        let call = src.rfind("f(x - 1)").unwrap();
        assert!(guard_condition_at(src, call).is_none());
    }

    #[test]
    fn unconditional_call_is_not_guarded() {
        let src = "void g(void);
void f(void) { g(); }
";
        let call = src.rfind("g()").unwrap();
        assert!(guard_condition_at(src, call).is_none());
    }
}
