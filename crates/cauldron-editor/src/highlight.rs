//! Syntax highlighting: tree-sitter highlight queries → per-line colored spans.
//!
//! API CONTRACT (view.rs compiles against this — keep signatures stable):
//! Spans are byte ranges RELATIVE TO THE LINE START, non-overlapping, sorted. Cached internally
//! by (buffer generation, viewport line range).
//!
//! Also home to the rainbow-bracket machinery ([`BracketIndex`] + [`bracket_color`]): bracket
//! nesting depth is a document-GLOBAL property, so it lives in its own whole-buffer index
//! (rebuilt lazily per edit generation) rather than in the viewport-scoped span cache above.
//!
//! Implementation: a `QueryCursor` limited (via `set_byte_range`) to the byte range covering the
//! requested viewport lines runs the grammar's bundled `highlights.scm` over the persistent
//! [`Syntax`] tree. Capture names are bucketed into [`HighlightKind`] by their first dotted
//! segment ("punctuation.bracket" → `Punctuation`). Overlapping captures are resolved by paint
//! order: `QueryCursor::captures` yields captures in document order with later (more specific)
//! patterns after earlier ones for the same node, so painting later captures over earlier ones
//! reproduces the tree-sitter-highlight "last pattern wins" convention. Predicates
//! (`#match?`/`#eq?` in the C and Rust queries) get real node text through a rope-chunk
//! `TextProvider`.

use std::ops::Range;
use std::sync::{Arc, OnceLock};

use ropey::Rope;
use tree_sitter::QueryCursor;

use crate::syntax::{Lang, Syntax};

/// Semantic bucket → color. Kept coarse: these map onto the autumn palette in [`color`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HighlightKind {
    Keyword,
    Type,
    Function,
    String,
    Comment,
    Number,
    Macro,
    Constant,
    Operator,
    Punctuation,
    Variable,
    Property,
    Text,
}

/// Spans for one line: (byte range within the line, kind). Sorted, non-overlapping.
pub type LineSpans = Vec<(Range<usize>, HighlightKind)>;

pub struct Highlighter {
    /// The compiled highlight query + capture buckets, shared PROCESS-WIDE per [`Lang`]:
    /// `Query::new` walks and validates the whole grammar (milliseconds per call), so every
    /// view for the same language reuses one compilation instead of paying it per tab.
    shared: Arc<CompiledQuery>,
    cache_generation: u64,
    cache_first_line: usize,
    cache: Vec<LineSpans>,
}

/// One language's compiled highlight query. Immutable after construction; `tree_sitter::Query`
/// is `Send + Sync`, so a single `Arc` serves every view/thread.
struct CompiledQuery {
    query: tree_sitter::Query,
    /// Capture index → semantic bucket, precomputed from `Query::capture_names()`.
    kinds: Vec<HighlightKind>,
}

/// Process-wide cache: one compiled query per [`Lang`] variant, built on first use.
/// `None` is cached too (a grammar/query mismatch never recompiles on every open).
fn shared_query(lang: Lang) -> Option<Arc<CompiledQuery>> {
    static SLOTS: [OnceLock<Option<Arc<CompiledQuery>>>; 13] = [
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
    ];
    let slot = match lang {
        Lang::C => 0,
        Lang::Cpp => 1,
        Lang::Rust => 2,
        Lang::Python => 3,
        Lang::Js => 4,
        Lang::Ts => 5,
        Lang::Tsx => 6,
        Lang::Css => 7,
        Lang::Html => 8,
        Lang::CSharp => 9,
        Lang::Json => 10,
        Lang::Yaml => 11,
        Lang::Java => 12,
    };
    SLOTS[slot].get_or_init(|| compile_query(lang).map(Arc::new)).clone()
}

