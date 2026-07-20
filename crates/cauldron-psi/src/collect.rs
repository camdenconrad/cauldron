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
    // --- aggregates (verified against the pinned tree-sitter-c with a shape probe) ------------
    pub const STRUCT_SPECIFIER: &str = "struct_specifier";
    pub const UNION_SPECIFIER: &str = "union_specifier";
    pub const ENUM_SPECIFIER: &str = "enum_specifier";
    pub const FIELD_DECLARATION: &str = "field_declaration";
    pub const FIELD_IDENTIFIER: &str = "field_identifier";
    pub const ENUMERATOR: &str = "enumerator";
    pub const COMPOUND_STATEMENT: &str = "compound_statement";
    pub const TRANSLATION_UNIT: &str = "translation_unit";
}

/// The declared type of a node's `type` field as source text, whitespace-collapsed. An aggregate
/// with a body is trimmed to its HEAD (`struct Tag { … }` -> `struct Tag`) so what we store stays
/// a type reference rather than an entire definition.
fn type_text(node: Node, text: &str) -> String {
    let full = &text[node.byte_range()];
    let head = match node.kind() {
        kind::STRUCT_SPECIFIER | kind::UNION_SPECIFIER | kind::ENUM_SPECIFIER => {
            match node.child_by_field_name("body") {
                // Everything up to the body: `struct Tag `, or bare `struct` when anonymous.
                Some(b) => &text[node.start_byte()..b.start_byte()],
                None => full,
            }
        }
        _ => full,
    };
    head.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Pointer/array/function declarator decoration prepended to a member's stored type, so a
/// `char *name;` field reads as `char *` rather than `char`.
fn declarator_suffix(d: Node) -> &'static str {
    match d.kind() {
        kind::POINTER_DECLARATOR => " *",
        kind::ARRAY_DECLARATOR => " []",
        _ => "",
    }
}

/// The `field_identifier` (or `identifier`) a member declarator bottoms out in, unwrapping
/// pointer / array / parenthesized / function declarators the same way [`declarator_info`] does
/// for functions.
/// A name node only if it actually spans characters. tree-sitter emits ZERO-WIDTH identifier
/// nodes for constructs it is recovering from — an anonymous bitfield, or C++ shapes like
/// `operator T*()` seen through the C grammar. An empty-named stub is unusable and pollutes
/// every by-name map, so it must never be created.
fn nonempty(n: Node) -> Option<Node> {
    (!n.byte_range().is_empty()).then_some(n)
}

fn member_name(d: Node) -> Option<Node> {
    let mut cur = d;
    loop {
        match cur.kind() {
            // An ANONYMOUS bitfield (`unsigned : 7;`) still produces a field_identifier — a
            // ZERO-WIDTH one. It names nothing and must not become a member.
            kind::FIELD_IDENTIFIER | kind::IDENTIFIER => {
                return (!cur.byte_range().is_empty()).then_some(cur)
            }
            // `(*run)(int)` nests function_declarator > parenthesized_declarator > pointer_
            // declarator > field_identifier, and the parenthesized layer exposes NO `declarator`
            // field — its children are the bare parens and the inner declarator. Descend by
            // position there, by field everywhere else.
            kind::PARENTHESIZED_DECLARATOR => cur = cur.named_child(0)?,
            kind::POINTER_DECLARATOR
            | kind::ARRAY_DECLARATOR
            | kind::ATTRIBUTED_DECLARATOR
            | kind::INIT_DECLARATOR
            | kind::FUNCTION_DECLARATOR => {
                cur = cur.child_by_field_name("declarator")?;
            }
            _ => return None,
        }
    }
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
    // APPEND-ONLY BELOW. The discriminant is cast to u8 into `interface_hash`; reordering the
    // variants above would silently change every file's hash and force a full reindex.
    /// `struct S { … }` — a definition WITH a body.
    Struct,
    /// `union U { … }`.
    Union,
    /// `enum E { … }`.
    Enum,
    /// `struct S;` — a forward declaration, no body.
    TagDecl,
    /// One member of a struct/union. `parent` names the aggregate.
    Field,
    /// One `enum` constant. `parent` names the enum.
    Enumerator,
    /// A file-scope variable with a definition.
    Global,
    /// `extern int x;` — a file-scope variable declared elsewhere.
    GlobalDecl,
}

