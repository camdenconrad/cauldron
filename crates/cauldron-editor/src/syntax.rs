//! Persistent per-buffer tree-sitter tree with incremental reparse.
//!
//! The tree survives across edits: each [`Transaction`] change becomes a `tree_sitter::InputEdit`
//! (positions derived via [`crate::position`]), then `Parser::parse` reuses the old tree so a
//! keystroke reparses only the damaged region — this is what keeps Gate A's CPU budget honest on
//! a 5k-line cFS file.

use std::ops::Range;

use ropey::Rope;
use tree_sitter::{InputEdit, Language, Parser, Point as TsPoint, Tree};

use crate::buffer::Change;
use crate::position::byte_to_point;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    C,
    Cpp,
    Rust,
    Python,
    Js,
    Ts,
    Tsx,
    Css,
    Html,
    CSharp,
    Json,
    Yaml,
    Java,
}

impl Lang {
    pub fn from_path(path: &str) -> Option<Self> {
        let ext = path.rsplit('.').next()?;
        match ext {
            "c" | "h" => Some(Lang::C),
            "cc" | "cpp" | "cxx" | "hpp" | "hh" => Some(Lang::Cpp),
            "rs" => Some(Lang::Rust),
            "py" => Some(Lang::Python),
            "js" | "mjs" | "cjs" | "jsx" => Some(Lang::Js),
            "ts" | "mts" => Some(Lang::Ts),
            "tsx" => Some(Lang::Tsx),
            // .scss is best-effort: the CSS grammar parses the shared subset fine and degrades
            // gracefully on SCSS-only syntax (nested rules mostly still highlight).
            "css" | "scss" => Some(Lang::Css),
            "html" | "htm" => Some(Lang::Html),
            "cs" => Some(Lang::CSharp),
            "json" | "jsonc" => Some(Lang::Json),
            "yaml" | "yml" => Some(Lang::Yaml),
            "java" => Some(Lang::Java),
            _ => None,
        }
    }

    /// The line-comment token, if the language has one. Drives Ctrl+/ (comment toggle). CSS and
    /// HTML have no line comment — they fall back to their block comment.
    pub fn line_comment(self) -> Option<&'static str> {
        match self {
            Lang::C | Lang::Cpp | Lang::Rust | Lang::Js | Lang::Ts | Lang::Tsx | Lang::CSharp
            | Lang::Java => Some("//"),
            Lang::Python => Some("#"),
            // JSONC tolerates //; strict JSON doesn't, but the toggle is user-initiated.
            Lang::Json => Some("//"),
            Lang::Yaml => Some("#"),
            Lang::Css | Lang::Html => None,
        }
    }

    /// Node kinds whose CONTENTS (between the delimiters) are a meaningful selection step, so
    /// Ctrl+W inside a string selects the text before the quotes.
    pub fn string_kinds(self) -> &'static [&'static str] {
        match self {
            Lang::Rust => &["string_literal", "raw_string_literal", "char_literal"],
            Lang::C | Lang::Cpp => &["string_literal", "char_literal"],
            Lang::Python => &["string"],
            Lang::Js | Lang::Ts | Lang::Tsx => &["string", "template_string"],
            _ => &["string", "string_literal"],
        }
    }

    /// Delimited lists whose INTERIOR is an intermediate step, so a selection never stops on a
    /// range that includes the opening paren but not the closing one.
    pub fn list_kinds(self) -> &'static [&'static str] {
        match self {
            Lang::Rust => &["arguments", "parameters", "field_declaration_list", "tuple_expression"],
            Lang::C | Lang::Cpp => &["argument_list", "parameter_list", "initializer_list"],
            Lang::Python => &["argument_list", "parameters", "list", "tuple", "dictionary"],
            Lang::Js | Lang::Ts | Lang::Tsx => &["arguments", "formal_parameters", "array", "object"],
            _ => &["argument_list", "arguments", "parameter_list"],
        }
    }

    /// Does this language use braces to delimit blocks? Drives auto-indent: a language that
    /// indents by other means (Python, YAML) must never have brace rules applied to it.
    pub fn brace_indented(self) -> bool {
        matches!(
            self,
            Lang::C
                | Lang::Cpp
                | Lang::Rust
                | Lang::Js
                | Lang::Ts
                | Lang::Tsx
                | Lang::CSharp
                | Lang::Java
                | Lang::Css
                | Lang::Json
        )
    }

    /// Is `'` a STRING delimiter here, or a character/lifetime marker? Getting this backwards
    /// corrupts any line scan: in JS `'{'` is a string, while in Rust `'a` is a lifetime that
    /// never closes and in C `'{'` is a one-character literal.
    pub fn single_quote_is_string(self) -> bool {
        matches!(self, Lang::Js | Lang::Ts | Lang::Tsx | Lang::Python | Lang::Css | Lang::Yaml)
    }

    /// The block-comment delimiters `(open, close)`, if any. Drives Ctrl+Shift+/ and the Ctrl+/
    /// fallback for languages with no line comment.
    pub fn block_comment(self) -> Option<(&'static str, &'static str)> {
        match self {
            Lang::C
            | Lang::Cpp
            | Lang::Rust
            | Lang::Js
            | Lang::Ts
            | Lang::Tsx
            | Lang::Css
            | Lang::CSharp
            | Lang::Java => Some(("/*", "*/")),
            Lang::Html => Some(("<!--", "-->")),
            Lang::Python | Lang::Json | Lang::Yaml => None,
        }
    }

    fn language(self) -> Language {
        match self {
            Lang::C => tree_sitter_c::language(),
            Lang::Cpp => tree_sitter_cpp::language(),
            Lang::Rust => tree_sitter_rust::language(),
            Lang::Python => tree_sitter_python::language(),
            Lang::Js => tree_sitter_javascript::language(),
            Lang::Ts => tree_sitter_typescript::language_typescript(),
            Lang::Tsx => tree_sitter_typescript::language_tsx(),
            Lang::Css => tree_sitter_css::language(),
            Lang::Html => tree_sitter_html::language(),
            Lang::CSharp => tree_sitter_c_sharp::language(),
            Lang::Json => tree_sitter_json::language(),
            Lang::Yaml => tree_sitter_yaml::language(),
            Lang::Java => tree_sitter_java::language(),
        }
    }
}

