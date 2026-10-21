//! LSP's wire format: `Content-Length` headers, then a JSON body.
//!
//! This is the whole transport. It is separate from the protocol so it can be tested against bytes
//! rather than against a process, and so a caller with its own framing (an editor plugin, a test
//! harness) can skip it.
//!
//! Framing rules that matter and are easy to get wrong:
//! * Headers are `Name: value` separated by `\r\n`, terminated by a blank `\r\n` line.
//! * `Content-Length` counts **bytes of the body**, not characters: a message full of CJK must use
//!   the UTF-8 length, or the reader desynchronises.
//! * Unknown headers must be ignored, not rejected: the spec lets clients send extras.
//! * A header name is case-insensitive.

use std::io::{self, BufRead, Read, Write};

/// The largest body this will accept. A completion request is a few kilobytes; a megabyte is
/// already pathological, and refusing it beats allocating whatever a peer asks for.
pub const MAX_BODY: usize = 16 * 1024 * 1024;

/// A message read off the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub body: String,
}

/// Write one message: the header, then the body's bytes.
pub fn write_message(out: &mut impl Write, body: &str) -> io::Result<()> {
    write!(out, "Content-Length: {}\r\n\r\n", body.len())?;
    out.write_all(body.as_bytes())?;
    out.flush()
}

/// Read one message, or `None` at a clean end of stream.
///
/// Returns an error for a malformed header block, a body that is not valid UTF-8, or a length over
/// [`MAX_BODY`] - all of which mean the stream can no longer be trusted.
pub fn read_message(input: &mut impl BufRead) -> io::Result<Option<Message>> {
    let mut length: Option<usize> = None;
    let mut saw_header = false;
    loop {
        let mut line = String::new();
        let read = input.read_line(&mut line)?;
        if read == 0 {
            // End of stream. A clean end only when nothing was pending.
            return if saw_header {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "stream ended in the middle of a header block",
                ))
            } else {
                Ok(None)
            };
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            // Blank line: headers are done.
            break;
        }
        saw_header = true;
        let Some((name, value)) = trimmed.split_once(':') else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "header without a colon",
            ));
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            let parsed = value.trim().parse::<usize>().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "content-length is not a number")
            })?;
            if parsed > MAX_BODY {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "content-length is larger than this server accepts",
                ));
            }
            length = Some(parsed);
        }
        // Any other header (`Content-Type`, vendor extensions) is ignored on purpose.
    }
    let Some(length) = length else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "header block has no content-length",
        ));
    };
    let mut body = vec![0u8; length];
    input.read_exact(&mut body)?;
    let body = String::from_utf8(body)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "body is not valid UTF-8"))?;
    Ok(Some(Message { body }))
}

/// Read one message from a raw byte stream (a pipe), buffering as needed.
pub fn read_message_from(input: &mut impl Read) -> io::Result<Option<Message>> {
    let mut reader = io::BufReader::new(input);
    read_message(&mut reader)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(body: &str) -> Message {
        let mut wire = Vec::new();
        write_message(&mut wire, body).expect("write");
        let mut cursor = io::Cursor::new(wire);
        read_message(&mut cursor).expect("read").expect("a message")
    }

    #[test]
    fn a_body_survives_a_round_trip() {
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#;
        assert_eq!(roundtrip(body).body, body);
    }

    #[test]
    fn the_length_counts_bytes_not_characters() {
        // Three characters, nine bytes: a reader that counts characters desynchronises here.
        let body = r#"{"a":"中文"}"#;
        assert!(body.len() > body.chars().count());
        let mut wire = Vec::new();
        write_message(&mut wire, body).expect("write");
        let header = String::from_utf8_lossy(&wire);
        assert!(
            header.starts_with(&format!("Content-Length: {}", body.len())),
            "the header is the byte length: {header}"
        );
        assert_eq!(roundtrip(body).body, body);
    }

    #[test]
    fn two_messages_in_one_stream_are_read_in_order() {
        let mut wire = Vec::new();
        write_message(&mut wire, r#"{"n":1}"#).expect("write");
        write_message(&mut wire, r#"{"n":2}"#).expect("write");
        let mut cursor = io::Cursor::new(wire);
        assert_eq!(
            read_message(&mut cursor).unwrap().unwrap().body,
            r#"{"n":1}"#
        );
        assert_eq!(
            read_message(&mut cursor).unwrap().unwrap().body,
            r#"{"n":2}"#
        );
        assert!(
            read_message(&mut cursor).unwrap().is_none(),
            "then end of stream"
        );
    }

    #[test]
    fn an_unknown_header_is_ignored() {
        let wire = b"Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: 2\r\n\r\n{}";
        let mut cursor = io::Cursor::new(&wire[..]);
        assert_eq!(read_message(&mut cursor).unwrap().unwrap().body, "{}");
    }

    #[test]
    fn the_header_name_is_case_insensitive() {
        let wire = b"content-length: 2\r\n\r\n{}";
        let mut cursor = io::Cursor::new(&wire[..]);
        assert_eq!(read_message(&mut cursor).unwrap().unwrap().body, "{}");
    }

    #[test]
    fn a_missing_length_is_an_error() {
        let wire = b"Content-Type: text/plain\r\n\r\n{}";
        let mut cursor = io::Cursor::new(&wire[..]);
        assert!(read_message(&mut cursor).is_err());
    }

    #[test]
    fn a_truncated_body_is_an_error() {
        let wire = b"Content-Length: 10\r\n\r\n{}";
        let mut cursor = io::Cursor::new(&wire[..]);
        assert!(read_message(&mut cursor).is_err());
    }

    #[test]
    fn a_body_that_is_not_utf8_is_an_error() {
        let wire = b"Content-Length: 2\r\n\r\n\xff\xfe";
        let mut cursor = io::Cursor::new(&wire[..]);
        assert!(read_message(&mut cursor).is_err());
    }

    #[test]
    fn an_absurd_length_is_refused_before_allocating() {
        let wire = format!("Content-Length: {}\r\n\r\n", MAX_BODY + 1);
        let mut cursor = io::Cursor::new(wire.as_bytes());
        let err = read_message(&mut cursor).expect_err("refused");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn an_empty_stream_is_a_clean_end() {
        let mut cursor = io::Cursor::new(&b""[..]);
        assert!(read_message(&mut cursor).unwrap().is_none());
    }

    #[test]
    fn a_stream_that_ends_inside_the_headers_is_an_error() {
        let wire = b"Content-Length: 2\r\n";
        let mut cursor = io::Cursor::new(&wire[..]);
        assert!(read_message(&mut cursor).is_err());
    }

    #[test]
    fn an_empty_body_is_a_message() {
        assert_eq!(roundtrip("").body, "");
    }
}
