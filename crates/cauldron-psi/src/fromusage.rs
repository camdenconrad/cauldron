//! Create Function From Usage: given a call to something that does not exist, write the function.
//!
//! The signature has to be inferred from the CALL, which is the only evidence available. Argument
//! types come from what each argument syntactically is — a literal, a variable whose declaration
//! is in scope, a call to a function we know, a string. Anything unreadable becomes `int` with a
//! `TODO` marker rather than blocking the whole action, because a stub you then fix up beats no
//! stub at all; that is the one place this module guesses on purpose, and it says so in the
//! generated code.
//!
//! The return type comes from CONTEXT, not from the call: `int x = f();` says `int`, `f();` on its
//! own says `void`, `if (f())` says `int`. Reading the context is what makes the stub compile
//! against the call that motivated it.

use std::ops::Range;

use tree_sitter::Node;

/// A generated stub and where to put it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StubPlan {
    /// The name being defined.
    pub name: String,
    /// Byte offset for the insertion (always a line start).
    pub insert_at: usize,
    /// The whole function, with a trailing blank line.
    pub text: String,
    /// Byte range of the body's placeholder inside `text`, so the caller can drop the caret there.
    pub body_placeholder: Range<usize>,
    /// True when at least one argument type had to be guessed.
    pub guessed_types: bool,
}

/// Plan a stub for the call at `offset` in `src`. `known` answers "does a function with this name
/// already exist?" — the caller wires it to the PSI index, so a call to something real never
/// offers to redefine it.
pub fn plan(src: &str, offset: usize, known: &dyn Fn(&str) -> bool) -> Option<StubPlan> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&tree_sitter_c::language()).ok()?;
    let tree = parser.parse(src, None)?;
    let root = tree.root_node();
    let call = enclosing_call(root, offset)?;
    let callee = call.child_by_field_name("function")?;
    if callee.kind() != "identifier" {
        return None; // a call through a pointer or a member has no name to define
    }
    let name = src[callee.byte_range()].to_string();
    if known(&name) {
        return None;
    }

    // The enclosing function, so the stub can be placed above it (declared before use).
    let host = enclosing(root, offset, "function_definition")?;
    // Scoped to the HOST, not the whole file: a syntax error in an unrelated function elsewhere
    // (or C++ in the same header) must not disable the action for code that parses fine. Every
    // inference below reads types off this function's tree, so only this function must be clean.
    if host.has_error() {
        return None;
    }
    let insert_at = line_start(src, host.start_byte());

    let args = call.child_by_field_name("arguments")?;
    let mut cur = args.walk();
    let arg_nodes: Vec<Node> = args.named_children(&mut cur).collect();

    let mut guessed = false;
    let mut params: Vec<String> = Vec::new();
    let mut used_names: Vec<String> = Vec::new();
    for (i, a) in arg_nodes.iter().enumerate() {
        let (ty, sure) = argument_type(*a, src, host);
        if !sure {
            guessed = true;
        }
        // `char *p`, not `char * p` — the star binds to the declarator in C, and every C
        // codebase writes it that way.
        let sep = if ty.ends_with('*') { "" } else { " " };
        // `f(n, n)` would otherwise emit two parameters called `n`, which does not compile.
        let mut nm = param_name(*a, src, i);
        while used_names.contains(&nm) {
            nm = format!("{nm}_{}", i + 1);
        }
        used_names.push(nm.clone());
        params.push(format!("{ty}{sep}{nm}"));
    }
    let sig_params = match params.is_empty() {
        true => "void".to_string(),
        false => params.join(", "),
    };
    let ret = return_type(call, src);

    let mut text = String::new();
    if guessed {
        // Say so IN the code. A silently wrong parameter type is a compile error the user has to
        // trace back; a marked one is a two-second fix.
        text.push_str("/* TODO: check the generated parameter types */\n");
    }
    text.push_str(&format!("static {ret} {name}({sig_params})\n{{\n"));
    let ph_start = text.len();
    let placeholder = match ret.as_str() {
        "void" => "    /* TODO */".to_string(),
        _ => format!("    /* TODO */\n    return 0;"),
    };
    text.push_str(&placeholder);
    let ph_end = ph_start + "    /* TODO */".len();
    text.push_str("\n}\n\n");

    Some(StubPlan {
        name,
        insert_at,
        text,
        body_placeholder: ph_start..ph_end,
        guessed_types: guessed,
    })
}

