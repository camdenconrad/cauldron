//! THE single home of position conversions: byte offset ↔ (line, col) ↔ LSP UTF-16 positions.
//!
//! LSP positions are (line, character) where `character` counts UTF-16 code units by default.
//! clangd and rust-analyzer both honor `positionEncoding: utf-8` negotiation (then `character`
//! is a byte column), but the utf-16 path must exist as the fallback — a server that ignores the
//! negotiation would otherwise get corrupted positions on any non-ASCII line. Everything routes
//! through here; nothing else in the codebase converts positions.

use ropey::Rope;

/// A zero-based (line, column) pair. `col`'s unit depends on the function that produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    pub line: usize,
    pub col: usize,
}

/// Byte offset → (line, byte-column). For tree-sitter and the utf-8 LSP encoding.
///
/// The byte is clamped to the buffer end. A caller can hand us a STALE offset — most commonly a
/// hover byte captured under the mouse, then dispatched ~0.45s later against a buffer the user has
/// since shrunk by typing — and `byte_to_line` PANICS on an out-of-range index. A position at the
/// end is the only sane answer (and the response it feeds is dropped anyway once its buffer
/// generation no longer matches). This is the one chokepoint every byte→LSP-position path funnels
/// through, so clamping here immunizes hover, completion, signature-help, definition and the rest.
pub fn byte_to_point(rope: &Rope, byte: usize) -> Point {
    let byte = byte.min(rope.len_bytes());
    let line = rope.byte_to_line(byte);
    let line_start = rope.line_to_byte(line);
    Point { line, col: byte - line_start }
}

/// (line, byte-column) → byte offset.
pub fn point_to_byte(rope: &Rope, p: Point) -> usize {
    rope.line_to_byte(p.line) + p.col
}

/// Byte offset → LSP UTF-16 position (line, utf-16 code units). Clamps a stale/out-of-range byte
/// to the buffer end for the same reason as [`byte_to_point`] — never panic on a shrunk buffer.
pub fn byte_to_utf16(rope: &Rope, byte: usize) -> Point {
    let byte = byte.min(rope.len_bytes());
    let line = rope.byte_to_line(byte);
    let line_start_c = rope.line_to_char(line);
    let c = rope.byte_to_char(byte);
    let mut units = 0usize;
    for ch in rope.slice(line_start_c..c).chars() {
        units += ch.len_utf16();
    }
    Point { line, col: units }
}

/// (line, byte-column) → byte offset, CLAMPED — the utf-8 LSP inverse. Servers legally send
/// positions past the end of a line or past the end of the file; the naive [`point_to_byte`]
/// would panic or land inside a line break. Mirrors [`utf16_to_byte`]'s clamping exactly:
/// line clamps to the last line, column clamps to the line CONTENT end (before the trailing
/// `\n` / `\r`). A column landing mid-codepoint snaps forward to the next char boundary
/// (same rounding [`utf16_to_byte`] applies to a lone surrogate column).
pub fn point_to_byte_clamped(rope: &Rope, p: Point) -> usize {
    let line = p.line.min(rope.len_lines().saturating_sub(1));
    let line_start_c = rope.line_to_char(line);
    let mut bytes = 0usize;
    let mut c = line_start_c;
    for ch in rope.line(line).chars() {
        // Clamp to the line CONTENT — never land past the line break.
        if bytes >= p.col || ch == '\n' || ch == '\r' {
            break;
        }
        bytes += ch.len_utf8();
        c += 1;
    }
    rope.char_to_byte(c)
}

