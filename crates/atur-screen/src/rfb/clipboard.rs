//! Extended clipboard pseudo-encoding (text only), both directions.

use std::io::Write;

use flate2::write::ZlibEncoder;
use flate2::{Compression, Decompress};

use super::MAX_CLIPBOARD;
use super::inflate::inflate_into;
use super::msg::MSG_CLIENT_CUT_TEXT;

pub(super) const FORMAT_TEXT: u32 = 1;
pub(super) const CAPS: u32 = 1 << 24;
pub(super) const REQUEST: u32 = 1 << 25;
pub(super) const PEEK: u32 = 1 << 26;
pub(super) const NOTIFY: u32 = 1 << 27;
pub(super) const PROVIDE: u32 = 1 << 28;

/// What an extended clipboard message from the server asks for or carries.
#[derive(Debug, Eq, PartialEq)]
pub(super) enum Incoming {
    /// The server announced which formats and actions it accepts.
    Caps(u32),
    /// The server wants our text.
    Request,
    /// The server wants to know which formats we have.
    Peek,
    /// The server has new clipboard content; `text` says whether it offers text.
    Notify { text: bool },
    /// The server sent its text.
    Provide(String),
    /// Anything else (other formats, damaged data).
    Ignored,
}

/// Whether the server's capability flags let us notify it of text and request text from it.
pub(super) fn server_accepts_text_flow(flags: u32) -> bool {
    flags & FORMAT_TEXT != 0 && flags & NOTIFY != 0 && flags & REQUEST != 0
}

/// Interprets the body of an extended `ServerCutText` (everything after the length field).
pub(super) fn parse_incoming(body: &[u8]) -> Incoming {
    let Some(flag_bytes) = body.first_chunk::<4>() else {
        return Incoming::Ignored;
    };
    let flags = u32::from_be_bytes(*flag_bytes);
    if flags & CAPS != 0 {
        Incoming::Caps(flags)
    } else if flags & REQUEST != 0 && flags & FORMAT_TEXT != 0 {
        Incoming::Request
    } else if flags & PEEK != 0 {
        Incoming::Peek
    } else if flags & NOTIFY != 0 {
        Incoming::Notify {
            text: flags & FORMAT_TEXT != 0,
        }
    } else if flags & PROVIDE != 0 && flags & FORMAT_TEXT != 0 {
        body.get(4..)
            .and_then(parse_provided_text)
            .map_or(Incoming::Ignored, Incoming::Provide)
    } else {
        Incoming::Ignored
    }
}

/// The zlib payload of a Provide message: `u32 size`, then `size` bytes of UTF-8 text
/// (CRLF line ends, NUL terminated) for the text format, which always comes first.
fn parse_provided_text(compressed: &[u8]) -> Option<String> {
    let mut stream = Decompress::new(true);
    let mut data = Vec::new();
    inflate_into(&mut stream, compressed, MAX_CLIPBOARD + 8, &mut data).ok()?;
    let size_bytes = data.first_chunk::<4>()?;
    let size = usize::try_from(u32::from_be_bytes(*size_bytes)).ok()?;
    if size > MAX_CLIPBOARD {
        return None;
    }
    let mut text = data.get(4..4usize.checked_add(size)?)?;
    while let Some((&0, rest)) = text.split_last() {
        text = rest;
    }
    Some(String::from_utf8_lossy(text).replace("\r\n", "\n"))
}

fn header(out: &mut Vec<u8>, body_len: usize) {
    out.extend_from_slice(&[MSG_CLIENT_CUT_TEXT, 0, 0, 0]);
    let length = i32::try_from(body_len).unwrap_or(i32::MAX);
    out.extend_from_slice(&(-length).to_be_bytes());
}