/// One level of the scope chain at a byte: the enclosing named construct.
#[derive(Debug, Clone, PartialEq)]
pub struct ScopeCrumb {
    /// Display name (identifier of the fn/struct/class/impl…).
    pub name: String,
    /// Byte offset of the construct's definition (jump target).
    pub def_byte: usize,
    /// Byte where the construct's body ends.
    pub end_byte: usize,
}

/// Node kinds that form breadcrumb scopes, per grammar family. The name is taken from the
/// node's `name`/`declarator` field (C declarators recurse to the identifier).
fn scope_kinds(kind: &str) -> bool {
    matches!(
        kind,
        // rust
        "function_item" | "impl_item" | "struct_item" | "enum_item" | "trait_item" | "mod_item"
        // c / c++
        | "function_definition" | "struct_specifier" | "enum_specifier" | "union_specifier"
        // python
        | "function_definition_py" | "class_definition" | "decorated_definition"
        // js / ts
        | "function_declaration" | "method_definition" | "class_declaration"
        | "arrow_function" | "interface_declaration"
        // java
        | "class_declaration" | "method_declaration" | "interface_declaration"
        | "enum_declaration" | "constructor_declaration"
    )
}

pub struct Syntax {
    parser: Parser,
    pub tree: Tree,
}

impl Syntax {
    pub fn new(lang: Lang, rope: &Rope) -> Option<Self> {
        let mut parser = Parser::new();
        parser.set_language(&lang.language()).ok()?;
        let tree = parse_rope(&mut parser, rope, None)?;
        Some(Self { parser, tree })
    }

    /// Register the edits of an applied transaction, then incrementally reparse.
    /// `rope` is the buffer AFTER the transaction; `changes` are its (pre-apply) changes.
    ///
    /// Edits are registered FRONT-TO-BACK with each one shifted by the cumulative byte delta of
    /// the changes before it — i.e. in "earlier changes already applied" coordinates. In that
    /// space the final rope's content below each shifted start equals the at-time content, so
    /// `byte_to_point` against the final rope is exact AND in bounds even when a multi-change
    /// transaction shrinks the buffer (two-caret backspace, block unindent — the naive pre-edit
    /// offsets would index past the post-edit rope and panic).
    /// Scope chain (outermost → innermost) enclosing `byte` — breadcrumbs + sticky headers.
    /// The smallest syntax node that STRICTLY contains `range` on at least one side — one rung of
    /// Ctrl+W. Ancestors whose span equals `range` are skipped: several grammars wrap a node in a
    /// same-extent parent, and stopping there would make the keystroke look dead.
    pub fn enclosing_range(&self, range: Range<usize>) -> Option<Range<usize>> {
        let mut cur = self
            .tree
            .root_node()
            .named_descendant_for_byte_range(range.start, range.end.max(range.start))?;
        loop {
            let r = cur.start_byte()..cur.end_byte();
            if r.start < range.start || r.end > range.end {
                return Some(r);
            }
            cur = cur.parent()?;
        }
    }