/// LSP UTF-16 position → byte offset. Clamps past-end-of-line columns to the line end
/// (LSP explicitly allows servers to send those).
pub fn utf16_to_byte(rope: &Rope, p: Point) -> usize {
    let line = p.line.min(rope.len_lines().saturating_sub(1));
    let line_start_c = rope.line_to_char(line);
    let mut units = 0usize;
    let mut c = line_start_c;
    for ch in rope.line(line).chars() {
        // Clamp to the line CONTENT — never land past the line break.
        if units >= p.col || ch == '\n' || ch == '\r' {
            break;
        }
        units += ch.len_utf16();
        c += 1;
    }
    rope.char_to_byte(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ASCII, BMP multibyte (é, 2 bytes / 1 utf-16 unit), astral (𐍈, 4 bytes / 2 utf-16 units).
    const S: &str = "abc\nhé𐍈llo\nplain\n";

    /// Regression: a STALE byte past the buffer end (a hover offset dispatched after the buffer
    /// shrank) must clamp to the end, not panic. This is the exact crash the field build hit —
    /// `byte index 79, Rope/RopeSlice byte length 77`.
    #[test]
    fn out_of_range_byte_clamps_instead_of_panicking() {
        let rope = Rope::from_str("abc\ndef\n"); // 8 bytes, last line index 2 (the "" after \n)
        let end = byte_to_point(&rope, 8);
        // Byte well past the end lands at the same place as the true end — never a panic.
        assert_eq!(byte_to_point(&rope, 79), end);
        assert_eq!(byte_to_utf16(&rope, 79), byte_to_utf16(&rope, 8));
        // And a byte exactly at len is fine too (a common off-by-one at end-of-buffer).
        let _ = byte_to_point(&rope, rope.len_bytes());
        let _ = byte_to_utf16(&rope, rope.len_bytes());
    }

    #[test]
    fn roundtrip_every_char_boundary() {
        let rope = Rope::from_str(S);
        for c in 0..=rope.len_chars() {
            let byte = rope.char_to_byte(c);
            assert_eq!(point_to_byte(&rope, byte_to_point(&rope, byte)), byte, "byte-point at {byte}");
            assert_eq!(utf16_to_byte(&rope, byte_to_utf16(&rope, byte)), byte, "utf16 at {byte}");
        }
    }

    #[test]
    fn astral_counts_two_utf16_units() {
        let rope = Rope::from_str(S);
        // byte offset of 'l' after 𐍈 on line 1: 'h'(1)+'é'(2)+'𐍈'(4) = 7 into the line
        let byte = rope.line_to_byte(1) + 7;
        let p = byte_to_utf16(&rope, byte);
        assert_eq!(p, Point { line: 1, col: 4 }); // h=1, é=1, 𐍈=2
    }

    #[test]
    fn past_eol_clamps() {
        let rope = Rope::from_str("ab\ncd");
        let b = utf16_to_byte(&rope, Point { line: 0, col: 99 });
        assert_eq!(b, 2); // clamped to end of line CONTENT, before the '\n'
    }

    #[test]
    fn clamped_inverse_roundtrips_every_char_boundary() {
        let rope = Rope::from_str(S);
        for c in 0..=rope.len_chars() {
            let byte = rope.char_to_byte(c);
            assert_eq!(point_to_byte_clamped(&rope, byte_to_point(&rope, byte)), byte, "clamped inverse at {byte}");
        }
    }

    #[test]
    fn clamped_inverse_past_eof_line() {
        let rope = Rope::from_str("ab\ncd"); // 2 lines, no trailing newline
        // Any line past the last clamps to the last line; the column then applies there.
        assert_eq!(point_to_byte_clamped(&rope, Point { line: 99, col: 0 }), 3); // start of "cd"
        assert_eq!(point_to_byte_clamped(&rope, Point { line: 99, col: 99 }), 5); // end of "cd"
        // A trailing '\n' means ropey counts a final EMPTY line — clamp lands there, col irrelevant.
        let rope = Rope::from_str("ab\n");
        assert_eq!(point_to_byte_clamped(&rope, Point { line: 99, col: 99 }), 3);
    }

    #[test]
    fn clamped_inverse_past_eol_col() {
        let rope = Rope::from_str("ab\ncd\n");
        // Clamps to line CONTENT end, before the '\n' — never inside the break.
        assert_eq!(point_to_byte_clamped(&rope, Point { line: 0, col: 99 }), 2);
        assert_eq!(point_to_byte_clamped(&rope, Point { line: 1, col: 99 }), 5);
        // CRLF: clamp stops before the '\r' too.
        let rope = Rope::from_str("ab\r\ncd");
        assert_eq!(point_to_byte_clamped(&rope, Point { line: 0, col: 99 }), 2);
    }

    #[test]
    fn clamped_inverse_exact_boundaries() {
        let rope = Rope::from_str(S);
        // Exactly at line-content end (line 1: "hé𐍈llo" = 1+2+4+3 = 10 bytes of content).
        let l1 = rope.line_to_byte(1);
        assert_eq!(point_to_byte_clamped(&rope, Point { line: 1, col: 10 }), l1 + 10);
        // Exactly at column 0 of the last line.
        let last = rope.len_lines() - 1;
        assert_eq!(point_to_byte_clamped(&rope, Point { line: last, col: 0 }), rope.line_to_byte(last));
        // Exactly the last content byte of the file's last non-empty line.
        assert_eq!(point_to_byte_clamped(&rope, Point { line: 2, col: 5 }), rope.line_to_byte(2) + 5);
    }
}
