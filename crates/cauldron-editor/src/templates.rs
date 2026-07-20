//! Live templates and postfix completion.
//!
//! Two shapes of the same idea — type a short thing, get a long thing with the caret already where
//! you would have put it:
//!
//! * **Live template**: an abbreviation on its own (`for`, `main`, `test`) expands into a
//!   construct. Triggered by Tab, JetBrains-style.
//! * **Postfix**: an expression followed by `.name` (`x.if`, `p.ret`) rewrites the expression it
//!   is attached to. This is the one that changes how it feels to type, because the expression
//!   comes first and you decide what to do with it afterwards — the order you actually think in.
//!
//! Bodies are LSP snippet syntax (`${1:placeholder}`, `$0`), which the editor's existing
//! [`crate::snippet`] engine already expands and steps through, so nothing new is needed to drive
//! the tab stops.

use crate::syntax::Lang;

/// One abbreviation and the snippet it expands to.
pub struct Template {
    pub key: &'static str,
    /// Shown next to the key when listing templates.
    pub about: &'static str,
    pub body: &'static str,
}

/// Live templates for `lang`, matched against the word before the caret.
pub fn live_templates(lang: Option<Lang>) -> &'static [Template] {
    match lang {
        Some(Lang::C) | Some(Lang::Cpp) => C_TEMPLATES,
        Some(Lang::Rust) => RUST_TEMPLATES,
        Some(Lang::Python) => PY_TEMPLATES,
        _ => &[],
    }
}

/// Postfix templates for `lang`. `$E` in the body stands for the expression the postfix was
/// attached to — the one thing a plain snippet cannot express.
pub fn postfix_templates(lang: Option<Lang>) -> &'static [Template] {
    match lang {
        Some(Lang::C) | Some(Lang::Cpp) => C_POSTFIX,
        Some(Lang::Rust) => RUST_POSTFIX,
        Some(Lang::Python) => PY_POSTFIX,
        _ => &[],
    }
}

static C_TEMPLATES: &[Template] = &[
    Template {
        key: "for",
        about: "counted loop",
        body: "for (int ${1:i} = 0; ${1:i} < ${2:n}; ${1:i}++) {\n    $0\n}",
    },
    Template { key: "while", about: "while loop", body: "while (${1:cond}) {\n    $0\n}" },
    Template { key: "if", about: "if", body: "if (${1:cond}) {\n    $0\n}" },
    Template {
        key: "ife",
        about: "if / else",
        body: "if (${1:cond}) {\n    $0\n} else {\n}",
    },
    Template {
        key: "sw",
        about: "switch",
        body: "switch (${1:x}) {\ncase ${2:v}:\n    $0\n    break;\ndefault:\n    break;\n}",
    },
    Template { key: "st", about: "struct typedef", body: "typedef struct {\n    $0\n} ${1:Name};" },
    Template {
        key: "main",
        about: "main()",
        body: "int main(int argc, char **argv)\n{\n    $0\n    return 0;\n}",
    },
    Template { key: "pr", about: "printf", body: "printf(\"${1:%s}\\n\", $0);" },
    Template {
        key: "guard",
        about: "include guard",
        body: "#ifndef ${1:NAME_H}\n#define ${1:NAME_H}\n\n$0\n\n#endif /* ${1:NAME_H} */",
    },
];

static RUST_TEMPLATES: &[Template] = &[
    Template { key: "fn", about: "function", body: "fn ${1:name}($2) {\n    $0\n}" },
    Template { key: "pfn", about: "pub function", body: "pub fn ${1:name}($2) {\n    $0\n}" },
    Template { key: "st", about: "struct", body: "struct ${1:Name} {\n    $0\n}" },
    Template { key: "impl", about: "impl block", body: "impl ${1:Type} {\n    $0\n}" },
    Template {
        key: "match",
        about: "match",
        body: "match ${1:expr} {\n    ${2:pat} => $0,\n}",
    },
    Template { key: "for", about: "for loop", body: "for ${1:x} in ${2:iter} {\n    $0\n}" },
    Template {
        key: "test",
        about: "unit test",
        body: "#[test]\nfn ${1:name}() {\n    $0\n}",
    },
    Template {
        key: "derive",
        about: "derive attribute",
        body: "#[derive(${1:Debug, Clone})]",
    },
];