fn compile_query(lang: Lang) -> Option<CompiledQuery> {
    let query = match lang {
        Lang::C => {
            tree_sitter::Query::new(&tree_sitter_c::language(), tree_sitter_c::HIGHLIGHT_QUERY)
        }
        // The cpp highlights.scm is written to EXTEND the C one upstream (it only adds the
        // C++-specific captures), so concatenate: C first, then C++ so the more specific
        // C++ patterns win under later-pattern-wins precedence.
        Lang::Cpp => {
            let combined =
                format!("{}\n{}", tree_sitter_c::HIGHLIGHT_QUERY, tree_sitter_cpp::HIGHLIGHT_QUERY);
            tree_sitter::Query::new(&tree_sitter_cpp::language(), &combined)
        }
        Lang::Rust => tree_sitter::Query::new(
            &tree_sitter_rust::language(),
            tree_sitter_rust::HIGHLIGHTS_QUERY,
        ),
        Lang::Python => tree_sitter::Query::new(
            &tree_sitter_python::language(),
            tree_sitter_python::HIGHLIGHTS_QUERY,
        ),
        // The JS grammar parses JSX natively but ships the JSX captures in a SEPARATE
        // bundled query — concatenate (same trick as C+Cpp), JSX second so it wins.
        Lang::Js => {
            let combined = format!(
                "{}\n{}",
                tree_sitter_javascript::HIGHLIGHT_QUERY,
                tree_sitter_javascript::JSX_HIGHLIGHT_QUERY
            );
            tree_sitter::Query::new(&tree_sitter_javascript::language(), &combined)
        }
        // The TS highlights.scm EXTENDS the JS one upstream (same layout as cpp-extends-c):
        // JS base first, TS additions last so the more specific TS patterns win.
        Lang::Ts => {
            let combined = format!(
                "{}\n{}",
                tree_sitter_javascript::HIGHLIGHT_QUERY,
                tree_sitter_typescript::HIGHLIGHTS_QUERY
            );
            tree_sitter::Query::new(&tree_sitter_typescript::language_typescript(), &combined)
        }
        // TSX = JS base + JSX captures + TS additions, against the tsx grammar.
        Lang::Tsx => {
            let combined = format!(
                "{}\n{}\n{}",
                tree_sitter_javascript::HIGHLIGHT_QUERY,
                tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
                tree_sitter_typescript::HIGHLIGHTS_QUERY
            );
            tree_sitter::Query::new(&tree_sitter_typescript::language_tsx(), &combined)
        }
        Lang::Css => tree_sitter::Query::new(
            &tree_sitter_css::language(),
            tree_sitter_css::HIGHLIGHTS_QUERY,
        ),
        Lang::Html => tree_sitter::Query::new(
            &tree_sitter_html::language(),
            tree_sitter_html::HIGHLIGHTS_QUERY,
        ),
        Lang::CSharp => tree_sitter::Query::new(
            &tree_sitter_c_sharp::language(),
            tree_sitter_c_sharp::HIGHLIGHTS_QUERY,
        ),
        Lang::Json => tree_sitter::Query::new(
            &tree_sitter_json::language(),
            tree_sitter_json::HIGHLIGHTS_QUERY,
        ),
        Lang::Yaml => tree_sitter::Query::new(
            &tree_sitter_yaml::language(),
            tree_sitter_yaml::HIGHLIGHTS_QUERY,
        ),
        Lang::Java => tree_sitter::Query::new(
            &tree_sitter_java::language(),
            tree_sitter_java::HIGHLIGHTS_QUERY,
        ),
    }
    .ok()?;
    let kinds = query.capture_names().iter().map(|n| kind_for_capture(n)).collect();
    Some(CompiledQuery { query, kinds })
}

impl Highlighter {
    pub fn new(lang: Lang) -> Option<Self> {
        Some(Self {
            shared: shared_query(lang)?,
            cache_generation: u64::MAX,
            cache_first_line: 0,
            cache: Vec::new(),
        })
    }

    /// Spans for `lines` (a viewport). `generation` = the buffer's edit generation, used to
    /// invalidate the cache. Returns one `LineSpans` per requested line.
    pub fn line_spans(
        &mut self,
        syntax: &Syntax,
        rope: &Rope,
        lines: Range<usize>,
        generation: u64,
    ) -> Vec<LineSpans> {
        // Cache hit: same generation, same viewport.
        if generation == self.cache_generation
            && lines.start == self.cache_first_line
            && lines.len() == self.cache.len()
        {
            return self.cache.clone();
        }

        let total_lines = rope.len_lines();
        let len_bytes = rope.len_bytes();

        // Per requested line: (line start byte, content length excluding the line terminator).
        // `None` for requested lines past the end of the rope.
        let metas: Vec<Option<(usize, usize)>> = lines
            .clone()
            .map(|line| {
                if line >= total_lines {
                    return None;
                }
                let start = rope.line_to_byte(line);
                let slice = rope.line(line);
                let mut content_len = slice.len_bytes();
                let mut chars = slice.chars_at(slice.len_chars());
                while let Some(c) = chars.prev() {
                    if c == '\n' || c == '\r' {
                        content_len -= c.len_utf8();
                    } else {
                        break;
                    }
                }
                Some((start, content_len))
            })
            .collect();

        // Per-line paint buffers: one Option<kind> per content byte. Painting captures in
        // iteration order (overwriting) resolves overlaps with later-capture-wins precedence.
        let mut paint: Vec<Option<Vec<Option<HighlightKind>>>> =
            metas.iter().map(|m| m.map(|(_, len)| vec![None; len])).collect();

        let first_line = lines.start.min(total_lines);
        let end_line = lines.end.min(total_lines);
        if first_line < end_line {
            let start_byte = rope.line_to_byte(first_line);
            let end_byte = rope.line_to_byte(end_line); // line_to_byte(len_lines()) == len_bytes()

            let mut cursor = QueryCursor::new();
            cursor.set_byte_range(start_byte..end_byte);
            // Real node text for #match?/#eq? predicates, streamed straight from the rope.
            let provider = |node: tree_sitter::Node| {
                let r = node.byte_range();
                let r = r.start.min(len_bytes)..r.end.min(len_bytes);
                rope.byte_slice(r).chunks().map(str::as_bytes)
            };
            let captures = cursor.captures(&self.shared.query, syntax.tree.root_node(), provider);
            for (m, capture_index) in captures {
                let cap = m.captures[capture_index];
                let kind = self.shared.kinds[cap.index as usize];
                let node_range = cap.node.byte_range();
                // Clamp to the viewport's byte range (captures may extend beyond it, e.g. a
                // block comment that starts above the viewport).
                let s = node_range.start.max(start_byte);
                let e = node_range.end.min(end_byte);
                if s >= e {
                    continue;
                }
                // Split at line boundaries: paint the intersection with each covered line.
                let lo_line = rope.byte_to_line(s);
                let hi_line = rope.byte_to_line(e - 1);
                for line in lo_line..=hi_line {
                    let idx = line - lines.start;
                    let (line_start, content_len) = match metas[idx] {
                        Some(m) => m,
                        None => continue,
                    };
                    let seg_start = s.max(line_start);
                    let seg_end = e.min(line_start + content_len);
                    if seg_start >= seg_end {
                        continue; // capture only touches the line terminator
                    }
                    let buf = paint[idx].as_mut().expect("meta implies buffer");
                    for slot in &mut buf[seg_start - line_start..seg_end - line_start] {
                        *slot = Some(kind);
                    }
                }
            }
        }

        // Run-length encode each paint buffer into sorted, non-overlapping spans.
        let result: Vec<LineSpans> = paint
            .into_iter()
            .map(|buf| {
                let buf = match buf {
                    Some(b) => b,
                    None => return LineSpans::new(),
                };
                let mut spans = LineSpans::new();
                let mut i = 0;
                while i < buf.len() {
                    match buf[i] {
                        None => i += 1,
                        Some(kind) => {
                            let start = i;
                            while i < buf.len() && buf[i] == Some(kind) {
                                i += 1;
                            }
                            spans.push((start..i, kind));
                        }
                    }
                }
                spans
            })
            .collect();

        self.cache_generation = generation;
        self.cache_first_line = lines.start;
        self.cache = result.clone();
        result
    }
}