/// The innermost `call_expression` containing `offset`.
fn enclosing_call(root: Node, offset: usize) -> Option<Node> {
    enclosing(root, offset, "call_expression")
}

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

/// The type to give a parameter, and whether we are sure of it.
fn argument_type(a: Node, src: &str, host: Node) -> (String, bool) {
    match a.kind() {
        "number_literal" => {
            let t = &src[a.byte_range()];
            let hex = t.starts_with("0x") || t.starts_with("0X");
            // `0x1f` is an INT. Only a non-hex literal's trailing f/F means float, and a hex
            // float still needs the `p` exponent to be one.
            let float = !hex && (t.contains('.') || t.ends_with('f') || t.ends_with('F'));
            (if float { "double" } else { "int" }.to_string(), true)
        }
        "string_literal" => ("const char *".to_string(), true),
        "char_literal" => ("char".to_string(), true),
        "true" | "false" => ("int".to_string(), true),
        "identifier" => {
            let name = &src[a.byte_range()];
            match declared_type_in(host, src, name) {
                Some(DeclLookup::Unique(ty)) => (ty, true),
                // Declared more than once in this function (shadowing): which one the call sees
                // depends on block scope, which is not modelled. Guess and mark it.
                Some(DeclLookup::Ambiguous(ty)) => (ty, false),
                None => ("int".to_string(), false),
            }
        }
        "pointer_expression" => {
            let addr_of = src[a.byte_range()].starts_with('&');
            let inner = a.named_child(0).map(|i| argument_type(i, src, host));
            match (addr_of, inner) {
                // `&x` — a pointer to whatever x is.
                (true, Some((ty, sure))) => (format!("{ty} *"), sure),
                // `*p` — the POINTEE type, which we would have to strip a star off `p`'s
                // declared type to know. Answering with the pointer's own type was wrong AND
                // marked certain; guess and say so instead.
                (false, Some((ty, _))) => (ty.trim_end_matches([' ', '*']).to_string(), false),
                (_, None) => ("int".to_string(), false),
            }
        }
        "cast_expression" => match a.child_by_field_name("type") {
            Some(t) => (src[t.byte_range()].split_whitespace().collect::<Vec<_>>().join(" "), true),
            None => ("int".to_string(), false),
        },
        _ => ("int".to_string(), false),
    }
}

/// Result of resolving a name to a declaration in the host function.
enum DeclLookup {
    /// Exactly one declaration of this name — the type is trustworthy.
    Unique(String),
    /// Several declarations (shadowing). The first is returned, but the caller must treat it as
    /// a guess: which one the call actually sees depends on block scope, which is not modelled.
    Ambiguous(String),
}

/// The declared type of local `name` inside `host`, if a declaration or parameter gives one.
fn declared_type_in(host: Node, src: &str, name: &str) -> Option<DeclLookup> {
    fn search(n: Node, src: &str, name: &str, found: &mut Vec<String>, depth: usize) {
        if depth > 400 {
            return;
        }
        if n.kind() == "declaration" || n.kind() == "parameter_declaration" {
            let ty = n.child_by_field_name("type").map(|t| text_of(t, src));
            let mut cur = n.walk();
            for d in n.children_by_field_name("declarator", &mut cur) {
                if declarator_name(d, src).as_deref() == Some(name) {
                    if let Some(ty) = &ty {
                        found.push(format!("{ty}{}", pointer_suffix(d)));
                    }
                }
            }
        }
        let mut cur = n.walk();
        for ch in n.named_children(&mut cur) {
            search(ch, src, name, found, depth + 1);
        }
    }
    let mut found = Vec::new();
    search(host, src, name, &mut found, 0);
    match found.len() {
        0 => None,
        1 => Some(DeclLookup::Unique(found.remove(0))),
        _ => Some(DeclLookup::Ambiguous(found.remove(0))),
    }
}