static PY_TEMPLATES: &[Template] = &[
    Template { key: "def", about: "function", body: "def ${1:name}($2):\n    $0" },
    Template { key: "for", about: "for loop", body: "for ${1:x} in ${2:it}:\n    $0" },
    Template { key: "class", about: "class", body: "class ${1:Name}:\n    $0" },
    Template { key: "main", about: "main guard", body: "if __name__ == \"__main__\":\n    $0" },
];

static C_POSTFIX: &[Template] = &[
    Template { key: "if", about: "if (expr)", body: "if ($E) {\n    $0\n}" },
    Template { key: "not", about: "!expr", body: "!($E)$0" },
    Template { key: "ret", about: "return expr;", body: "return $E;$0" },
    Template { key: "par", about: "(expr)", body: "($E)$0" },
    Template {
        key: "for",
        about: "counted loop over expr",
        body: "for (int ${1:i} = 0; ${1:i} < $E; ${1:i}++) {\n    $0\n}",
    },
    Template { key: "null", about: "NULL check", body: "if ($E == NULL) {\n    $0\n}" },
];

static RUST_POSTFIX: &[Template] = &[
    Template { key: "if", about: "if expr", body: "if $E {\n    $0\n}" },
    Template { key: "not", about: "!expr", body: "!($E)$0" },
    Template { key: "ret", about: "return expr;", body: "return $E;$0" },
    Template { key: "let", about: "let binding", body: "let ${1:name} = $E;$0" },
    Template { key: "match", about: "match expr", body: "match $E {\n    ${1:pat} => $0,\n}" },
    Template { key: "some", about: "Some(expr)", body: "Some($E)$0" },
    Template { key: "ok", about: "Ok(expr)", body: "Ok($E)$0" },
    Template { key: "dbg", about: "dbg!(expr)", body: "dbg!($E)$0" },
    Template { key: "iter", about: "for loop over expr", body: "for ${1:x} in $E {\n    $0\n}" },
];

static PY_POSTFIX: &[Template] = &[
    Template { key: "if", about: "if expr:", body: "if $E:\n    $0" },
    Template { key: "not", about: "not expr", body: "not ($E)$0" },
    Template { key: "ret", about: "return expr", body: "return $E$0" },
    Template { key: "print", about: "print(expr)", body: "print($E)$0" },
];

/// A postfix expansion resolved against the text before the caret.
#[derive(Debug, PartialEq, Eq)]
pub struct PostfixHit {
    /// Byte range to replace: the expression, the dot, and the key.
    pub replace: std::ops::Range<usize>,
    /// Snippet body with `$E` already substituted.
    pub snippet: String,
}

/// Resolve a postfix template at `caret`, if the text before it looks like `<expr>.<key>`.
///
/// The expression is found by scanning LEFT from the dot, balancing brackets so `f(a, b).if` and
/// `arr[i].ret` take the whole call and the whole index — stopping at the first operator or
/// separator outside brackets. This is deliberately syntactic rather than parsed: it must work
/// mid-edit, when the line is not yet valid code.
pub fn resolve_postfix(text: &str, caret: usize, lang: Option<Lang>) -> Option<PostfixHit> {
    let templates = postfix_templates(lang);
    if templates.is_empty() {
        return None;
    }
    let before = &text[..caret.min(text.len())];
    // The key: word characters immediately before the caret.
    let key_start = before
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
        .last()
        .map(|(i, _)| i)?;
    let key = &before[key_start..];
    if key.is_empty() {
        return None;
    }
    let tmpl = templates.iter().find(|t| t.key == key)?;
    // The dot immediately before the key.
    let dot = before[..key_start].strip_suffix('.')?;
    let expr = expression_before(dot)?;
    if expr.is_empty() {
        return None;
    }
    let expr_start = dot.len() - expr.len();
    Some(PostfixHit {
        replace: expr_start..caret,
        snippet: tmpl.body.replace("$E", expr),
    })
}