impl StubKind {
    /// Is this a type the user can navigate to (struct/union/enum/typedef)?
    pub fn is_type(self) -> bool {
        matches!(
            self,
            StubKind::Struct | StubKind::Union | StubKind::Enum | StubKind::TagDecl | StubKind::Typedef
        )
    }

    /// Is this a MEMBER of an aggregate rather than a top-level entity?
    pub fn is_member(self) -> bool {
        matches!(self, StubKind::Field | StubKind::Enumerator)
    }
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
    /// Span of the `parameter_list` INCLUDING its parentheses, i.e. `(int a, int b)`.
    /// `None` for macros and typedefs, and for anything with no parseable parameter list.
    /// Change Signature rewrites exactly this span on the declaration side.
    pub params_range: Option<Range<usize>>,
    /// Byte span of each parameter declaration, in source order, excluding commas and comments.
    /// A lone `void` yields NO entries (it means zero parameters, not one named `void`); a
    /// variadic `...` contributes its own span so a rewriter can preserve it.
    pub param_ranges: Vec<Range<usize>>,
    /// Owning aggregate, as an index into the SAME file's `stubs`. Set for [`StubKind::Field`]
    /// and [`StubKind::Enumerator`], and for a tag nested inside another aggregate. `None` for
    /// anything at file scope. An index (not a name) because anonymous aggregates have none.
    pub parent: Option<u32>,
    /// Declared type, verbatim source text — `int`, `char *`, `struct Cfg *`. Present for fields,
    /// globals, and typedefs (where it is the ALIASED type); `None` for everything else. Kept as
    /// text rather than a parsed type: consumers show it, and C's declarator syntax makes a
    /// faithful parse a much larger commitment than this slice needs.
    pub ty: Option<String>,
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
    /// Span of the `argument_list` INCLUDING its parentheses, i.e. `(a, b)` in `f(a, b)`.
    /// `None` for macro-mined calls, whose offsets point into a macro body rather than a real
    /// call expression — those must never be rewritten. Rewriting refactors (Change Signature)
    /// key off this; it is deliberately excluded from the file hashes so that pure body edits
    /// which merely shift bytes stay free.
    pub args_range: Option<Range<usize>>,
    /// Byte span of each argument expression, in source order, excluding commas and whitespace.
    /// Empty for a zero-argument call. `None` alongside a `None` [`Self::args_range`].
    pub arg_ranges: Vec<Range<usize>>,
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
        // (aggregate stub idx, end byte) for the struct/union/enum whose members we are inside.
        let mut agg_stack: Vec<(u32, usize)> = Vec::new();
        // End bytes of every compound_statement, giving an exact "is this file scope?" test.
        // A block-local `struct` or variable must NOT be indexed as a file-scope entity.
        let mut block_stack: Vec<usize> = Vec::new();