/// Bucket a query capture name into a [`HighlightKind`].
///
/// Names in the bundled grammars are dotted ("function.special", "punctuation.bracket",
/// "constant.builtin"); after a few full-name specials, bucket by the FIRST dotted segment and
/// fall back to `Text` for anything unknown.
fn kind_for_capture(name: &str) -> HighlightKind {
    use HighlightKind as K;
    // Full-name specials that shouldn't follow their first segment.
    if name == "function.macro" {
        return K::Macro; // rust `foo!(...)` — macros, not functions
    }
    match name.split('.').next().unwrap_or(name) {
        "keyword" => K::Keyword,
        "type" | "constructor" => K::Type,
        "function" => K::Function,
        "string" => K::String,
        "comment" => K::Comment,
        "number" => K::Number,
        "macro" | "attribute" => K::Macro,
        "constant" | "escape" | "label" => K::Constant,
        "operator" => K::Operator,
        // HTML/JSX element names + CSS tag selectors: structural, paint like keywords.
        "tag" => K::Keyword,
        "punctuation" | "delimiter" => K::Punctuation,
        "variable" => K::Variable,
        "property" => K::Property,
        _ => K::Text,
    }
}

/// The autumn palette (occult/autumn Rune theme — rust, ember, bone, amber, plum).
pub fn color(kind: HighlightKind) -> egui::Color32 {
    use egui::Color32 as C;
    let pick = crate::theme::pick;
    match kind {
        HighlightKind::Keyword => pick(C::from_rgb(233, 110, 44), C::from_rgb(180, 68, 12)), // rust orange
        HighlightKind::Type => pick(C::from_rgb(217, 164, 65), C::from_rgb(150, 100, 10)), // amber
        HighlightKind::Function => pick(C::from_rgb(240, 195, 130), C::from_rgb(140, 92, 24)), // pale amber
        HighlightKind::String => pick(C::from_rgb(163, 190, 140), C::from_rgb(74, 112, 44)), // moss
        HighlightKind::Comment => pick(C::from_rgb(120, 113, 108), C::from_rgb(150, 144, 138)), // ash
        HighlightKind::Number => pick(C::from_rgb(197, 134, 192), C::from_rgb(140, 66, 150)), // plum
        HighlightKind::Macro => pick(C::from_rgb(197, 82, 46), C::from_rgb(168, 56, 24)), // burnt rust
        HighlightKind::Constant => pick(C::from_rgb(197, 134, 192), C::from_rgb(140, 66, 150)), // plum
        HighlightKind::Operator => pick(C::from_rgb(200, 195, 190), C::from_rgb(70, 66, 62)), // bone-dim / ink
        HighlightKind::Punctuation => pick(C::from_rgb(160, 155, 150), C::from_rgb(96, 92, 88)),
        HighlightKind::Variable => pick(C::from_rgb(238, 235, 232), C::from_rgb(30, 28, 26)), // bone / ink
        HighlightKind::Property => pick(C::from_rgb(220, 208, 190), C::from_rgb(64, 56, 44)),
        HighlightKind::Text => pick(C::from_rgb(238, 235, 232), C::from_rgb(30, 28, 26)),
    }
}

