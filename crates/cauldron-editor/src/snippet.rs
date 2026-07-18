//! LSP snippet syntax → plain text + tabstops. The subset that matters in practice:
//! `$1`, `${1}`, `${1:placeholder}`, `${1|choice,other|}` (first choice wins), `$0` (final
//! caret), `\$`/`\\` escapes. Variables (`$TM_FILENAME`, `${VAR:default}`) resolve to their
//! default or empty — this editor doesn't grow a variable table for the one or two servers
//! that emit them. Nested placeholders keep their plain text; only the OUTER stop is kept.
//!
//! Output ranges are byte offsets into the returned plain text, ordered for Tab traversal:
//! ascending tabstop number, `$0` (or an implicit end-of-snippet stop) last.

use std::ops::Range;

/// One traversal stop: where the caret goes, selecting `range` (empty = bare caret).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stop {
    pub index: u32,
    pub range: Range<usize>,
}

/// Parse `snippet` into `(plain_text, stops)`. Stops are Tab-ordered (1,2,…,0-last) with an
/// implicit end-of-text stop appended when the snippet has no `$0`. Only the FIRST occurrence
/// of each index is kept (mirrored stops degrade to single).
pub fn parse(snippet: &str) -> (String, Vec<Stop>) {
    let mut out = String::with_capacity(snippet.len());
    let mut stops: Vec<Stop> = Vec::new();
    parse_into(snippet, &mut out, &mut stops, 0);
    // Tab order: 1,2,3,…, then 0 last. Stable sort keeps document order within an index;
    // dedup keeps the first occurrence.
    stops.sort_by_key(|s| if s.index == 0 { u32::MAX } else { s.index });
    let mut seen = std::collections::HashSet::new();
    stops.retain(|s| seen.insert(s.index));
    if !stops.iter().any(|s| s.index == 0) {
        stops.push(Stop { index: 0, range: out.len()..out.len() });
    }
    (out, stops)
}

/// Placeholder-default recursion is capped: past this depth the default text passes through
/// UNPARSED. Real snippets nest 2–3 deep; a hostile server's 10k-deep `${1:${1:…}}` must
/// not overflow the UI thread's stack.
const MAX_NEST: u32 = 16;

/// Recursive worker: appends plain text to `out`, collecting stops at absolute offsets.
/// Bounded by [`MAX_NEST`].
fn parse_into(src: &str, out: &mut String, stops: &mut Vec<Stop>, depth: u32) {
    if depth > MAX_NEST {
        out.push_str(src);
        return;
    }
    let mut chars = src.char_indices().peekable();
    while let Some((_, ch)) = chars.next() {
        match ch {
            '\\' => {
                // `\$`, `\\`, `\}` escape; a trailing backslash stays literal.
                match chars.next() {
                    Some((_, c @ ('$' | '\\' | '}'))) => out.push(c),
                    Some((_, c)) => {
                        out.push('\\');
                        out.push(c);
                    }
                    None => out.push('\\'),
                }
            }
            '$' => match chars.peek() {
                Some((_, '{')) => {
                    chars.next();
                    // Balanced-brace body (placeholders nest: `${1:{ $2 }}` is rare but legal).
                    let mut body = String::new();
                    let mut braces = 1;
                    for (_, c) in chars.by_ref() {
                        match c {
                            '{' => braces += 1,
                            '}' => {
                                braces -= 1;
                                if braces == 0 {
                                    break;
                                }
                            }
                            _ => {}
                        }
                        body.push(c);
                    }
                    parse_braced(&body, out, stops, depth);
                }
                Some((_, c)) if c.is_ascii_digit() => {
                    let mut n = 0u32;
                    while let Some((_, c)) = chars.peek() {
                        let Some(d) = c.to_digit(10) else { break };
                        n = n.saturating_mul(10).saturating_add(d);
                        chars.next();
                    }
                    stops.push(Stop { index: n, range: out.len()..out.len() });
                }
                Some((_, c)) if c.is_ascii_alphabetic() || *c == '_' => {
                    // `$VARIABLE` → empty (no variable table).
                    while let Some((_, c)) = chars.peek() {
                        if c.is_ascii_alphanumeric() || *c == '_' {
                            chars.next();
                        } else {
                            break;
                        }
                    }
                }
                _ => out.push('$'),
            },
            c => out.push(c),
        }
    }
}