        for_each_preorder(tree.root_node(), &mut |node| {
            let start = node.start_byte();
            while fn_stack.last().is_some_and(|&(_, end)| end <= start) {
                fn_stack.pop();
            }
            while err_stack.last().is_some_and(|&end| end <= start) {
                err_stack.pop();
            }
            while agg_stack.last().is_some_and(|&(_, end)| end <= start) {
                agg_stack.pop();
            }
            while block_stack.last().is_some_and(|&end| end <= start) {
                block_stack.pop();
            }
            if node.kind() == kind::COMPOUND_STATEMENT {
                block_stack.push(node.end_byte());
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
                        if let Some(name_node) = info.name.and_then(nonempty) {
                            let idx = stubs.len() as u32;
                            stubs.push(Stub {
                                name: text[name_node.byte_range()].to_string(),
                                kind: StubKind::FnDef,
                                is_static: has_static(node, text),
                                byte_range: node.byte_range(),
                                name_range: name_node.byte_range(),
                                name_line: name_node.start_position().row,
                                arity: info.params.and_then(|p| params_arity(p, text)),
                                params_range: info.params.map(|p| p.byte_range()),
                                param_ranges: info
                                    .params
                                    .map(|p| param_ranges(p, text))
                                    .unwrap_or_default(),
                                parent: None,
                                ty: None,
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
                            // A file-scope VARIABLE. Only at file scope: `fn_stack` excludes K&R
                            // parameter declarations (inside function_definition but outside any
                            // block) and `block_stack` excludes ordinary locals.
                            if block_stack.is_empty() && fn_stack.is_empty() {
                                if let Some(name_node) = member_name(d).and_then(nonempty) {
                                    // `extern int x;` DECLARES; anything with an initializer, or
                                    // without `extern`, defines.
                                    let is_extern = has_storage(node, text, "extern");
                                    let defines =
                                        !is_extern || d.kind() == kind::INIT_DECLARATOR;
                                    stubs.push(Stub {
                                        name: text[name_node.byte_range()].to_string(),
                                        kind: match defines {
                                            true => StubKind::Global,
                                            false => StubKind::GlobalDecl,
                                        },
                                        is_static,
                                        byte_range: node.byte_range(),
                                        name_range: name_node.byte_range(),
                                        name_line: name_node.start_position().row,
                                        arity: None,
                                        params_range: None,
                                        param_ranges: Vec::new(),
                                        parent: None,
                                        ty: node
                                            .child_by_field_name("type")
                                            .map(|t| type_text(t, text))
                                            .map(|b| format!("{b}{}", declarator_suffix(d))),
                                    });
                                }
                            }
                            continue; // fn-pointer variables etc. are not declarations of functions
                        }
                        if let Some(name_node) = info.name.and_then(nonempty) {
                            stubs.push(Stub {
                                name: text[name_node.byte_range()].to_string(),
                                kind: StubKind::FnDecl,
                                is_static,
                                byte_range: node.byte_range(),
                                name_range: name_node.byte_range(),
                                name_line: name_node.start_position().row,
                                arity: info.params.and_then(|p| params_arity(p, text)),
                                params_range: info.params.map(|p| p.byte_range()),
                                param_ranges: info
                                    .params
                                    .map(|p| param_ranges(p, text))
                                    .unwrap_or_default(),
                                parent: None,
                                ty: None,
                            });
                        }
                    }
                }
                kind::TYPE_DEFINITION => {
                    let mut cur = node.walk();
                    let decls: Vec<Node> =
                        node.children_by_field_name("declarator", &mut cur).collect();
                    for d in decls {
                        if let Some(name_node) = declarator_info(d).name.and_then(nonempty) {
                            stubs.push(Stub {
                                name: text[name_node.byte_range()].to_string(),
                                kind: StubKind::Typedef,
                                is_static: false,
                                byte_range: node.byte_range(),
                                name_range: name_node.byte_range(),
                                name_line: name_node.start_position().row,
                                arity: None,
                                // Typedefs have no parameter_list to rewrite.
                                params_range: None,
                                param_ranges: Vec::new(),
                                parent: None,
                                // The aliased type: `typedef struct Tag {…} Tag_t` -> "struct Tag {…}"
                                // is trimmed to its head so the stored text stays a type, not a body.
                                ty: node
                                    .child_by_field_name("type")
                                    .map(|t| type_text(t, text)),
                            });
                        }
                    }
                }
                kind::STRUCT_SPECIFIER | kind::UNION_SPECIFIER | kind::ENUM_SPECIFIER => {
                    // An aggregate with a `body` is a definition; without one it is either a
                    // forward declaration (`struct S;`) or a mere reference in a type position
                    // (`struct S *p;`). Only the former two are entities.
                    let Some(name_node) = node.child_by_field_name("name").and_then(nonempty)
                    else {
                        // Anonymous (`typedef struct { … } T;`). It has no name to index, but its
                        // members still belong to something — push a parentless scope so the
                        // fields below do not attach to an enclosing aggregate by accident.
                        if node.child_by_field_name("body").is_some() {
                            agg_stack.push((u32::MAX, node.end_byte()));
                        }
                        return;
                    };
                    let has_body = node.child_by_field_name("body").is_some();
                    // Without a body this is a forward declaration ONLY when it stands alone as a
                    // statement (`struct S;`, a direct child of the translation unit). Everywhere
                    // else — `struct S *p;`, a parameter type, a cast — it is just a REFERENCE to
                    // a type declared elsewhere, and indexing those would bury the real
                    // declaration under one entry per mention.
                    let standalone = node.parent().map(|p| p.kind()) == Some(kind::TRANSLATION_UNIT);
                    if !has_body && !standalone {
                        return;
                    }
                    let kind_of = match (node.kind(), has_body) {
                        (_, false) => StubKind::TagDecl,
                        (kind::UNION_SPECIFIER, _) => StubKind::Union,
                        (kind::ENUM_SPECIFIER, _) => StubKind::Enum,
                        _ => StubKind::Struct,
                    };
                    let idx = stubs.len() as u32;
                    stubs.push(Stub {
                        name: text[name_node.byte_range()].to_string(),
                        kind: kind_of,
                        is_static: false,
                        byte_range: node.byte_range(),
                        name_range: name_node.byte_range(),
                        name_line: name_node.start_position().row,
                        arity: None,
                        params_range: None,
                        param_ranges: Vec::new(),
                        parent: agg_stack.last().map(|&(p, _)| p).filter(|p| *p != u32::MAX),
                        ty: None,
                    });
                    if has_body {
                        agg_stack.push((idx, node.end_byte()));
                    }
                }
                kind::FIELD_DECLARATION => {
                    let parent = agg_stack.last().map(|&(p, _)| p).filter(|p| *p != u32::MAX);
                    let base = node.child_by_field_name("type").map(|t| type_text(t, text));
                    let mut cur = node.walk();
                    let decls: Vec<Node> =
                        node.children_by_field_name("declarator", &mut cur).collect();
                    for d in decls {
                        // An anonymous bitfield (`unsigned : 5;`) has no declarator child at all,
                        // so it never reaches here — which is the right outcome: it has no name.
                        let Some(name_node) = member_name(d).and_then(nonempty) else { continue };
                        stubs.push(Stub {
                            name: text[name_node.byte_range()].to_string(),
                            kind: StubKind::Field,
                            is_static: false,
                            byte_range: node.byte_range(),
                            name_range: name_node.byte_range(),
                            name_line: name_node.start_position().row,
                            arity: None,
                            params_range: None,
                            param_ranges: Vec::new(),
                            parent,
                            ty: base
                                .as_ref()
                                .map(|b| format!("{b}{}", declarator_suffix(d))),
                        });
                    }
                }
                kind::ENUMERATOR => {
                    if let Some(name_node) = node.child_by_field_name("name").and_then(nonempty) {
                        stubs.push(Stub {
                            name: text[name_node.byte_range()].to_string(),
                            kind: StubKind::Enumerator,
                            is_static: false,
                            byte_range: node.byte_range(),
                            name_range: name_node.byte_range(),
                            name_line: name_node.start_position().row,
                            arity: None,
                            params_range: None,
                            param_ranges: Vec::new(),
                            parent: agg_stack.last().map(|&(p, _)| p).filter(|p| *p != u32::MAX),
                            ty: None,
                        });
                    }
                }
                kind::PREPROC_FUNCTION_DEF => {
                    if let Some(name_node) = node.child_by_field_name("name").and_then(nonempty) {
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
                            // `preproc_params` is not a C parameter_list; Change Signature does
                            // not rewrite macros, so withhold spans rather than offer bad ones.
                            params_range: None,
                            param_ranges: Vec::new(),
                            parent: None,
                            ty: None,
                        });
                        if let Some(v) = node.child_by_field_name("value") {
                            macro_bodies.push((idx, v.byte_range()));
                        }
                    }
                }
                kind::PREPROC_DEF => {
                    if let Some(name_node) = node.child_by_field_name("name").and_then(nonempty) {
                        let idx = stubs.len() as u32;
                        stubs.push(Stub {
                            name: text[name_node.byte_range()].to_string(),
                            kind: StubKind::MacroObj,
                            is_static: false,
                            byte_range: node.byte_range(),
                            name_range: name_node.byte_range(),
                            name_line: name_node.start_position().row,
                            arity: None,
                            // Object-like macro: no parameter list at all.
                            params_range: None,
                            param_ranges: Vec::new(),
                            parent: None,
                            ty: None,
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
                            let args = node.child_by_field_name("arguments");
                            calls.push(CallSite {
                                caller_stub: caller,
                                callee: text[f.byte_range()].to_string(),
                                offset: node.start_byte(),
                                mined_from_macro: false,
                                args_range: args.map(|a| a.byte_range()),
                                arg_ranges: args.map(arg_ranges).unwrap_or_default(),
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

    // The tuple carries the PARENT'S NAME and the declared TYPE as well. Without them, changing
    // `int x;` to `long x;` inside a struct, or moving a field between two structs, left the
    // interface hash identical — an interface change that dependents would never be told about.
    let mut iface: Vec<(&str, u8, bool, Option<u8>, Option<&str>, Option<&str>)> = stubs
        .iter()
        .map(|s| {
            (
                s.name.as_str(),
                s.kind as u8,
                s.is_static,
                s.arity,
                s.parent.and_then(|p| stubs.get(p as usize)).map(|p| p.name.as_str()),
                s.ty.as_deref(),
            )
        })
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
    has_storage(node, text, "static")
}

/// Does `node` carry the given storage-class specifier (`static`, `extern`, …)?
fn has_storage(node: Node, text: &str, want: &str) -> bool {
    (0..node.named_child_count())
        .filter_map(|i| node.named_child(i))
        .any(|c| c.kind() == kind::STORAGE_CLASS_SPECIFIER && &text[c.byte_range()] == want)
}

/// Byte span of each parameter in a `parameter_list`, in source order.
///
/// A single `void` parameter is C's spelling of "no parameters" and yields an EMPTY list, so
/// callers can treat `param_ranges.len()` as the real parameter count. `...` is included, so a
/// rewriter can keep a variadic tail while editing the fixed parameters before it.
fn param_ranges(params: Node, text: &str) -> Vec<Range<usize>> {
    let named: Vec<Node> = (0..params.named_child_count())
        .filter_map(|i| params.named_child(i))
        .filter(|c| c.kind() != kind::COMMENT)
        .collect();
    if named.len() == 1
        && named[0].kind() == kind::PARAMETER_DECLARATION
        && text[named[0].byte_range()].trim() == "void"
    {
        return Vec::new();
    }
    named.into_iter().map(|c| c.byte_range()).collect()
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

/// Byte span of each argument expression at a call site, in source order.
///
/// Comments are skipped so `f(a /* why */, b)` yields two arguments, matching [`args_arity`].
/// Spans cover the expression only — commas, the enclosing parens, and surrounding whitespace
/// are excluded, so a rewriter can replace an argument without disturbing the call's layout.
fn arg_ranges(args: Node) -> Vec<Range<usize>> {
    (0..args.named_child_count())
        .filter_map(|i| args.named_child(i))
        .filter(|c| c.kind() != kind::COMMENT)
        .map(|c| c.byte_range())
        .collect()
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
            // A macro-mined "call" is text inside a macro BODY, not a call expression. Its
            // offset is useful for the call graph but there is nothing here a rewriter may
            // safely edit — every expansion site would need its own edit.
            args_range: None,
            arg_ranges: Vec::new(),
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

    /// Members of the aggregate at `stubs[parent_of]`, in declaration order.
    fn members(f: &FileFacts, agg: &str) -> Vec<(String, Option<String>)> {
        let Some(ix) = f.stubs.iter().position(|s| s.name == agg && s.kind.is_type()) else {
            return Vec::new();
        };
        f.stubs
            .iter()
            .filter(|s| s.parent == Some(ix as u32) && s.kind.is_member())
            .map(|s| (s.name.clone(), s.ty.clone()))
            .collect()
    }

    #[test]
    fn struct_fields_are_indexed_with_their_types() {
        let f = file_facts("struct Point { int x; char *name; };\n");
        assert_eq!(stub_names(&f, StubKind::Struct), vec!["Point"]);
        assert_eq!(
            members(&f, "Point"),
            vec![
                ("x".to_string(), Some("int".to_string())),
                // The pointer lives in the DECLARATOR, not the type node — a field typed `char`
                // would be wrong here.
                ("name".to_string(), Some("char *".to_string())),
            ]
        );
    }

    #[test]
    fn union_and_enum_members() {
        let f = file_facts("union U { int i; float f; };\nenum Color { RED, GREEN = 5 };\n");
        assert_eq!(stub_names(&f, StubKind::Union), vec!["U"]);
        assert_eq!(stub_names(&f, StubKind::Enum), vec!["Color"]);
        assert_eq!(members(&f, "U").len(), 2);
        let colors: Vec<String> = members(&f, "Color").into_iter().map(|(n, _)| n).collect();
        assert_eq!(colors, vec!["RED", "GREEN"], "an explicit value must not drop the constant");
    }

    #[test]
    fn anonymous_bitfield_has_no_name_and_is_skipped() {
        let f = file_facts("struct S { unsigned a : 1; unsigned : 7; unsigned b : 1; };\n");
        let names: Vec<String> = members(&f, "S").into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["a", "b"], "the padding field has no name to index");
    }

    #[test]
    fn nested_aggregate_is_a_type_not_a_member() {
        // A completion after `outer.` must offer `in`, never the type `Inner`.
        let f = file_facts("struct Outer { struct Inner { int y; } in; };\n");
        let names: Vec<String> = members(&f, "Outer").into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["in"]);
        assert_eq!(members(&f, "Inner").into_iter().map(|(n, _)| n).collect::<Vec<_>>(), vec!["y"]);
    }

    #[test]
    fn typedef_records_the_aliased_type() {
        let f = file_facts("typedef unsigned int u32;\ntypedef struct Tag { int a; } Tag_t;\n");
        let t: Vec<(&str, Option<&str>)> = f
            .stubs
            .iter()
            .filter(|s| s.kind == StubKind::Typedef)
            .map(|s| (s.name.as_str(), s.ty.as_deref()))
            .collect();
        assert_eq!(t, vec![("u32", Some("unsigned int")), ("Tag_t", Some("struct Tag"))]);
        // The body is NOT stored in `ty` — a type reference, not a definition.
        assert_eq!(members(&f, "Tag").into_iter().map(|(n, _)| n).collect::<Vec<_>>(), vec!["a"]);
    }

    #[test]
    fn forward_declaration_versus_a_mere_reference() {
        // `struct Fwd;` declares. `struct Fwd *p;` only MENTIONS — indexing every mention would
        // bury the real declaration under one row per use.
        let f = file_facts("struct Fwd;\nstruct Fwd *p;\nvoid g(struct Fwd *q) { }\n");
        assert_eq!(stub_names(&f, StubKind::TagDecl), vec!["Fwd"], "exactly one");
    }

    #[test]
    fn file_scope_globals_but_not_locals() {
        let src = "static int g_count = 0;\nextern int g_ext;\nint g_def;\n\
                   void f(void) { int local = 1; struct Tmp { int z; } t; }\n";
        let f = file_facts(src);
        assert_eq!(stub_names(&f, StubKind::Global), vec!["g_count", "g_def"]);
        assert_eq!(stub_names(&f, StubKind::GlobalDecl), vec!["g_ext"]);
        assert!(
            !f.stubs.iter().any(|s| s.name == "local"),
            "a block-local variable is not a file-scope entity"
        );
        assert!(f.stubs.iter().find(|s| s.name == "g_count").unwrap().is_static);
        assert!(!f.stubs.iter().find(|s| s.name == "g_def").unwrap().is_static);
    }

    #[test]
    fn function_pointer_field_keeps_its_name() {
        let f = file_facts("struct Ops { int (*run)(int); void *ctx; };\n");
        let names: Vec<String> = members(&f, "Ops").into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["run", "ctx"]);
    }

    #[test]
    fn anonymous_struct_members_do_not_leak_to_an_enclosing_aggregate() {
        // `typedef struct { … } T;` has no tag. Its fields must not attach to whatever aggregate
        // happens to be open, and must not crash.
        let f = file_facts("struct Outer { int a; };\ntypedef struct { int b; } T;\n");
        let names: Vec<String> = members(&f, "Outer").into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["a"], "`b` belongs to the anonymous struct, not Outer");
    }

    #[test]
    fn interface_hash_notices_a_field_type_change() {
        // Widening a field is an INTERFACE change; dependents must be told.
        let a = file_facts("struct S { int x; };\n");
        let b = file_facts("struct S { long x; };\n");
        assert_ne!(a.interface_hash, b.interface_hash);
    }

    #[test]
    fn interface_hash_notices_a_field_moving_between_structs() {
        let a = file_facts("struct A { int x; };\nstruct B { int y; };\n");
        let b = file_facts("struct A { int y; };\nstruct B { int x; };\n");
        assert_ne!(a.interface_hash, b.interface_hash, "parent name is part of the interface");
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
    fn param_and_arg_spans_cover_exact_source_text() {
        let src = "\
int add(int a, char *b) { return 0; }
void call_it(void) { add(1 + 2, \"hi\"); }
";
        let f = file_facts(src);
        let add = f.stubs.iter().find(|s| s.name == "add" && s.kind == StubKind::FnDef).unwrap();
        // The params span includes the parens, so a rewriter replaces the whole list at once.
        assert_eq!(&src[add.params_range.clone().unwrap()], "(int a, char *b)");
        let params: Vec<&str> =
            add.param_ranges.iter().map(|r| &src[r.clone()]).collect();
        assert_eq!(params, ["int a", "char *b"]);

        let call = f.calls.iter().find(|c| c.callee == "add").unwrap();
        assert_eq!(&src[call.args_range.clone().unwrap()], "(1 + 2, \"hi\")");
        // Argument spans cover the expression only — no commas, no surrounding whitespace —
        // so replacing one leaves the call's formatting untouched.
        let args: Vec<&str> = call.arg_ranges.iter().map(|r| &src[r.clone()]).collect();
        assert_eq!(args, ["1 + 2", "\"hi\""]);
    }

    #[test]
    fn lone_void_param_is_zero_parameters_not_one() {
        // `f(void)` means NO parameters in C. A rewriter that saw one parameter here would try
        // to keep or reorder a parameter that does not exist.
        let f = file_facts("void f(void) {}\nvoid g() {}\n");
        let vf = f.stubs.iter().find(|s| s.name == "f").unwrap();
        assert!(vf.param_ranges.is_empty());
        assert_eq!(vf.arity, Some(0));
        // K&R `()` is unspecified, not zero — no params to list either.
        let g = f.stubs.iter().find(|s| s.name == "g").unwrap();
        assert!(g.param_ranges.is_empty());
        assert_eq!(g.arity, None);
    }

    #[test]
    fn variadic_tail_gets_its_own_param_span() {
        let src = "int logf(const char *fmt, ...);\n";
        let f = file_facts(src);
        let d = f.stubs.iter().find(|s| s.name == "logf").unwrap();
        let params: Vec<&str> = d.param_ranges.iter().map(|r| &src[r.clone()]).collect();
        // `...` is preserved as a span so the fixed parameters before it can be edited while
        // the variadic tail survives.
        assert_eq!(params, ["const char *fmt", "..."]);
        assert_eq!(d.arity, None);
    }

    #[test]
    fn zero_arg_call_has_empty_arg_ranges_but_a_real_args_span() {
        let src = "void f(void); void g(void) { f(); }\n";
        let f = file_facts(src);
        let call = f.calls.iter().find(|c| c.callee == "f").unwrap();
        assert!(call.arg_ranges.is_empty());
        // The span still exists — that is where an added argument gets inserted.
        assert_eq!(&src[call.args_range.clone().unwrap()], "()");
    }

    #[test]
    fn macro_mined_calls_withhold_spans() {
        // Offsets inside a macro BODY are not a call expression; handing a rewriter spans here
        // would corrupt the macro definition.
        let f = file_facts("#define M() target(1)\nvoid u(void) { M(); }\n");
        let mined = f.calls.iter().find(|c| c.mined_from_macro).unwrap();
        assert_eq!(mined.callee, "target");
        assert!(mined.args_range.is_none());
        assert!(mined.arg_ranges.is_empty());
    }

    #[test]
    fn nested_call_args_are_spanned_per_call_not_flattened() {
        let src = "void u(void) { outer(inner(1, 2), 3); }\n";
        let f = file_facts(src);
        let outer = f.calls.iter().find(|c| c.callee == "outer").unwrap();
        let outer_args: Vec<&str> = outer.arg_ranges.iter().map(|r| &src[r.clone()]).collect();
        // The whole nested call is ONE argument of `outer`.
        assert_eq!(outer_args, ["inner(1, 2)", "3"]);
        let inner = f.calls.iter().find(|c| c.callee == "inner").unwrap();
        let inner_args: Vec<&str> = inner.arg_ranges.iter().map(|r| &src[r.clone()]).collect();
        assert_eq!(inner_args, ["1", "2"]);
    }

    #[test]
    fn comments_between_args_do_not_become_arguments() {
        let src = "void u(void) { f(a /* why */, b); }\n";
        let f = file_facts(src);
        let call = f.calls.iter().find(|c| c.callee == "f").unwrap();
        let args: Vec<&str> = call.arg_ranges.iter().map(|r| &src[r.clone()]).collect();
        assert_eq!(args, ["a", "b"]);
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