/// The return type implied by what the call's RESULT is used for.
fn return_type(call: Node, src: &str) -> String {
    let Some(parent) = call.parent() else { return "void".into() };
    match parent.kind() {
        // `f();` as a statement — nothing wants a value.
        "expression_statement" => "void".to_string(),
        // `int x = f();` — the declaration's own type.
        // `char *p = f();` is `char *`, not `char`: the pointer lives in the DECLARATOR, and
        // dropping it generated a function whose return type could not be assigned.
        "init_declarator" => parent
            .parent()
            .and_then(|d| {
                let base = d.child_by_field_name("type").map(|t| text_of(t, src))?;
                Some(format!("{base}{}", pointer_suffix(parent)))
            })
            .unwrap_or_else(|| "int".to_string()),
        // `x = f();` — whatever x is, which we cannot see from here without scope; `int` is the
        // honest default and the assignment will flag a mismatch immediately if wrong.
        "assignment_expression" => "int".to_string(),
        "return_statement" => "int".to_string(),
        _ => "int".to_string(),
    }
}

/// A readable parameter name: reuse the argument's own identifier where there is one, else `aN`.
fn param_name(a: Node, src: &str, i: usize) -> String {
    match a.kind() {
        "identifier" => src[a.byte_range()].to_string(),
        // `&count` should name its parameter `count`, not `a1` — the caller already told us what
        // this value is called.
        "pointer_expression" | "parenthesized_expression" => a
            .named_child(0)
            .map(|inner| param_name(inner, src, i))
            .unwrap_or_else(|| format!("a{}", i + 1)),
        _ => format!("a{}", i + 1),
    }
}

fn declarator_name(d: Node, src: &str) -> Option<String> {
    let mut cur = d;
    loop {
        match cur.kind() {
            "identifier" => return Some(src[cur.byte_range()].to_string()),
            "pointer_declarator" | "array_declarator" | "init_declarator" | "function_declarator"
            | "attributed_declarator" => cur = cur.child_by_field_name("declarator")?,
            "parenthesized_declarator" => cur = cur.named_child(0)?,
            _ => return None,
        }
    }
}

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

fn text_of(n: Node, src: &str) -> String {
    src[n.byte_range()].split_whitespace().collect::<Vec<_>>().join(" ")
}

fn line_start(src: &str, byte: usize) -> usize {
    src[..byte].rfind('\n').map_or(0, |i| i + 1)
}

