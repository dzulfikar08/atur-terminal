//! The extended clipboard pseudo-encoding, text only, from the server's side.
//!
//! The flow is Notify (text changed), Request, Provide, in either direction.

use std::io::{Read, Write};

use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;

use super::MAX_CLIPBOARD;

const SERVER_CUT_TEXT: u8 = 3;
const FORMAT_TEXT: u32 = 1;
const CAPS: u32 = 1 << 24;
const REQUEST: u32 = 1 << 25;
const PEEK: u32 = 1 << 26;
const NOTIFY: u32 = 1 << 27;
const PROVIDE: u32 = 1 << 28;

/// What an extended clipboard message from the viewer asks for or carries.
#[derive(Debug, Eq, PartialEq)]
pub enum Incoming {
    /// The viewer announced what it accepts.
    Caps,
    /// The viewer wants our text.
    Request,
    /// The viewer wants to know which formats we have.
    Peek,
    /// The viewer's clipboard changed; `text` says whether it offers text.
    Notify { text: bool },
    /// The viewer sent its text.
    Provide(String),
    /// Anything else.
    Ignored,
}

/// Interprets the body of an extended `ClientCutText` (after the negative length).
#[must_use]
pub fn parse_incoming(body: &[u8]) -> Incoming {
    let Some(flag_bytes) = body.first_chunk::<4>() else {
        return Incoming::Ignored;
    };
    let flags = u32::from_be_bytes(*flag_bytes);
    if flags & CAPS != 0 {
        Incoming::Caps
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
            .and_then(provided_text)
            .map_or(Incoming::Ignored, Incoming::Provide)
    } else {
        Incoming::Ignored
    }
}

/// The zlib payload of a Provide message: `u32 size`, then the text as UTF-8
/// with CRLF line ends and a NUL at the end.
fn provided_text(compressed: &[u8]) -> Option<String> {
    let limit = u64::try_from(MAX_CLIPBOARD + 8).ok()?;
    let mut data = Vec::new();
    ZlibDecoder::new(compressed)
        .take(limit)
        .read_to_end(&mut data)
        .ok()?;
    let size = usize::try_from(u32::from_be_bytes(*data.first_chunk::<4>()?)).ok()?;
    if size > MAX_CLIPBOARD {
        return None;
    }
    let mut text = data.get(4..4usize.checked_add(size)?)?;
    while let Some((&0, rest)) = text.split_last() {
        text = rest;
    }
    Some(String::from_utf8_lossy(text).replace("\r\n", "\n"))
}

fn message(body: &[u8]) -> Vec<u8> {
    let length = i32::try_from(body.len()).unwrap_or(i32::MAX);
    let mut out = vec![SERVER_CUT_TEXT, 0, 0, 0];
    out.extend_from_slice(&(-length).to_be_bytes());
    out.extend_from_slice(body);
    out
}

/// What this server accepts: text, every action. A maximum unsolicited size of
/// 0 makes the viewer announce changes with Notify, as the flow expects.
#[must_use]
pub fn caps() -> Vec<u8> {
    let flags = CAPS | REQUEST | PEEK | NOTIFY | PROVIDE | FORMAT_TEXT;
    let mut body = flags.to_be_bytes().to_vec();
    body.extend_from_slice(&0_u32.to_be_bytes());
    message(&body)
}

/// Asks the viewer for its text.
#[must_use]
pub fn request() -> Vec<u8> {
    message(&(REQUEST | FORMAT_TEXT).to_be_bytes())
}

/// Tells the viewer that the clipboard here changed, or is empty.
#[must_use]
pub fn notify(has_text: bool) -> Vec<u8> {
    let flags = if has_text {
        NOTIFY | FORMAT_TEXT
    } else {
        NOTIFY
    };
    message(&flags.to_be_bytes())
}

/// Sends `text` (LF converted to CRLF, NUL terminated, deflated). `None` when it
/// is too long or cannot be compressed.
#[must_use]
pub fn provide(text: &str) -> Option<Vec<u8>> {
    if text.len() > MAX_CLIPBOARD {
        return None;
    }
    let mut payload = Vec::with_capacity(text.len() + 8);
    payload.extend_from_slice(&[0, 0, 0, 0]);
    let mut previous = 0_u8;
    for &byte in text.as_bytes() {
        if byte == b'\n' && previous != b'\r' {
            payload.push(b'\r');
        }
        payload.push(byte);
        previous = byte;
    }
    payload.push(0);
    let size = u32::try_from(payload.len() - 4).ok()?;
    payload[..4].copy_from_slice(&size.to_be_bytes());
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(&payload).ok()?;
    let compressed = encoder.finish().ok()?;
    let mut body = (PROVIDE | FORMAT_TEXT).to_be_bytes().to_vec();
    body.extend_from_slice(&compressed);
    Some(message(&body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(out: &[u8]) -> &[u8] {
        &out[8..]
    }

    #[test]
    fn the_flow_messages_are_what_the_viewer_expects() {
        assert_eq!(&caps()[..8], [3, 0, 0, 0, 0xff, 0xff, 0xff, 0xf8]);
        assert_eq!(&caps()[8..12], 0x1f00_0001_u32.to_be_bytes());
        assert_eq!(request(), [3, 0, 0, 0, 0xff, 0xff, 0xff, 0xfc, 2, 0, 0, 1]);
        assert_eq!(
            notify(true),
            [3, 0, 0, 0, 0xff, 0xff, 0xff, 0xfc, 8, 0, 0, 1]
        );
        assert_eq!(parse_incoming(body(&caps())), Incoming::Caps);
        assert_eq!(parse_incoming(body(&request())), Incoming::Request);
        assert_eq!(
            parse_incoming(body(&notify(true))),
            Incoming::Notify { text: true }
        );
        assert_eq!(
            parse_incoming(body(&notify(false))),
            Incoming::Notify { text: false }
        );
    }

    #[test]
    fn text_survives_provide_with_its_line_ends() {
        let out = provide("one\ntwo \u{20ac}").unwrap();
        assert_eq!(
            parse_incoming(body(&out)),
            Incoming::Provide("one\ntwo \u{20ac}".to_owned())
        );
    }

    #[test]
    fn damaged_or_oversized_text_is_not_accepted() {
        let out = provide("text").unwrap();
        assert_eq!(parse_incoming(&out[8..16]), Incoming::Ignored);
        assert_eq!(parse_incoming(&[1, 2]), Incoming::Ignored);
        assert!(provide(&"a".repeat(MAX_CLIPBOARD + 1)).is_none());
    }
}