/// Our capabilities: text only, every action; a maximum unsolicited size of 0 makes the
/// server announce changes with Notify so that the flow is Notify, Request, Provide.
pub(super) fn caps(out: &mut Vec<u8>) {
    header(out, 8);
    out.extend_from_slice(&(CAPS | REQUEST | PEEK | NOTIFY | PROVIDE | FORMAT_TEXT).to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
}

pub(super) fn request(out: &mut Vec<u8>) {
    header(out, 4);
    out.extend_from_slice(&(REQUEST | FORMAT_TEXT).to_be_bytes());
}

/// Tells the server that the local clipboard changed (`has_text`) or is empty.
pub(crate) fn notify(out: &mut Vec<u8>, has_text: bool) {
    header(out, 4);
    let flags = if has_text {
        NOTIFY | FORMAT_TEXT
    } else {
        NOTIFY
    };
    out.extend_from_slice(&flags.to_be_bytes());
}

/// Sends `text` (LF converted to CRLF, NUL terminated, deflated). Returns `false` and writes
/// nothing when the text is too long or cannot be compressed.
pub(crate) fn provide(out: &mut Vec<u8>, text: &str) -> bool {
    if text.len() > MAX_CLIPBOARD {
        return false;
    }
    let mut payload = Vec::with_capacity(text.len() + 8);
    payload.extend_from_slice(&[0, 0, 0, 0]);
    let mut previous = 0u8;
    for &byte in text.as_bytes() {
        if byte == b'\n' && previous != b'\r' {
            payload.push(b'\r');
        }
        payload.push(byte);
        previous = byte;
    }
    payload.push(0);
    let Ok(size) = u32::try_from(payload.len() - 4) else {
        return false;
    };
    payload[..4].copy_from_slice(&size.to_be_bytes());
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
    if encoder.write_all(&payload).is_err() {
        return false;
    }
    let Ok(compressed) = encoder.finish() else {
        return false;
    };
    header(out, 4 + compressed.len());
    out.extend_from_slice(&(PROVIDE | FORMAT_TEXT).to_be_bytes());
    out.extend_from_slice(&compressed);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(out: &[u8]) -> &[u8] {
        &out[8..]
    }

    #[test]
    fn caps_wire_bytes() {
        let mut out = Vec::new();
        caps(&mut out);
        assert_eq!(&out[..8], [6, 0, 0, 0, 0xff, 0xff, 0xff, 0xf8]);
        assert_eq!(&out[8..12], 0x1f00_0001u32.to_be_bytes());
        assert_eq!(&out[12..], [0, 0, 0, 0]);
        assert_eq!(parse_incoming(body(&out)), Incoming::Caps(0x1f00_0001));
    }

    #[test]
    fn request_notify_wire_bytes() {
        let mut out = Vec::new();
        request(&mut out);
        assert_eq!(out, [6, 0, 0, 0, 0xff, 0xff, 0xff, 0xfc, 2, 0, 0, 1]);
        assert_eq!(parse_incoming(body(&out)), Incoming::Request);
        out.clear();
        notify(&mut out, true);
        assert_eq!(out, [6, 0, 0, 0, 0xff, 0xff, 0xff, 0xfc, 8, 0, 0, 1]);
        assert_eq!(parse_incoming(body(&out)), Incoming::Notify { text: true });
        out.clear();
        notify(&mut out, false);
        assert_eq!(parse_incoming(body(&out)), Incoming::Notify { text: false });
    }

    #[test]
    fn provide_round_trip_converts_line_ends() {
        let mut out = Vec::new();
        assert!(provide(&mut out, "one\ntwo \u{20ac}"));
        assert_eq!(out[0], 6);
        let length = i32::from_be_bytes([out[4], out[5], out[6], out[7]]);
        assert_eq!(usize::try_from(-length).unwrap(), out.len() - 8);
        assert_eq!(
            parse_incoming(body(&out)),
            Incoming::Provide("one\ntwo \u{20ac}".to_owned())
        );
    }

    #[test]
    fn damaged_provide_is_ignored() {
        let mut out = Vec::new();
        assert!(provide(&mut out, "text"));
        let cut = out.len() - 8;
        assert_eq!(parse_incoming(&out[8..cut]), Incoming::Ignored);
        assert_eq!(parse_incoming(&[1, 2]), Incoming::Ignored);
    }

    #[test]
    fn oversized_text_is_not_sent() {
        let mut out = Vec::new();
        assert!(!provide(&mut out, &"a".repeat(MAX_CLIPBOARD + 1)));
        assert!(out.is_empty());
    }
}
