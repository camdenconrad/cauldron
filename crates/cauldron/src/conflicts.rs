//! Merge-conflict resolution inside the editor. Parses Git conflict regions — both the 2-way
//! (`<<<<<<< / ======= / >>>>>>>`) and the diff3 (`<<<<<<< / ||||||| base / ======= / >>>>>>>`)
//! styles — into structured hunks the app walks through, applying a chosen side as an
//! undo-safe buffer edit. This is the practical per-conflict resolver that complements the
//! file-level take-ours/take-theirs in the git panel.

/// One conflict region in a file, by BYTE offsets into the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// Byte range of the WHOLE region (from `<<<<<<<` line through the `>>>>>>>` line's newline).
    pub start: usize,
    pub end: usize,
    /// "Ours" side (current branch) text, without markers or trailing newline normalization.
    pub ours: String,
    /// "Theirs" side (incoming) text.
    pub theirs: String,
    /// The common ancestor, present only in diff3 conflict style.
    pub base: Option<String>,
}

impl Conflict {
    /// The replacement text for a chosen resolution.
    pub fn resolved(&self, side: Side) -> String {
        match side {
            Side::Ours => self.ours.clone(),
            Side::Theirs => self.theirs.clone(),
            // Both: ours then theirs, each keeping its own lines.
            Side::Both => {
                if self.ours.is_empty() {
                    self.theirs.clone()
                } else if self.theirs.is_empty() {
                    self.ours.clone()
                } else {
                    format!("{}{}", self.ours, self.theirs)
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Ours,
    Theirs,
    Both,
}

/// Parse every conflict region in `text`. Robust to files with no conflicts (returns empty)
/// and to a trailing region missing its closing marker (dropped). Byte-exact so the caller
/// can splice replacements without re-scanning.
pub fn parse(text: &str) -> Vec<Conflict> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    // Walk line by line, tracking each line's byte range.
    let mut line_start = 0usize;
    // State while inside a conflict.
    enum St {
        Idle,
        Ours,
        Base,
        Theirs,
    }
    let mut st = St::Idle;
    let mut region_start = 0usize;
    let mut ours = String::new();
    let mut base = String::new();
    let mut theirs = String::new();
    let mut has_base = false;

    let mut i = 0usize;
    while i <= bytes.len() {
        // Find the end of the current line (exclusive of, then including, the newline).
        let nl = text[line_start..].find('\n').map(|p| line_start + p);
        let content_end = nl.unwrap_or(bytes.len());
        let next_start = nl.map(|p| p + 1).unwrap_or(bytes.len());
        let line = &text[line_start..content_end];

        match st {
            St::Idle => {
                if line.starts_with("<<<<<<<") {
                    region_start = line_start;
                    ours.clear();
                    base.clear();
                    theirs.clear();
                    has_base = false;
                    st = St::Ours;
                }
            }
            St::Ours => {
                if line.starts_with("|||||||") {
                    has_base = true;
                    st = St::Base;
                } else if line.starts_with("=======") {
                    st = St::Theirs;
                } else if line.starts_with("<<<<<<<") {
                    // Malformed nested start — abandon this region, restart here.
                    region_start = line_start;
                    ours.clear();
                } else {
                    ours.push_str(&text[line_start..next_start]);
                }
            }
            St::Base => {
                if line.starts_with("=======") {
                    st = St::Theirs;
                } else {
                    base.push_str(&text[line_start..next_start]);
                }
            }
            St::Theirs => {
                if line.starts_with(">>>>>>>") {
                    out.push(Conflict {
                        start: region_start,
                        end: next_start,
                        ours: std::mem::take(&mut ours),
                        theirs: std::mem::take(&mut theirs),
                        base: has_base.then(|| std::mem::take(&mut base)),
                    });
                    st = St::Idle;
                } else {
                    theirs.push_str(&text[line_start..next_start]);
                }
            }
        }

        if nl.is_none() {
            break;
        }
        line_start = next_start;
        i = next_start;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO_WAY: &str = "line1\n<<<<<<< HEAD\nours a\nours b\n=======\ntheirs a\n>>>>>>> branch\nline2\n";

    #[test]
    fn parses_two_way() {
        let c = parse(TWO_WAY);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].ours, "ours a\nours b\n");
        assert_eq!(c[0].theirs, "theirs a\n");
        assert_eq!(c[0].base, None);
        // The region spans exactly the marker block.
        assert_eq!(&TWO_WAY[c[0].start..c[0].end], "<<<<<<< HEAD\nours a\nours b\n=======\ntheirs a\n>>>>>>> branch\n");
    }

    #[test]
    fn parses_diff3_with_base() {
        let src = "<<<<<<< HEAD\nours\n||||||| base\ncommon\n=======\ntheirs\n>>>>>>> b\n";
        let c = parse(src);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].ours, "ours\n");
        assert_eq!(c[0].base.as_deref(), Some("common\n"));
        assert_eq!(c[0].theirs, "theirs\n");
    }

    #[test]
    fn resolved_sides() {
        let c = &parse(TWO_WAY)[0];
        assert_eq!(c.resolved(Side::Ours), "ours a\nours b\n");
        assert_eq!(c.resolved(Side::Theirs), "theirs a\n");
        assert_eq!(c.resolved(Side::Both), "ours a\nours b\ntheirs a\n");
    }

    #[test]
    fn multiple_and_none() {
        assert!(parse("no conflicts here\njust text\n").is_empty());
        let two = format!("{TWO_WAY}{TWO_WAY}");
        assert_eq!(parse(&two).len(), 2);
        // Unterminated region is dropped, not panicked on.
        assert!(parse("<<<<<<< HEAD\nours\n=======\ntheirs\n").is_empty());
    }
}