    /// The innermost node at `byte` whose kind is one of `kinds`, as a byte range.
    pub fn innermost_of_kind(&self, byte: usize, kinds: &[&str]) -> Option<Range<usize>> {
        let mut cur = self.tree.root_node().named_descendant_for_byte_range(byte, byte);
        while let Some(n) = cur {
            if kinds.contains(&n.kind()) {
                return Some(n.start_byte()..n.end_byte());
            }
            cur = n.parent();
        }
        None
    }

    pub fn scopes_at(&self, rope: &Rope, byte: usize) -> Vec<ScopeCrumb> {
        let mut out = Vec::new();
        // Walk up from the deepest node at `byte`, collecting scope-forming ancestors.
        let mut cur = self.tree.root_node().named_descendant_for_byte_range(byte, byte);
        while let Some(n) = cur {
            if scope_kinds(n.kind()) {
                if let Some(name) = scope_name(n, rope) {
                    out.push(ScopeCrumb { name, def_byte: n.start_byte(), end_byte: n.end_byte() });
                }
            }
            cur = n.parent();
        }
        out.reverse();
        out.dedup();
        out
    }

    pub fn edited(&mut self, rope: &Rope, changes: &[Change]) {
        let mut delta: isize = 0;
        for ch in changes {
            let start_byte = (ch.start as isize + delta) as usize;
            let old_end_byte = (ch.end as isize + delta) as usize;
            let new_end_byte = start_byte + ch.text.len();
            let start = byte_to_point(rope, start_byte);
            let new_end = byte_to_point(rope, new_end_byte);
            self.tree.edit(&InputEdit {
                start_byte,
                old_end_byte,
                new_end_byte,
                start_position: TsPoint::new(start.line, start.col),
                // old_end_position is in a text state we no longer have; tree-sitter tolerates an
                // approximate old_end_position as long as the bytes are exact.
                old_end_position: TsPoint::new(start.line, start.col + (ch.end - ch.start)),
                new_end_position: TsPoint::new(new_end.line, new_end.col),
            });
            delta += ch.text.len() as isize - (ch.end - ch.start) as isize;
        }
        if let Some(t) = parse_rope(&mut self.parser, rope, Some(&self.tree)) {
            self.tree = t;
        }
    }
}

fn parse_rope(parser: &mut Parser, rope: &Rope, old: Option<&Tree>) -> Option<Tree> {
    parser.parse_with(
        &mut |byte, _| {
            if byte >= rope.len_bytes() {
                return &[] as &[u8];
            }
            let (chunk, chunk_start, _, _) = rope.chunk_at_byte(byte);
            &chunk.as_bytes()[byte - chunk_start..]
        },
        old,
    )
}

/// Best-effort display name for a scope node: its `name` field, or the identifier buried in a
/// C `declarator` chain, or the type of an impl block.
fn scope_name(node: tree_sitter::Node, rope: &Rope) -> Option<String> {
    let text = |n: tree_sitter::Node| -> String {
        rope.byte_slice(n.start_byte().min(rope.len_bytes())..n.end_byte().min(rope.len_bytes()))
            .to_string()
    };
    if let Some(n) = node.child_by_field_name("name") {
        return Some(text(n));
    }
    if node.kind() == "impl_item" {
        if let Some(t) = node.child_by_field_name("type") {
            return Some(format!("impl {}", text(t)));
        }
    }
    // C: function_definition → declarator → … → identifier
    if let Some(mut d) = node.child_by_field_name("declarator") {
        loop {
            if d.kind() == "identifier" || d.kind() == "field_identifier" {
                return Some(text(d));
            }
            match d.child_by_field_name("declarator") {
                Some(inner) => d = inner,
                None => break,
            }
        }
        // fall back to the first identifier inside the declarator
        let mut walker = d.walk();
        for c in d.children(&mut walker) {
            if c.kind() == "identifier" {
                return Some(text(c));
            }
        }
    }
    None
}

#[cfg(test)]
mod scope_tests {
    use super::*;
    use std::ops::Range;

use ropey::Rope;

    fn scopes(lang: Lang, src: &str, at: &str) -> Vec<String> {
        let rope = Rope::from_str(src);
        let syn = Syntax::new(lang, &rope).unwrap();
        let byte = src.find(at).unwrap();
        syn.scopes_at(&rope, byte).into_iter().map(|c| c.name).collect()
    }

    #[test]
    fn rust_scope_chain() {
        let src = "mod util {\n    impl Foo {\n        fn bar(&self) {\n            let x = 1;\n        }\n    }\n}\n";
        assert_eq!(scopes(Lang::Rust, src, "let x"), vec!["util", "impl Foo", "bar"]);
    }

    #[test]
    fn c_function_scope() {
        let src = "int32 CFE_ES_Main(uint32 StartType)\n{\n    int32 Status;\n    return Status;\n}\n";
        assert_eq!(scopes(Lang::C, src, "return"), vec!["CFE_ES_Main"]);
    }