// --------------------------------------------------------------------------------------------
// rainbow brackets
// --------------------------------------------------------------------------------------------

/// Rainbow-bracket depth cycle: nesting depth `d` paints `BRACKET_PALETTE[d % 5]`. Five
/// HIGH-CONTRAST hues chosen to be instantly tellable-apart at code size on the near-black bg —
/// depth colors are a working tool, not theme decoration (user call: the autumn set blended).
pub const BRACKET_PALETTE: [egui::Color32; 5] = [
    egui::Color32::from_rgb(255, 215, 0),   // gold
    egui::Color32::from_rgb(218, 112, 214), // orchid
    egui::Color32::from_rgb(97, 175, 239),  // sky blue
    egui::Color32::from_rgb(152, 195, 121), // green
    egui::Color32::from_rgb(86, 182, 194),  // cyan
];

/// Unmatched/extra closers paint in ember red — same red as the error squiggle.
pub const BRACKET_UNMATCHED: egui::Color32 = egui::Color32::from_rgb(224, 82, 60);

/// Color for one indexed bracket: `Some(depth)` cycles the palette, `None` (unmatched closer)
/// is the error red.
pub fn bracket_color(depth: Option<usize>) -> egui::Color32 {
    match depth {
        Some(d) => BRACKET_PALETTE[d % BRACKET_PALETTE.len()],
        None => BRACKET_UNMATCHED,
    }
}

/// Every `( ) [ ] { }` in the buffer as `(byte offset, Some(nesting depth) | None)` — `None`
/// flags an unmatched/extra closer. ONE linear pass with an explicit `Vec` stack (sub-ms at
/// ~5k lines): an opener's depth is the stack size at push (0-based); a closer matching the
/// stack top pops and inherits the opener's depth; a wrong-kind or extra closer is flagged
/// `None` and does NOT pop, so a later correct closer can still match its opener. Unclosed
/// openers keep their depth color — the `(` you just typed must not flash red mid-edit.
///
/// KNOWN LIMITATION (v1): brackets inside strings and comments are counted too. Skipping them
/// needs per-byte syntax context, which this deliberately-cheap scan doesn't have.
pub fn index_brackets(rope: &Rope) -> Vec<(usize, Option<usize>)> {
    let mut entries: Vec<(usize, Option<usize>)> = Vec::new();
    let mut stack: Vec<u8> = Vec::new();
    let mut offset = 0usize;
    for chunk in rope.chunks() {
        // Byte scan is safe: bracket bytes are ASCII, never UTF-8 continuation bytes.
        for (i, b) in chunk.bytes().enumerate() {
            match b {
                b'(' | b'[' | b'{' => {
                    entries.push((offset + i, Some(stack.len())));
                    stack.push(b);
                }
                b')' | b']' | b'}' => {
                    let opener = match b {
                        b')' => b'(',
                        b']' => b'[',
                        _ => b'{',
                    };
                    if stack.last() == Some(&opener) {
                        stack.pop();
                        entries.push((offset + i, Some(stack.len())));
                    } else {
                        entries.push((offset + i, None));
                    }
                }
                _ => {}
            }
        }
        offset += chunk.len();
    }
    entries
}

/// Cached whole-buffer bracket index, invalidated by the buffer's edit generation — the same
/// cache contract as [`Highlighter::line_spans`]. The view holds one per open buffer and calls
/// [`BracketIndex::refresh`] each frame: unchanged generation = zero work.
#[derive(Default)]
pub struct BracketIndex {
    /// Generation the entries were built for; `None` = never built.
    built_for: Option<u64>,
    /// Sorted by byte offset (the scan emits them in document order).
    entries: Vec<(usize, Option<usize>)>,
}

impl BracketIndex {
    /// Rebuild the index iff `generation` differs from the one it was built for.
    pub fn refresh(&mut self, rope: &Rope, generation: u64) {
        if self.built_for == Some(generation) {
            return;
        }
        self.built_for = Some(generation);
        self.entries = index_brackets(rope);
    }

    /// Entries with byte offsets in `range` (a line's content bytes) — binary-searched slice,
    /// zero-copy.
    pub fn in_range(&self, range: Range<usize>) -> &[(usize, Option<usize>)] {
        let lo = self.entries.partition_point(|&(o, _)| o < range.start);
        let hi = self.entries.partition_point(|&(o, _)| o < range.end);
        &self.entries[lo..hi]
    }