/// The expression ending at the end of `s`, scanning left with bracket balancing.
fn expression_before(s: &str) -> Option<&str> {
    let b = s.as_bytes();
    let mut i = s.len();
    let mut depth = 0i32;
    while i > 0 {
        let c = b[i - 1];
        match c {
            b')' | b']' => depth += 1,
            b'(' | b'[' => {
                if depth == 0 {
                    break; // an unbalanced opener: the expression starts after it
                }
                depth -= 1;
            }
            // Inside brackets everything belongs to the expression, including commas and spaces.
            _ if depth > 0 => {}
            c if c.is_ascii_alphanumeric() || c == b'_' || c == b'.' || c == b'-' && i >= 2 && b[i - 2] == b'-' => {}
            b'>' if i >= 2 && b[i - 2] == b'-' => {} // `->`
            b'-' if i < s.len() && b[i] == b'>' => {}
            _ => break,
        }
        i -= 1;
    }
    let expr = s[i..].trim();
    (!expr.is_empty()).then_some(expr)
}

/// Resolve a live template from the word immediately before `caret`.
pub fn resolve_live(text: &str, caret: usize, lang: Option<Lang>) -> Option<(std::ops::Range<usize>, &'static str)> {
    let templates = live_templates(lang);
    if templates.is_empty() {
        return None;
    }
    let before = &text[..caret.min(text.len())];
    let start = before
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
        .last()
        .map(|(i, _)| i)?;
    let word = &before[start..];
    // A template must be the WHOLE word: `format` must not fire the `for` template.
    let t = templates.iter().find(|t| t.key == word)?;
    Some((start..caret, t.body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_template_needs_the_whole_word() {
        assert!(resolve_live("for", 3, Some(Lang::C)).is_some());
        // `format` merely STARTS with `for` — expanding here would corrupt an identifier.
        assert!(resolve_live("format", 6, Some(Lang::C)).is_none());
        assert!(resolve_live("xfor", 4, Some(Lang::C)).is_none());
    }

    #[test]
    fn live_templates_are_language_scoped() {
        // `impl` is a Rust template and means nothing in C.
        assert!(resolve_live("impl", 4, Some(Lang::Rust)).is_some());
        assert!(resolve_live("impl", 4, Some(Lang::C)).is_none());
        assert!(resolve_live("for", 3, None).is_none(), "no language, no templates");
    }

    #[test]
    fn postfix_takes_the_whole_call_expression() {
        let src = "    compute(a, b).if";
        let hit = resolve_postfix(src, src.len(), Some(Lang::C)).unwrap();
        assert_eq!(&src[hit.replace.clone()], "compute(a, b).if");
        assert!(hit.snippet.starts_with("if (compute(a, b))"), "{}", hit.snippet);
    }

    #[test]
    fn postfix_takes_the_whole_index_expression() {
        let src = "arr[i + 1].ret";
        let hit = resolve_postfix(src, src.len(), Some(Lang::C)).unwrap();
        assert_eq!(hit.snippet, "return arr[i + 1];$0");
    }

    #[test]
    fn postfix_stops_at_an_operator() {
        // Only `b` is the expression; `a + ` is not part of it.
        let src = "a + b.not";
        let hit = resolve_postfix(src, src.len(), Some(Lang::C)).unwrap();
        assert_eq!(&src[hit.replace.clone()], "b.not");
        assert_eq!(hit.snippet, "!(b)$0");
    }

    #[test]
    fn postfix_follows_member_access() {
        let src = "cfg.limits.max.ret";
        let hit = resolve_postfix(src, src.len(), Some(Lang::C)).unwrap();
        assert_eq!(hit.snippet, "return cfg.limits.max;$0");
    }

    #[test]
    fn postfix_needs_a_real_expression() {
        assert!(resolve_postfix(".if", 3, Some(Lang::C)).is_none(), "nothing before the dot");
        assert!(resolve_postfix("x.nosuch", 8, Some(Lang::C)).is_none(), "unknown key");
    }

    #[test]
    fn postfix_is_language_scoped() {
        assert!(resolve_postfix("x.some", 6, Some(Lang::Rust)).is_some());
        assert!(resolve_postfix("x.some", 6, Some(Lang::C)).is_none());
    }

    /// Every body must be parseable by the snippet engine, or a template would insert literal
    /// `${1:...}` text into the user's file.
    #[test]
    fn every_template_body_parses_as_a_snippet() {
        for lang in [Some(Lang::C), Some(Lang::Rust), Some(Lang::Python)] {
            for t in live_templates(lang).iter().chain(postfix_templates(lang)) {
                let (plain, _stops) = crate::snippet::parse(&t.body.replace("$E", "expr"));
                assert!(!plain.contains("${"), "unexpanded placeholder in `{}`: {plain}", t.key);
            }
        }
    }
}