    #[test]
    fn c_pointer_declarator() {
        let src = "static char *lookup(int k)\n{\n    return 0;\n}\n";
        assert_eq!(scopes(Lang::C, src, "return"), vec!["lookup"]);
    }

    #[test]
    fn python_class_method() {
        let src = "class App:\n    def run(self):\n        pass\n";
        assert_eq!(scopes(Lang::Python, src, "pass"), vec!["App", "run"]);
    }

    #[test]
    fn top_level_empty() {
        let src = "static int x = 1;\n";
        assert!(scopes(Lang::C, src, "x =").is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::{Buffer, Transaction};

    #[test]
    fn from_path_maps_extensions() {
        let cases: &[(&str, Option<Lang>)] = &[
            ("main.c", Some(Lang::C)),
            ("hdr.h", Some(Lang::C)),
            ("a.cc", Some(Lang::Cpp)),
            ("a.cpp", Some(Lang::Cpp)),
            ("a.hpp", Some(Lang::Cpp)),
            ("lib.rs", Some(Lang::Rust)),
            ("app.py", Some(Lang::Python)),
            ("a.js", Some(Lang::Js)),
            ("a.mjs", Some(Lang::Js)),
            ("a.cjs", Some(Lang::Js)),
            ("a.jsx", Some(Lang::Js)),
            ("a.ts", Some(Lang::Ts)),
            ("a.mts", Some(Lang::Ts)),
            ("a.tsx", Some(Lang::Tsx)),
            ("style.css", Some(Lang::Css)),
            ("style.scss", Some(Lang::Css)),
            ("index.html", Some(Lang::Html)),
            ("index.htm", Some(Lang::Html)),
            ("Program.cs", Some(Lang::CSharp)),
            ("notes.txt", None),
            ("Makefile", None),
        ];
        for (path, want) in cases {
            assert_eq!(Lang::from_path(path), *want, "{path}");
        }
    }

    #[test]
    fn all_langs_parse_a_trivial_snippet() {
        // Grammar/core ABI mismatch would surface here as set_language() failing → None.
        let snippets: &[(Lang, &str)] = &[
            (Lang::C, "int x;\n"),
            (Lang::Cpp, "int x;\n"),
            (Lang::Rust, "fn f() {}\n"),
            (Lang::Python, "x = 1\n"),
            (Lang::Js, "const x = 1;\n"),
            (Lang::Ts, "const x: number = 1;\n"),
            (Lang::Tsx, "const x = <a>b</a>;\n"),
            (Lang::Css, "a { color: red; }\n"),
            (Lang::Html, "<p>hi</p>\n"),
            (Lang::CSharp, "class C { void M() {} }\n"),
        ];
        for (lang, src) in snippets {
            let rope = Rope::from_str(src);
            let syn = Syntax::new(*lang, &rope).unwrap_or_else(|| panic!("{lang:?}"));
            assert!(!syn.tree.root_node().has_error(), "{lang:?}");
        }
    }

    #[test]
    fn incremental_reparse_tracks_edits() {
        let src = "int main(void) {\n    return 0;\n}\n";
        let mut buf = Buffer::from_text(src);
        let mut syn = Syntax::new(Lang::C, buf.rope()).unwrap();
        assert!(!syn.tree.root_node().has_error());

        // Insert a statement before `return`.
        let at = src.find("return").unwrap();
        let tx = Transaction::insert(at, "int x = 1;\n    ");
        buf.apply(&tx);
        syn.edited(buf.rope(), &tx.changes);

        assert!(!syn.tree.root_node().has_error());
        let text = buf.rope().to_string();
        assert!(text.contains("int x = 1;"));
    }

    #[test]
    fn multi_change_shrinking_transaction_reparses_without_panic() {
        // A multi-change transaction that SHRINKS the buffer: naive pre-edit offsets would index
        // past the post-edit rope inside edited() (regression: block unindent / 2-caret backspace).
        let src = "    int a;\n    int b;\n    int c;\n";
        let mut buf = Buffer::from_text(src);
        let mut syn = Syntax::new(Lang::C, buf.rope()).unwrap();
        // Strip the 4-space indent from all three lines in ONE transaction.
        let tx = Transaction {
            changes: vec![
                crate::buffer::Change { start: 0, end: 4, text: String::new() },
                crate::buffer::Change { start: 11, end: 15, text: String::new() },
                crate::buffer::Change { start: 22, end: 26, text: String::new() },
            ],
        };
        buf.apply(&tx);
        assert_eq!(buf.rope().to_string(), "int a;\nint b;\nint c;\n");
        syn.edited(buf.rope(), &tx.changes); // must not panic
        assert!(!syn.tree.root_node().has_error());
    }
}