    /// The byte offset of the bracket matching the one AT `offset`, or `None` if `offset` is not a
    /// (matched) bracket. `opener` says whether the bracket at `offset` opens (caller reads the
    /// byte). An opener and its closer carry the SAME depth (the opener is recorded before its push,
    /// the closer after its pop), and everything strictly nested carries a deeper one — so the match
    /// is the nearest same-depth entry on the correct side.
    pub fn matching(&self, offset: usize, opener: bool) -> Option<usize> {
        let idx = self.entries.binary_search_by_key(&offset, |&(o, _)| o).ok()?;
        let depth = self.entries[idx].1?; // None ⇒ an unmatched bracket, no partner
        if opener {
            self.entries[idx + 1..].iter().find(|&&(_, d)| d == Some(depth)).map(|&(o, _)| o)
        } else {
            self.entries[..idx].iter().rev().find(|&&(_, d)| d == Some(depth)).map(|&(o, _)| o)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Boot-wave item 4: the compiled highlight query is shared per language — two views of
    /// the same Lang must hold the SAME Arc (one `Query::new` per language per process, not
    /// per tab), while different languages get different compilations.
    #[test]
    fn highlight_query_is_shared_per_lang() {
        let a = Highlighter::new(Lang::C).expect("C query");
        let b = Highlighter::new(Lang::C).expect("C query");
        assert!(Arc::ptr_eq(&a.shared, &b.shared), "same Lang must share one compiled query");
        let r = Highlighter::new(Lang::Rust).expect("Rust query");
        assert!(!Arc::ptr_eq(&a.shared, &r.shared), "different Langs must not share");
    }

    fn spans_for(lang: Lang, src: &str) -> Vec<LineSpans> {
        let rope = Rope::from_str(src);
        let syn = Syntax::new(lang, &rope).expect("parse");
        let mut hl = Highlighter::new(lang).expect("query");
        hl.line_spans(&syn, &rope, 0..rope.len_lines(), 0)
    }

    /// JSON grammar loads and parses without an ABI panic (the real risk of adding a grammar
    /// crate), and highlights string keys/values.
    #[test]
    fn json_grammar_loads_and_highlights() {
        let src = "{\n  \"name\": \"cauldron\",\n  \"count\": 42,\n  \"ok\": true\n}\n";
        let spans = spans_for(Lang::Json, src);
        // Something on the string-value line got a highlight kind (String at least).
        assert!(spans.iter().any(|line| !line.is_empty()), "JSON produced highlight spans");
    }

    /// YAML grammar loads and parses without an ABI panic, and highlights.
    #[test]
    fn yaml_grammar_loads_and_highlights() {
        let src = "name: cauldron\nversion: 1\nlist:\n  - a\n  - b\n";
        let spans = spans_for(Lang::Yaml, src);
        assert!(spans.iter().any(|line| !line.is_empty()), "YAML produced highlight spans");
    }

    /// Java grammar loads and parses without an ABI panic, and highlights.
    #[test]
    fn java_grammar_loads_and_highlights() {
        let src = "class Main {
  public static void main(String[] a) {
    int x = 42;
  }
}
";
        let spans = spans_for(Lang::Java, src);
        assert!(spans.iter().any(|line| !line.is_empty()), "Java produced highlight spans");
    }

    /// The kind covering byte `col` of line `line`, if any.
    fn kind_at(spans: &[LineSpans], line: usize, col: usize) -> Option<HighlightKind> {
        spans[line].iter().find(|(r, _)| r.contains(&col)).map(|&(_, k)| k)
    }

    /// (line, col-within-line) of the first occurrence of `needle` in `src`.
    fn locate(src: &str, needle: &str) -> (usize, usize) {
        let at = src.find(needle).expect("needle present");
        let line = src[..at].matches('\n').count();
        let line_start = src[..at].rfind('\n').map_or(0, |i| i + 1);
        (line, at - line_start)
    }

    const C_SRC: &str = "// a comment\nint main(void) {\n    const char *s = \"hello\";\n    return MAX_N;\n}\n";

    #[test]
    fn new_builds_query_for_all_langs() {
        // Catches a broken concatenated query (C+Cpp, JS+TS, JS+JSX+TS) or a stale bundled
        // query at test time.
        for lang in [
            Lang::C,
            Lang::Cpp,
            Lang::Rust,
            Lang::Python,
            Lang::Js,
            Lang::Ts,
            Lang::Tsx,
            Lang::Css,
            Lang::Html,
        ] {
            assert!(Highlighter::new(lang).is_some(), "{lang:?}");
        }
    }

    #[test]
    fn c_keyword_string_comment() {
        let spans = spans_for(Lang::C, C_SRC);

        let (l, c) = locate(C_SRC, "return");
        assert_eq!(kind_at(&spans, l, c), Some(HighlightKind::Keyword), "return");
        // Every byte of the keyword is covered.
        assert_eq!(kind_at(&spans, l, c + "return".len() - 1), Some(HighlightKind::Keyword));

        let (l, c) = locate(C_SRC, "\"hello\"");
        assert_eq!(kind_at(&spans, l, c), Some(HighlightKind::String), "string literal");
        assert_eq!(kind_at(&spans, l, c + 6), Some(HighlightKind::String), "closing quote");

        let (l, c) = locate(C_SRC, "// a comment");
        assert_eq!(kind_at(&spans, l, c), Some(HighlightKind::Comment), "comment start");
        assert_eq!(kind_at(&spans, l, c + 11), Some(HighlightKind::Comment), "comment end");
    }

    #[test]
    fn c_predicate_and_precedence_all_caps_is_constant() {
        // MAX_N matches BOTH `(identifier) @variable` and the later
        // `((identifier) @constant (#match? ...))` pattern: the #match? predicate needs real
        // node text (exercises the rope TextProvider) and the later pattern must win.
        let spans = spans_for(Lang::C, C_SRC);
        let (l, c) = locate(C_SRC, "MAX_N");
        assert_eq!(kind_at(&spans, l, c), Some(HighlightKind::Constant));
    }

    #[test]
    fn rust_keyword_and_string() {
        let src = "fn main() {\n    let s = \"hi\";\n}\n";
        let spans = spans_for(Lang::Rust, src);
        let (l, c) = locate(src, "fn");
        assert_eq!(kind_at(&spans, l, c), Some(HighlightKind::Keyword));
        let (l, c) = locate(src, "\"hi\"");
        assert_eq!(kind_at(&spans, l, c), Some(HighlightKind::String));
    }

    #[test]
    fn python_keyword_string_comment() {
        let src = "# note\ndef main():\n    s = \"hello\"\n    return s\n";
        let spans = spans_for(Lang::Python, src);
        let (l, c) = locate(src, "def");
        assert_eq!(kind_at(&spans, l, c), Some(HighlightKind::Keyword), "def");
        let (l, c) = locate(src, "return");
        assert_eq!(kind_at(&spans, l, c), Some(HighlightKind::Keyword), "return");
        let (l, c) = locate(src, "\"hello\"");
        assert_eq!(kind_at(&spans, l, c), Some(HighlightKind::String), "string");
        let (l, c) = locate(src, "# note");
        assert_eq!(kind_at(&spans, l, c), Some(HighlightKind::Comment), "comment");
    }

    #[test]
    fn js_keyword_string_comment() {
        let src = "// note\nfunction main() {\n  const s = \"hello\";\n  return s;\n}\n";
        let spans = spans_for(Lang::Js, src);
        let (l, c) = locate(src, "function");
        assert_eq!(kind_at(&spans, l, c), Some(HighlightKind::Keyword), "function");
        let (l, c) = locate(src, "const");
        assert_eq!(kind_at(&spans, l, c), Some(HighlightKind::Keyword), "const");
        let (l, c) = locate(src, "\"hello\"");
        assert_eq!(kind_at(&spans, l, c), Some(HighlightKind::String), "string");
        let (l, c) = locate(src, "// note");
        assert_eq!(kind_at(&spans, l, c), Some(HighlightKind::Comment), "comment");
    }

    #[test]
    fn ts_keyword_string_comment_and_ts_only_syntax() {
        // `interface` + `number` only highlight if the TS half of the concatenated query is
        // live; `const` + the string only if the JS half is — this asserts BOTH.
        let src = "// note\ninterface Foo { n: number }\nconst s = \"hi\";\n";
        let spans = spans_for(Lang::Ts, src);
        let (l, c) = locate(src, "interface");
        assert_eq!(kind_at(&spans, l, c), Some(HighlightKind::Keyword), "interface (TS half)");
        let (l, c) = locate(src, "number");
        assert_eq!(kind_at(&spans, l, c), Some(HighlightKind::Type), "number (TS half)");
        let (l, c) = locate(src, "const");
        assert_eq!(kind_at(&spans, l, c), Some(HighlightKind::Keyword), "const (JS half)");
        let (l, c) = locate(src, "\"hi\"");
        assert_eq!(kind_at(&spans, l, c), Some(HighlightKind::String), "string (JS half)");
        let (l, c) = locate(src, "// note");
        assert_eq!(kind_at(&spans, l, c), Some(HighlightKind::Comment), "comment");
    }

    #[test]
    fn tsx_element_type_string_comment() {
        // All three concatenated query parts: JS base (const/string), JSX captures (the <div>
        // tag), TS additions (the `number` annotation).
        let src = "// note\nconst n: number = 1;\nconst el = <div className=\"x\">hi</div>;\n";
        let spans = spans_for(Lang::Tsx, src);
        let (l, c) = locate(src, "const");
        assert_eq!(kind_at(&spans, l, c), Some(HighlightKind::Keyword), "const (JS half)");
        let (l, c) = locate(src, "number");
        assert_eq!(kind_at(&spans, l, c), Some(HighlightKind::Type), "number (TS half)");
        let (l, c) = locate(src, "div");
        assert_eq!(kind_at(&spans, l, c), Some(HighlightKind::Keyword), "div (JSX tag)");
        let (l, c) = locate(src, "\"x\"");
        assert_eq!(kind_at(&spans, l, c), Some(HighlightKind::String), "attr string");
        let (l, c) = locate(src, "// note");
        assert_eq!(kind_at(&spans, l, c), Some(HighlightKind::Comment), "comment");
    }

    #[test]
    fn css_keyword_string_comment() {
        let src = "/* note */\n@media screen {\nbody { content: \"hi\"; }\n}\n";
        let spans = spans_for(Lang::Css, src);
        let (l, c) = locate(src, "@media");
        assert_eq!(kind_at(&spans, l, c), Some(HighlightKind::Keyword), "@media");
        let (l, c) = locate(src, "body");
        assert_eq!(kind_at(&spans, l, c), Some(HighlightKind::Keyword), "tag selector");
        let (l, c) = locate(src, "content");
        assert_eq!(kind_at(&spans, l, c), Some(HighlightKind::Property), "property name");
        let (l, c) = locate(src, "\"hi\"");
        assert_eq!(kind_at(&spans, l, c), Some(HighlightKind::String), "string");
        let (l, c) = locate(src, "/* note */");
        assert_eq!(kind_at(&spans, l, c), Some(HighlightKind::Comment), "comment");
    }

    #[test]
    fn html_tag_string_comment() {
        // HTML has no keywords; the structural equivalent is the element name (@tag).
        let src = "<!-- note -->\n<body class=\"x\">hi</body>\n";
        let spans = spans_for(Lang::Html, src);
        let (l, c) = locate(src, "body");
        assert_eq!(kind_at(&spans, l, c), Some(HighlightKind::Keyword), "tag name");
        let (l, c) = locate(src, "class");
        assert_eq!(kind_at(&spans, l, c), Some(HighlightKind::Macro), "attribute name");
        // attribute_value is the unquoted inner text: check the `x` byte, not the quote.
        let (l, c) = locate(src, "\"x\"");
        assert_eq!(kind_at(&spans, l, c + 1), Some(HighlightKind::String), "attr value");
        let (l, c) = locate(src, "<!-- note -->");
        assert_eq!(kind_at(&spans, l, c), Some(HighlightKind::Comment), "comment");
    }

    #[test]
    fn spans_are_line_relative_sorted_non_overlapping() {
        let rope = Rope::from_str(C_SRC);
        let syn = Syntax::new(Lang::C, &rope).unwrap();
        let mut hl = Highlighter::new(Lang::C).unwrap();
        // Also request lines past EOF: must yield empty spans, not panic.
        let all = hl.line_spans(&syn, &rope, 0..rope.len_lines() + 3, 0);
        assert_eq!(all.len(), rope.len_lines() + 3);
        for (i, line_spans) in all.iter().enumerate() {
            let content_len = if i < rope.len_lines() {
                let s = rope.line(i);
                s.len_bytes() - s.chars().filter(|c| *c == '\n' || *c == '\r').map(char::len_utf8).sum::<usize>()
            } else {
                assert!(line_spans.is_empty(), "line {i} past EOF");
                0
            };
            let mut prev_end = 0usize;
            for (r, _) in line_spans {
                assert!(r.start < r.end, "line {i}: empty span {r:?}");
                assert!(r.start >= prev_end, "line {i}: overlap/unsorted at {r:?}");
                assert!(r.end <= content_len, "line {i}: span {r:?} past content len {content_len}");
                prev_end = r.end;
            }
        }
    }

    #[test]
    fn viewport_subrange_spans_stay_line_relative() {
        // Request only lines 2..4; spans must be relative to EACH line's own start.
        let rope = Rope::from_str(C_SRC);
        let syn = Syntax::new(Lang::C, &rope).unwrap();
        let mut hl = Highlighter::new(Lang::C).unwrap();
        let spans = hl.line_spans(&syn, &rope, 2..4, 0);
        assert_eq!(spans.len(), 2);
        let (l, c) = locate(C_SRC, "return"); // line 3, col 4
        assert_eq!(l, 3);
        assert!(
            spans[l - 2].iter().any(|(r, k)| r.contains(&c) && *k == HighlightKind::Keyword),
            "return highlighted relative to its own line: {:?}",
            spans[l - 2]
        );
    }

    #[test]
    fn cache_invalidates_across_generations() {
        let src_v0 = "int main(void) {\n    return 0;\n}\n";
        let src_v1 = "int main(void) {\n    /* x */ return 0;\n}\n";
        let rope0 = Rope::from_str(src_v0);
        let syn0 = Syntax::new(Lang::C, &rope0).unwrap();
        let mut hl = Highlighter::new(Lang::C).unwrap();

        let lines = 0..rope0.len_lines();
        let first = hl.line_spans(&syn0, &rope0, lines.clone(), 0);
        // Repeat call with the SAME generation + range → served from cache, identical.
        assert_eq!(hl.line_spans(&syn0, &rope0, lines.clone(), 0), first);

        // "Edit": new rope/tree, same generation → cache is (deliberately) served stale.
        let rope1 = Rope::from_str(src_v1);
        let syn1 = Syntax::new(Lang::C, &rope1).unwrap();
        let stale = hl.line_spans(&syn1, &rope1, lines.clone(), 0);
        assert_eq!(stale, first, "same generation must hit the cache");
        assert!(!stale[1].iter().any(|&(_, k)| k == HighlightKind::Comment));

        // Bump the generation → recompute: the comment now shows up on line 1.
        let fresh = hl.line_spans(&syn1, &rope1, lines.clone(), 1);
        assert_ne!(fresh, first);
        let (l, c) = locate(src_v1, "/* x */");
        assert_eq!(kind_at(&fresh, l, c), Some(HighlightKind::Comment));

        // A different viewport at the same generation also recomputes (range mismatch).
        let sub = hl.line_spans(&syn1, &rope1, 1..2, 1);
        assert_eq!(sub.len(), 1);
        assert!(sub[0].iter().any(|&(_, k)| k == HighlightKind::Comment));
    }

    // --- rainbow brackets ----------------------------------------------------------------

    #[test]
    fn bracket_nesting_depths_and_pairs() {
        let rope = Rope::from_str("fn f(a: [u8; { 1 }]) {}\n");
        assert_eq!(
            index_brackets(&rope),
            vec![
                (4, Some(0)),  // (
                (8, Some(1)),  // [
                (13, Some(2)), // {
                (17, Some(2)), // } — closer inherits its opener's depth
                (18, Some(1)), // ]
                (19, Some(0)), // )
                (21, Some(0)), // { — a SIBLING reuses the depth (not a running counter)
                (22, Some(0)), // }
            ]
        );
    }

    #[test]
    fn bracket_cycle_wraps_past_the_palette() {
        // 6 nested openers: depths 0..=5; depth 5 must WRAP back to the first palette color.
        let idx = index_brackets(&Rope::from_str("((((((x))))))"));
        assert_eq!(idx[5], (5, Some(5)));
        assert_eq!(bracket_color(Some(5)), BRACKET_PALETTE[0], "depth 5 wraps to color 0");
        assert_eq!(bracket_color(Some(6)), BRACKET_PALETTE[1]);
        assert_eq!(bracket_color(Some(2)), BRACKET_PALETTE[2]);
        assert_eq!(bracket_color(None), BRACKET_UNMATCHED);
    }

    #[test]
    fn unmatched_and_mismatched_closers_flagged() {
        // Extra closer on an empty stack…
        assert_eq!(index_brackets(&Rope::from_str(")")), vec![(0, None)]);
        // …and a WRONG-KIND closer: flagged, but does NOT pop — the pair around it still matches.
        assert_eq!(
            index_brackets(&Rope::from_str("[)]")),
            vec![(0, Some(0)), (1, None), (2, Some(0))]
        );
        // An unclosed opener keeps its depth color (typing `(` must not flash red mid-edit).
        assert_eq!(index_brackets(&Rope::from_str("(")), vec![(0, Some(0))]);
    }

    #[test]
    fn matching_finds_the_partner_on_both_sides() {
        let rope = Rope::from_str("a([{x}])");
        let mut idx = BracketIndex::default();
        idx.refresh(&rope, 0);
        // Openers: ( @1, [ @2, { @3.  Closers: } @5, ] @6, ) @7.
        assert_eq!(idx.matching(1, true), Some(7), "paren pair");
        assert_eq!(idx.matching(2, true), Some(6), "square pair");
        assert_eq!(idx.matching(3, true), Some(5), "brace pair");
        assert_eq!(idx.matching(7, false), Some(1), "closer finds its opener");
        assert_eq!(idx.matching(5, false), Some(3));
        // A non-bracket byte and an unmatched bracket have no partner.
        assert_eq!(idx.matching(0, true), None, "not a bracket");
        let mut idx2 = BracketIndex::default();
        idx2.refresh(&Rope::from_str(")"), 0);
        assert_eq!(idx2.matching(0, false), None, "unmatched closer");
    }

    #[test]
    fn bracket_index_rebuilds_only_on_generation_change() {
        // Same pattern as cache_invalidates_across_generations above.
        let rope0 = Rope::from_str("(a)\n");
        let mut idx = BracketIndex::default();
        idx.refresh(&rope0, 0);
        assert_eq!(idx.in_range(0..rope0.len_bytes()).len(), 2);

        // "Edit": new rope, SAME generation → (deliberately) served stale.
        let rope1 = Rope::from_str("((a))\n");
        idx.refresh(&rope1, 0);
        assert_eq!(idx.in_range(0..rope1.len_bytes()).len(), 2, "same generation must not rebuild");

        // Bump the generation → rebuild sees all four brackets.
        idx.refresh(&rope1, 1);
        assert_eq!(idx.in_range(0..rope1.len_bytes()).len(), 4);
        // in_range slices by byte offset (the paint loop asks per visible line).
        assert_eq!(idx.in_range(1..2), &[(1, Some(1))]);
    }
}