/// Generate a DEFINITION for the prototype at `offset`.
///
/// The complement of [`plan`]: there the call exists and the function does not, here the
/// declaration exists and the body does not. This is the single most mechanical piece of typing in
/// C — copying a prototype from a header into a `.c` and turning `;` into `{ }` — and getting the
/// parameter names to match by hand is where it goes wrong.
///
/// Returns `None` when the prototype cannot be read, or when a definition already exists (`known`
/// is wired to the index, so re-generating over a real definition is impossible).
pub fn definition_for_declaration(
    src: &str,
    offset: usize,
    known: &dyn Fn(&str) -> bool,
) -> Option<StubPlan> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&tree_sitter_c::language()).ok()?;
    let tree = parser.parse(src, None)?;
    let root = tree.root_node();

    // A prototype is a `declaration` whose declarator is a function_declarator.
    let decl = enclosing(root, offset, "declaration")?;
    if decl.has_error() {
        return None;
    }
    let mut cur = decl.walk();
    let top = decl
        .children_by_field_name("declarator", &mut cur)
        .find(|d| unwrap_to_function(*d).is_some())?;
    let fd = unwrap_to_function(top)?;
    let name_node = fd.child_by_field_name("declarator").and_then(|d| innermost_identifier(d))?;
    let name = src[name_node.byte_range()].to_string();
    if known(&name) {
        return None;
    }
    let ret = decl.child_by_field_name("type").map(|t| text_of(t, src))?;
    let params = fd.child_by_field_name("parameters").map(|p| text_of(p, src))?;
    // Storage class travels with the DEFINITION too: a `static` prototype whose definition is
    // non-static is a different symbol, and the compiler will say so.
    let is_static = (0..decl.named_child_count())
        .filter_map(|i| decl.named_child(i))
        .any(|c| c.kind() == "storage_class_specifier" && &src[c.byte_range()] == "static");
    // `char *dup(...)` nests pointer_declarator ABOVE function_declarator, so the stars are on
    // the path DOWN to it — looking below the function declarator finds nothing.
    let stars = pointer_stars_above(top);

    let mut text = String::new();
    if is_static {
        text.push_str("static ");
    }
    text.push_str(&format!("{ret}{stars} "));
    let name_start = text.len();
    text.push_str(&name);
    text.push_str(&format!("{params}\n{{\n"));
    let ph_start = text.len();
    let void_ret = ret == "void" && stars.is_empty();
    let placeholder = "    /* TODO */";
    text.push_str(placeholder);
    if !void_ret {
        text.push_str("\n    return 0;");
    }
    text.push_str("\n}\n\n");
    let _ = name_start;

    Some(StubPlan {
        name,
        // At the END of the file: a definition placed above other code would sit between a header
        // block and the code that follows it, and there is no "right" neighbour to guess.
        insert_at: src.len(),
        text,
        body_placeholder: ph_start..ph_start + placeholder.len(),
        guessed_types: false,
    })
}

/// The `function_declarator` a prototype's declarator bottoms out in, if it is one.
fn unwrap_to_function(d: Node) -> Option<Node> {
    let mut cur = d;
    loop {
        match cur.kind() {
            "function_declarator" => return Some(cur),
            "pointer_declarator" | "parenthesized_declarator" | "attributed_declarator" => {
                cur = match cur.kind() {
                    "parenthesized_declarator" => cur.named_child(0)?,
                    _ => cur.child_by_field_name("declarator")?,
                };
            }
            _ => return None,
        }
    }
}

fn innermost_identifier(d: Node) -> Option<Node> {
    let mut cur = d;
    loop {
        match cur.kind() {
            "identifier" => return Some(cur),
            "parenthesized_declarator" => cur = cur.named_child(0)?,
            _ => cur = cur.child_by_field_name("declarator")?,
        }
    }
}

