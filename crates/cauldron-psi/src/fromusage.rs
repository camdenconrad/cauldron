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
    if root.has_error() {
        // Same reasoning as extract: a recovery parse makes every inference below fiction.
        return None;
    }

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
    let insert_at = line_start(src, host.start_byte());

    let args = call.child_by_field_name("arguments")?;
    let mut cur = args.walk();
    let arg_nodes: Vec<Node> = args.named_children(&mut cur).collect();

    let mut guessed = false;
    let mut params: Vec<String> = Vec::new();
    for (i, a) in arg_nodes.iter().enumerate() {
        let (ty, sure) = argument_type(*a, src, host);
        if !sure {
            guessed = true;
        }
        // `char *p`, not `char * p` — the star binds to the declarator in C, and every C
        // codebase writes it that way.
        let sep = if ty.ends_with('*') { "" } else { " " };
        params.push(format!("{ty}{sep}{}", param_name(*a, src, i)));
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
            let float = t.contains('.') || t.ends_with('f') || t.ends_with('F');
            (if float { "double" } else { "int" }.to_string(), true)
        }
        "string_literal" => ("const char *".to_string(), true),
        "char_literal" => ("char".to_string(), true),
        "true" | "false" => ("int".to_string(), true),
        "identifier" => {
            let name = &src[a.byte_range()];
            match declared_type_in(host, src, name) {
                Some(ty) => (ty, true),
                None => ("int".to_string(), false),
            }
        }
        "pointer_expression" => {
            // `&x` — a pointer to whatever x is.
            let inner = a.named_child(0);
            match inner.map(|i| argument_type(i, src, host)) {
                Some((ty, sure)) if src[a.byte_range()].starts_with('&') => {
                    (format!("{ty} *"), sure)
                }
                Some((ty, sure)) => (ty, sure),
                None => ("int".to_string(), false),
            }
        }
        "cast_expression" => match a.child_by_field_name("type") {
            Some(t) => (src[t.byte_range()].split_whitespace().collect::<Vec<_>>().join(" "), true),
            None => ("int".to_string(), false),
        },
        _ => ("int".to_string(), false),
    }
}

/// The declared type of local `name` inside `host`, if a declaration or parameter gives one.
fn declared_type_in(host: Node, src: &str, name: &str) -> Option<String> {
    fn search(n: Node, src: &str, name: &str, out: &mut Option<String>) {
        if out.is_some() {
            return;
        }
        if n.kind() == "declaration" || n.kind() == "parameter_declaration" {
            let ty = n.child_by_field_name("type").map(|t| text_of(t, src));
            let mut cur = n.walk();
            for d in n.children_by_field_name("declarator", &mut cur) {
                if declarator_name(d, src).as_deref() == Some(name) {
                    if let Some(ty) = &ty {
                        *out = Some(format!("{ty}{}", pointer_suffix(d)));
                        return;
                    }
                }
            }
        }
        let mut cur = n.walk();
        for ch in n.named_children(&mut cur) {
            search(ch, src, name, out);
        }
    }
    let mut out = None;
    search(host, src, name, &mut out);
    out
}

/// The return type implied by what the call's RESULT is used for.
fn return_type(call: Node, src: &str) -> String {
    let Some(parent) = call.parent() else { return "void".into() };
    match parent.kind() {
        // `f();` as a statement — nothing wants a value.
        "expression_statement" => "void".to_string(),
        // `int x = f();` — the declaration's own type.
        "init_declarator" => parent
            .parent()
            .and_then(|d| d.child_by_field_name("type"))
            .map(|t| text_of(t, src))
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
