//! Content-Length framing — the wire format stdio DAP adapters speak (identical to LSP's).
//!
//! Adapted from cauldron-lsp's transport with the same tolerances: shell-wrapped adapters print
//! banners before their first real header, so any line that isn't a header we recognize is
//! skipped silently instead of poisoning the stream. `Content-Length` counts BYTES of the JSON
//! body (multibyte-safe), never characters, and header names match case-insensitively.

use serde_json::Value;
use std::io::{self, BufRead, Write};

/// Read one framed message off the adapter's stdout.
///
/// The two buffers are caller-owned so the reader thread reuses their allocations across
/// messages; both are cleared on EVERY path out of this function — success, EOF, and error —
/// so a failed read never leaks state into the next call.
///
/// Returns `Ok(None)` on clean EOF at a message boundary, i.e. before any `Content-Length`
/// header was seen (a dying wrapper's trailing chatter still counts as a boundary), and
/// `Err(UnexpectedEof)` when the stream dies mid-message.
pub fn read_message(
    r: &mut impl BufRead,
    header_buf: &mut String,
    body_buf: &mut Vec<u8>,
) -> io::Result<Option<Value>> {
    header_buf.clear();
    body_buf.clear();

    // Header block: lines until the blank \r\n separator. Only Content-Length matters;
    // Content-Type, unknown headers, and colonless junk are all skipped.
    let mut content_length: Option<usize> = None;
    let len = loop {
        header_buf.clear();
        if r.read_line(header_buf)? == 0 {
            return match content_length {
                None => Ok(None),
                Some(_) => {
                    header_buf.clear();
                    Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "eof inside message headers",
                    ))
                }
            };
        }
        let line = header_buf.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            match content_length {
                Some(n) => break n, // end of headers, body follows
                None => continue,   // stray blank line amid pre-header junk
            }
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                // A malformed value is junk too — the parse just doesn't stick.
                if let Ok(n) = value.trim().parse::<usize>() {
                    content_length = Some(n);
                }
            }
        }
    };

    // Body: exactly `len` BYTES, then one JSON document. Both buffers cleared before any
    // return so the reader loop's next call starts pristine even after an error.
    body_buf.resize(len, 0);
    let parsed = r.read_exact(body_buf).and_then(|()| {
        serde_json::from_slice::<Value>(body_buf)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    });
    header_buf.clear();
    body_buf.clear();
    parsed.map(Some)
}

/// Frame and write one message: `Content-Length: {n}\r\n\r\n` + the JSON bytes, then flush.
/// `n` counts the serialized BYTES. The payload is written verbatim — the serializer escapes
/// any `\r\n` inside JSON strings, so the framing never collides with message content.
pub fn write_message(w: &mut impl Write, v: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(v)?;
    write!(w, "Content-Length: {}\r\n\r\n", body.len())?;
    w.write_all(&body)?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;

    /// One-shot read with fresh buffers, for tests that don't inspect buffer state.
    fn read(input: &[u8]) -> io::Result<Option<Value>> {
        let mut header = String::new();
        let mut body = Vec::new();
        read_message(&mut Cursor::new(input), &mut header, &mut body)
    }

    #[test]
    fn write_then_read_round_trips() {
        let v = json!({"seq": 1, "type": "request", "command": "initialize",
                       "arguments": {"clientID": "cauldron", "text": "line1\r\nline2"}});
        let mut wire = Vec::new();
        write_message(&mut wire, &v).unwrap();
        assert_eq!(read(&wire).unwrap().unwrap(), v);
    }

    #[test]
    fn junk_lines_before_the_first_header_are_skipped() {
        // Wrapped adapters (python -m …) can print chatter before their first frame.
        let input = b"debugpy warning: something\nStarting adapter...\r\n\r\nContent-Length: 11\r\n\r\n{\"ok\":true}";
        assert_eq!(read(input).unwrap().unwrap(), json!({"ok": true}));
    }

    #[test]
    fn content_type_header_alongside_content_length() {
        let input = b"Content-Length: 2\r\nContent-Type: application/vscode-jsonrpc; charset=utf-8\r\n\r\n{}";
        assert_eq!(read(input).unwrap().unwrap(), json!({}));
    }

    #[test]
    fn header_name_matches_case_insensitively() {
        assert_eq!(
            read(b"content-length: 2\r\n\r\n{}").unwrap().unwrap(),
            json!({})
        );
        assert_eq!(
            read(b"CONTENT-LENGTH: 2\r\n\r\n{}").unwrap().unwrap(),
            json!({})
        );
    }

    #[test]
    fn eof_mid_body_is_an_error_and_clears_buffers() {
        let input: &[u8] = b"Content-Length: 100\r\n\r\n{\"truncated\":true}";
        let mut header = String::from("stale");
        let mut body = vec![1, 2, 3];
        let err = read_message(&mut Cursor::new(input), &mut header, &mut body).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        // The reusable buffers must come back empty even on the error path.
        assert!(header.is_empty() && body.is_empty());
    }

    #[test]
    fn eof_after_content_length_but_before_body_is_an_error() {
        let err = read(b"Content-Length: 10\r\n").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn eof_at_message_boundary_is_clean() {
        assert!(read(b"").unwrap().is_none());
        // Trailing chatter from a dying adapter, with no Content-Length, is still a boundary.
        assert!(read(b"adapter exited unexpectedly\n").unwrap().is_none());
    }

    #[test]
    fn content_length_counts_bytes_not_chars() {
        let v = json!({"s": "héllo"});
        let body = serde_json::to_vec(&v).unwrap();
        // "héllo" is 5 chars but 6 bytes; serde_json emits raw UTF-8, so byte and char
        // counts genuinely differ here.
        assert!(std::str::from_utf8(&body).unwrap().chars().count() < body.len());
        let mut wire = Vec::new();
        write_message(&mut wire, &v).unwrap();
        assert!(wire.starts_with(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes()));
        assert_eq!(read(&wire).unwrap().unwrap(), v);
    }

    #[test]
    fn two_messages_back_to_back() {
        let mut wire = Vec::new();
        write_message(&mut wire, &json!({"seq": 1, "type": "response"})).unwrap();
        write_message(&mut wire, &json!({"seq": 2, "type": "event"})).unwrap();
        // One cursor, shared buffers — exactly how the reader thread consumes the stream.
        let mut cur = Cursor::new(wire.as_slice());
        let mut header = String::new();
        let mut body = Vec::new();
        let a = read_message(&mut cur, &mut header, &mut body)
            .unwrap()
            .unwrap();
        let b = read_message(&mut cur, &mut header, &mut body)
            .unwrap()
            .unwrap();
        assert_eq!(a["seq"], 1);
        assert_eq!(b["seq"], 2);
        assert!(read_message(&mut cur, &mut header, &mut body)
            .unwrap()
            .is_none());
    }
}