/// The stars between the return type and the name (`char *strdup(...)` -> ` *`), counted from the
/// declaration's own declarator down to the function declarator.
fn pointer_stars_above(top: Node) -> String {
    let mut stars = String::new();
    let mut cur = top;
    loop {
        match cur.kind() {
            "function_declarator" => break,
            "pointer_declarator" => {
                stars.push('*');
                match cur.child_by_field_name("declarator") {
                    Some(n) => cur = n,
                    None => break,
                }
            }
            "parenthesized_declarator" => match cur.named_child(0) {
                Some(n) => cur = n,
                None => break,
            },
            "attributed_declarator" => match cur.child_by_field_name("declarator") {
                Some(n) => cur = n,
                None => break,
            },
            _ => break,
        }
    }
    match stars.is_empty() {
        true => String::new(),
        false => format!(" {stars}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none(_: &str) -> bool {
        false
    }

    fn at(src: &str, needle: &str) -> usize {
        src.find(needle).expect("fixture")
    }

    #[test]
    fn infers_types_from_literal_arguments() {
        let src = "void run(void)\n{\n    report(\"hi\", 3, 1.5);\n}\n";
        let p = plan(src, at(src, "report"), &none).unwrap();
        assert!(
            p.text.contains("static void report(const char *a1, int a2, double a3)"),
            "{}",
            p.text
        );
        assert!(!p.guessed_types, "every argument here is unambiguous");
    }

    #[test]
    fn reuses_variable_names_and_their_declared_types() {
        let src = "void run(void)\n{\n    char *msg = 0;\n    int count = 0;\n    emit(msg, count);\n}\n";
        let p = plan(src, at(src, "emit"), &none).unwrap();
        assert!(p.text.contains("emit(char *msg, int count)"), "{}", p.text);
    }

    #[test]
    fn return_type_comes_from_the_context_not_the_call() {
        let stmt = "void run(void)\n{\n    doit(1);\n}\n";
        assert!(plan(stmt, at(stmt, "doit"), &none).unwrap().text.contains("static void doit"));

        let assigned = "void run(void)\n{\n    long v = measure(1);\n}\n";
        let p = plan(assigned, at(assigned, "measure"), &none).unwrap();
        assert!(p.text.contains("static long measure"), "{}", p.text);
    }

    #[test]
    fn a_void_stub_has_no_return_but_a_valued_one_does() {
        let stmt = "void run(void)\n{\n    doit();\n}\n";
        let p = plan(stmt, at(stmt, "doit"), &none).unwrap();
        assert!(!p.text.contains("return"), "{}", p.text);

        let val = "void run(void)\n{\n    int x = calc();\n}\n";
        let p = plan(val, at(val, "calc"), &none).unwrap();
        assert!(p.text.contains("return 0;"), "{}", p.text);
    }

    #[test]
    fn address_of_becomes_a_pointer_parameter() {
        let src = "void run(void)\n{\n    int n = 0;\n    fill(&n);\n}\n";
        let p = plan(src, at(src, "fill"), &none).unwrap();
        assert!(p.text.contains("fill(int *n)"), "{}", p.text);
    }

    #[test]
    fn an_unreadable_argument_is_guessed_and_says_so() {
        let src = "void run(void)\n{\n    handle(mystery());\n}\n";
        let p = plan(src, at(src, "handle"), &none).unwrap();
        assert!(p.guessed_types, "an unknown call's type cannot be known");
        assert!(p.text.contains("TODO: check the generated parameter types"), "{}", p.text);
    }

    // --- generate definition from declaration -------------------------------------------------

    #[test]
    fn generates_a_definition_matching_the_prototype() {
        let src = "int add(int a, int b);\n";
        let p = definition_for_declaration(src, at(src, "add"), &none).unwrap();
        assert!(p.text.starts_with("int add(int a, int b)\n{\n"), "{}", p.text);
        assert!(p.text.contains("return 0;"), "a non-void definition needs a return: {}", p.text);
        assert_eq!(p.insert_at, src.len(), "appended, not wedged between existing code");
    }

    #[test]
    fn a_void_prototype_gets_no_return() {
        let src = "void reset(void);\n";
        let p = definition_for_declaration(src, at(src, "reset"), &none).unwrap();
        assert!(!p.text.contains("return"), "{}", p.text);
        assert!(p.text.contains("void reset(void)"), "{}", p.text);
    }

    #[test]
    fn static_travels_to_the_definition() {
        // A static prototype whose definition is non-static is a DIFFERENT symbol, and the
        // compiler says so.
        let src = "static int helper(int n);\n";
        let p = definition_for_declaration(src, at(src, "helper"), &none).unwrap();
        assert!(p.text.starts_with("static int helper(int n)"), "{}", p.text);
    }

    #[test]
    fn a_pointer_return_keeps_its_stars() {
        let src = "char *dup_name(const char *s);\n";
        let p = definition_for_declaration(src, at(src, "dup_name"), &none).unwrap();
        assert!(p.text.starts_with("char * dup_name(const char *s)"), "{}", p.text);
    }

    #[test]
    fn declines_when_a_definition_already_exists() {
        let src = "int add(int a, int b);\n";
        assert!(definition_for_declaration(src, at(src, "add"), &|n| n == "add").is_none());
    }

    #[test]
    fn declines_on_a_variable_declaration() {
        // `int x;` is not a prototype and must not become a function.
        let src = "int x;\n";
        assert!(definition_for_declaration(src, at(src, "x"), &none).is_none());
    }

    // --- regressions found by adversarial review ---------------------------------------------

    #[test]
    fn pointer_return_types_survive() {
        // Was: `char *p = f();` generated `static char f(...)`, which cannot be assigned.
        let src = "void run(void)\n{\n    char *p = grab();\n}\n";
        let p = plan(src, at(src, "grab"), &none).unwrap();
        assert!(p.text.contains("static char * grab") || p.text.contains("static char *grab"), "{}", p.text);
    }

    #[test]
    fn duplicate_argument_names_are_deduplicated() {
        // Was: `f(n, n)` emitted two parameters called `n` — does not compile.
        let src = "void run(void)\n{\n    int n = 0;\n    f(n, n);\n}\n";
        let p = plan(src, at(src, "f(n"), &none).unwrap();
        let sig = p.text.lines().find(|l| l.contains("f(")).unwrap();
        let names: Vec<&str> = sig
            .split('(')
            .nth(1)
            .unwrap()
            .trim_end_matches(')')
            .split(',')
            .map(|x| x.trim().rsplit([' ', '*']).next().unwrap())
            .collect();
        assert_eq!(names.len(), 2);
        assert_ne!(names[0], names[1], "duplicate parameter names: {sig}");
    }

    #[test]
    fn a_hex_literal_ending_in_f_is_an_int() {
        // Was: `0x1f` classified as a double because it ends in `f`.
        let src = "void run(void)\n{\n    mask(0x1f);\n}\n";
        let p = plan(src, at(src, "mask"), &none).unwrap();
        assert!(p.text.contains("mask(int a1)"), "{}", p.text);
    }

    #[test]
    fn a_shadowed_argument_type_is_marked_as_a_guess() {
        // Was: the FIRST declaration anywhere in the function won, silently, even when another
        // declaration of the same name is the one actually in scope at the call.
        let src = "void run(void)\n{\n    {\n        char v = 0;\n    }\n    long v = 0;\n    take(v);\n}\n";
        let p = plan(src, at(src, "take"), &none).unwrap();
        assert!(p.guessed_types, "an ambiguous name must not be reported as certain: {}", p.text);
    }

    #[test]
    fn an_error_elsewhere_in_the_file_does_not_disable_the_action() {
        // Was: has_error was checked on the whole file, so one broken function (or C++ in the
        // same header) silently disabled create-from-usage everywhere.
        let src = "void broken( ;\n\nvoid run(void)\n{\n    helper(1);\n}\n";
        assert!(plan(src, at(src, "helper"), &none).is_some(), "a clean host must still work");
    }

    #[test]
    fn declines_when_the_function_already_exists() {
        let src = "void run(void)\n{\n    existing(1);\n}\n";
        assert!(plan(src, at(src, "existing"), &|n| n == "existing").is_none());
    }

    #[test]
    fn declines_on_a_call_through_a_pointer() {
        let src = "void run(void)\n{\n    struct O o;\n    o.fp(1);\n}\n";
        assert!(plan(src, at(src, "o.fp"), &none).is_none(), "no name to define");
    }

    #[test]
    fn declines_when_the_file_does_not_parse() {
        let src = "void run(void) {\n    broken( ;\n";
        assert!(plan(src, at(src, "broken"), &none).is_none());
    }

    #[test]
    fn the_stub_goes_above_its_caller() {
        let src = "void run(void)\n{\n    helper();\n}\n";
        let p = plan(src, at(src, "helper"), &none).unwrap();
        assert_eq!(p.insert_at, 0, "declared before use — no prototype needed");
        assert_eq!(&p.text[p.body_placeholder.clone()], "    /* TODO */");
    }
}