/// The inside of a `${…}`: `N`, `N:default`, `N|a,b|`, `VAR`, `VAR:default`.
fn parse_braced(body: &str, out: &mut String, stops: &mut Vec<Stop>, depth: u32) {
    let digits_end = body.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits_end > 0 {
        let index: u32 = body[..digits_end].parse().unwrap_or(0);
        let rest = &body[digits_end..];
        let start = out.len();
        if let Some(default) = rest.strip_prefix(':') {
            parse_into(default, out, stops, depth + 1);
        } else if let Some(choices) = rest.strip_prefix('|') {
            let first = choices.trim_end_matches('|').split(',').next().unwrap_or("");
            out.push_str(first);
        }
        stops.push(Stop { index, range: start..out.len() });
    } else {
        // Variable form: keep the default text, drop the name.
        if let Some(colon) = body.find(':') {
            parse_into(&body[colon + 1..], out, stops, depth + 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stop_texts<'a>(plain: &'a str, stops: &[Stop]) -> Vec<(u32, &'a str)> {
        stops.iter().map(|s| (s.index, &plain[s.range.clone()])).collect()
    }

    #[test]
    fn plain_text_passes_through() {
        let (t, s) = parse("hello world");
        assert_eq!(t, "hello world");
        // Implicit final stop at the end.
        assert_eq!(s, vec![Stop { index: 0, range: 11..11 }]);
    }

    /// rust-analyzer's classic: `format!("$1", $0)`-style postfix and call snippets.
    #[test]
    fn tabstops_and_placeholders() {
        let (t, s) = parse("for ${1:item} in ${2:iter} {\n    $0\n}");
        assert_eq!(t, "for item in iter {\n    \n}");
        assert_eq!(stop_texts(&t, &s), vec![(1, "item"), (2, "iter"), (0, "")]);

        let (t, s) = parse("foo($1)$0");
        assert_eq!(t, "foo()");
        assert_eq!(s[0], Stop { index: 1, range: 4..4 });
        assert_eq!(s[1].index, 0);
    }

    #[test]
    fn zero_last_and_duplicates_first_wins() {
        let (t, s) = parse("$0 then ${1:x} and $1");
        assert_eq!(t, " then x and ");
        assert_eq!(stop_texts(&t, &s), vec![(1, "x"), (0, "")]);
        assert_eq!(s[0].range, 6..7);
        assert_eq!(s[1].range, 0..0, "$0 keeps its position but traverses last");
    }

    #[test]
    fn choices_variables_and_escapes() {
        let (t, s) = parse("${1|const,let|} x = \\$HOME; $TM_FILENAME ${WORKSPACE:here}");
        assert_eq!(t, "const x = $HOME;  here");
        assert_eq!(stop_texts(&t, &s)[0], (1, "const"));

        let (t, _) = parse("a\\\\b \\}");
        assert_eq!(t, "a\\b }");
    }

    /// A hostile deeply-nested snippet parses without overflowing (Rule-2/Rule-1 dogfood):
    /// past MAX_NEST the default text passes through unparsed instead of recursing.
    #[test]
    fn pathological_nesting_is_bounded() {
        let mut s = String::new();
        for _ in 0..5_000 {
            s.push_str("${1:");
        }
        s.push('x');
        for _ in 0..5_000 {
            s.push('}');
        }
        let (plain, stops) = parse(&s); // must return, not blow the stack
        assert!(plain.contains('x'));
        assert!(!stops.is_empty());
    }

    #[test]
    fn nested_placeholder_keeps_outer_stop_and_inner_text() {
        let (t, s) = parse("${1:Vec<${2:u8}>}");
        assert_eq!(t, "Vec<u8>");
        // Both stops exist; 1 spans the whole default, 2 the inner.
        assert_eq!(stop_texts(&t, &s), vec![(1, "Vec<u8>"), (2, "u8"), (0, "")]);
    }
}
