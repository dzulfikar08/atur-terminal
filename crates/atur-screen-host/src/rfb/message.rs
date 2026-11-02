//! Bytes of RFB messages: parsing what the viewer sends, building what it receives.

use super::{ENC_H264, MAX_CLIPBOARD, Rect};

const MSG_SET_PIXEL_FORMAT: u8 = 0;
const MSG_SET_ENCODINGS: u8 = 2;
const MSG_UPDATE_REQUEST: u8 = 3;
const MSG_KEY_EVENT: u8 = 4;
const MSG_POINTER_EVENT: u8 = 5;
const MSG_CLIENT_CUT_TEXT: u8 = 6;
const MSG_ENABLE_CONTINUOUS_UPDATES: u8 = 150;
const MSG_FENCE: u8 = 248;
const MSG_SET_DESKTOP_SIZE: u8 = 251;
const MAX_ENCODINGS: usize = 1024;
const MAX_SCREENS: usize = 64;
const MAX_FENCE_PAYLOAD: usize = 64;
/// Room for the zlib framing of an extended clipboard message beyond the text.
const CLIPBOARD_OVERHEAD: usize = 4096;

/// How the viewer wants pixel values laid out. Only 32-bit true colour with 8
/// bits per channel is served; that is what the Atur Terminal viewers ask for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PixelFormat {
    pub big_endian: bool,
    pub red_shift: u8,
    pub green_shift: u8,
    pub blue_shift: u8,
}

impl PixelFormat {
    /// Memory order B, G, R, unused: the pictures' own layout.
    pub const NATIVE: Self = Self {
        big_endian: false,
        red_shift: 16,
        green_shift: 8,
        blue_shift: 0,
    };

    /// The 16 bytes of a `PIXEL_FORMAT` structure.
    #[must_use]
    pub fn to_bytes(self) -> [u8; 16] {
        let mut bytes = [0; 16];
        bytes[0] = 32;
        bytes[1] = 24;
        bytes[2] = u8::from(self.big_endian);
        bytes[3] = 1;
        for (index, max) in [(4, 255_u16), (6, 255), (8, 255)] {
            bytes[index..index + 2].copy_from_slice(&max.to_be_bytes());
        }
        bytes[10] = self.red_shift;
        bytes[11] = self.green_shift;
        bytes[12] = self.blue_shift;
        bytes
    }

    /// The viewer's choice, or `None` for a layout this server does not produce.
    #[must_use]
    pub fn from_bytes(bytes: &[u8; 16]) -> Option<Self> {
        let max = |at: usize| u16::from_be_bytes([bytes[at], bytes[at + 1]]);
        let shifts = [bytes[10], bytes[11], bytes[12]];
        let distinct = shifts[0] != shifts[1] && shifts[1] != shifts[2] && shifts[0] != shifts[2];
        let supported = bytes[0] == 32
            && bytes[3] != 0
            && max(4) == 255
            && max(6) == 255
            && max(8) == 255
            && distinct
            && shifts.iter().all(|shift| matches!(shift, 0 | 8 | 16 | 24));
        supported.then_some(Self {
            big_endian: bytes[2] != 0,
            red_shift: shifts[0],
            green_shift: shifts[1],
            blue_shift: shifts[2],
        })
    }

    /// One pixel of the picture (B, G, R) as the viewer wants its four bytes.
    #[must_use]
    pub fn pack(self, blue: u8, green: u8, red: u8) -> [u8; 4] {
        let value = (u32::from(red) << self.red_shift)
            | (u32::from(green) << self.green_shift)
            | (u32::from(blue) << self.blue_shift);
        if self.big_endian {
            value.to_be_bytes()
        } else {
            value.to_le_bytes()
        }
    }
}

/// A message from the viewer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClientMessage {
    SetPixelFormat(Option<PixelFormat>),
    SetEncodings(Vec<i32>),
    UpdateRequest {
        incremental: bool,
    },
    Key {
        down: bool,
        keysym: u32,
    },
    Pointer {
        buttons: u8,
        x: u16,
        y: u16,
    },
    /// Legacy clipboard text, Latin-1.
    CutText(String),
    /// Extended clipboard message body (everything after the negative length).
    ExtClipboard(Vec<u8>),
    EnableContinuousUpdates {
        enable: bool,
    },
    /// A fence the viewer asked to be answered; the flags are echoed back.
    Fence {
        flags: u32,
        payload: Vec<u8>,
    },
    /// Messages that are valid but of no use here.
    Ignored,
}

/// Cuts a byte stream into viewer messages, in any split.
#[derive(Debug, Default)]
pub struct ClientParser {
    input: Vec<u8>,
}

impl ClientParser {
    /// Adds bytes and returns every message that is now complete.
    ///
    /// # Errors
    /// A sentence for the log when the viewer sent something that is not RFB.
    pub fn feed(&mut self, data: &[u8]) -> Result<Vec<ClientMessage>, String> {
        self.input.extend_from_slice(data);
        let mut messages = Vec::new();
        let mut pos = 0;
        while let Some((message, used)) = parse_one(&self.input[pos..])? {
            messages.push(message);
            pos += used;
        }
        self.input.drain(..pos);
        Ok(messages)
    }
}

fn u16_at(data: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([data[at], data[at + 1]])
}

fn u32_at(data: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
}

/// One message at the start of `data` and its length; `None` when incomplete.
fn parse_one(data: &[u8]) -> Result<Option<(ClientMessage, usize)>, String> {
    let Some(&kind) = data.first() else {
        return Ok(None);
    };
    let message = match kind {
        MSG_SET_PIXEL_FORMAT => {
            let Some(bytes) = data.get(4..20) else {
                return Ok(None);
            };
            let mut format = [0_u8; 16];
            format.copy_from_slice(bytes);
            (
                ClientMessage::SetPixelFormat(PixelFormat::from_bytes(&format)),
                20,
            )
        }
        MSG_SET_ENCODINGS => {
            if data.len() < 4 {
                return Ok(None);
            }
            let count = usize::from(u16_at(data, 2));
            if count > MAX_ENCODINGS {
                return Err("the viewer listed too many encodings".into());
            }
            let total = 4 + count * 4;
            if data.len() < total {
                return Ok(None);
            }
            let list = (0..count)
                .map(|index| u32_at(data, 4 + index * 4).cast_signed())
                .collect();
            (ClientMessage::SetEncodings(list), total)
        }
        MSG_UPDATE_REQUEST => {
            if data.len() < 10 {
                return Ok(None);
            }
            (
                ClientMessage::UpdateRequest {
                    incremental: data[1] != 0,
                },
                10,
            )
        }
        MSG_KEY_EVENT => {
            if data.len() < 8 {
                return Ok(None);
            }
            (
                ClientMessage::Key {
                    down: data[1] != 0,
                    keysym: u32_at(data, 4),
                },
                8,
            )
        }
        MSG_POINTER_EVENT => {
            if data.len() < 6 {
                return Ok(None);
            }
            (
                ClientMessage::Pointer {
                    buttons: data[1],
                    x: u16_at(data, 2),
                    y: u16_at(data, 4),
                },
                6,
            )
        }
        MSG_CLIENT_CUT_TEXT => return parse_cut_text(data),
        MSG_ENABLE_CONTINUOUS_UPDATES => {
            if data.len() < 10 {
                return Ok(None);
            }
            (
                ClientMessage::EnableContinuousUpdates {
                    enable: data[1] != 0,
                },
                10,
            )
        }
        MSG_FENCE => {
            if data.len() < 9 {
                return Ok(None);
            }
            let length = usize::from(data[8]);
            if length > MAX_FENCE_PAYLOAD {
                return Err("the viewer sent an oversized fence".into());
            }
            if data.len() < 9 + length {
                return Ok(None);
            }
            (
                ClientMessage::Fence {
                    flags: u32_at(data, 4),
                    payload: data[9..9 + length].to_vec(),
                },
                9 + length,
            )
        }
        MSG_SET_DESKTOP_SIZE => {
            if data.len() < 8 {
                return Ok(None);
            }
            let screens = usize::from(data[6]);
            if screens > MAX_SCREENS {
                return Err("the viewer asked for too many screens".into());
            }
            let total = 8 + screens * 16;
            if data.len() < total {
                return Ok(None);
            }
            (ClientMessage::Ignored, total)
        }
        other => return Err(format!("the viewer sent an unknown message (type {other})")),
    };
    Ok(Some(message))
}

fn parse_cut_text(data: &[u8]) -> Result<Option<(ClientMessage, usize)>, String> {
    if data.len() < 8 {
        return Ok(None);
    }
    let length = u32_at(data, 4).cast_signed();
    let extended = length < 0;
    let body = usize::try_from(length.unsigned_abs()).unwrap_or(usize::MAX);
    let limit = if extended {
        MAX_CLIPBOARD + CLIPBOARD_OVERHEAD
    } else {
        MAX_CLIPBOARD
    };
    if body > limit {
        return Err("the viewer sent an oversized clipboard".into());
    }
    if data.len() < 8 + body {
        return Ok(None);
    }
    let bytes = &data[8..8 + body];
    let message = if extended {
        ClientMessage::ExtClipboard(bytes.to_vec())
    } else {
        ClientMessage::CutText(bytes.iter().map(|&byte| char::from(byte)).collect())
    };
    Ok(Some((message, 8 + body)))
}

/// `ServerInit` for a picture of `width` x `height`.
#[must_use]
pub fn server_init(width: u16, height: u16, format: PixelFormat, name: &str) -> Vec<u8> {
    let name = name.as_bytes();
    let length = u32::try_from(name.len()).unwrap_or(0);
    let name = &name[..usize::try_from(length).unwrap_or(0)];
    let mut out = Vec::with_capacity(24 + name.len());
    out.extend_from_slice(&width.to_be_bytes());
    out.extend_from_slice(&height.to_be_bytes());
    out.extend_from_slice(&format.to_bytes());
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(name);
    out
}

/// The header of a `FramebufferUpdate` with `rects` rectangles.
pub fn update_header(out: &mut Vec<u8>, rects: u16) {
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&rects.to_be_bytes());
}

/// The header of one rectangle.
pub fn rect_header(out: &mut Vec<u8>, rect: Rect, encoding: i32) {
    for value in [rect.x, rect.y, rect.w, rect.h] {
        out.extend_from_slice(&u16::try_from(value).unwrap_or(u16::MAX).to_be_bytes());
    }
    out.extend_from_slice(&encoding.to_be_bytes());
}

/// A `FramebufferUpdate` telling the viewer the picture is now `width` x `height`.
#[must_use]
pub fn desktop_size(width: usize, height: usize, extended: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    update_header(&mut out, 1);
    let rect = Rect {
        x: 0,
        y: 0,
        w: width,
        h: height,
    };
    if extended {
        // x = 0: the change came from the server; one screen of the whole picture.
        rect_header(&mut out, rect, super::ENC_EXT_DESKTOP_SIZE);
        out.extend_from_slice(&[1, 0, 0, 0]);
        out.extend_from_slice(&0_u32.to_be_bytes());
        out.extend_from_slice(&0_u16.to_be_bytes());
        out.extend_from_slice(&0_u16.to_be_bytes());
        out.extend_from_slice(&u16::try_from(width).unwrap_or(u16::MAX).to_be_bytes());
        out.extend_from_slice(&u16::try_from(height).unwrap_or(u16::MAX).to_be_bytes());
        out.extend_from_slice(&0_u32.to_be_bytes());
    } else {
        rect_header(&mut out, rect, super::ENC_DESKTOP_SIZE);
    }
    out
}

/// A `FramebufferUpdate` of one H.264 rectangle. `reset` tells the viewer's
/// decoder this is a new stream and its old reference pictures are gone.
#[must_use]
pub fn h264_update(width: usize, height: usize, access_unit: &[u8], reset: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + access_unit.len());
    update_header(&mut out, 1);
    rect_header(
        &mut out,
        Rect {
            x: 0,
            y: 0,
            w: width,
            h: height,
        },
        ENC_H264,
    );
    out.extend_from_slice(&u32::try_from(access_unit.len()).unwrap_or(0).to_be_bytes());
    out.extend_from_slice(&u32::from(reset).to_be_bytes());
    out.extend_from_slice(access_unit);
    out
}

/// The single byte with which a server says it understands continuous updates.
pub const END_OF_CONTINUOUS_UPDATES: [u8; 1] = [MSG_ENABLE_CONTINUOUS_UPDATES];

/// Legacy `ServerCutText`: Latin-1, other characters become `?`.
#[must_use]
pub fn server_cut_text(text: &str) -> Option<Vec<u8>> {
    let bytes: Vec<u8> = text
        .chars()
        .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
        .collect();
    if bytes.len() > MAX_CLIPBOARD {
        return None;
    }
    let mut out = vec![3, 0, 0, 0];
    out.extend_from_slice(&u32::try_from(bytes.len()).ok()?.to_be_bytes());
    out.extend_from_slice(&bytes);
    Some(out)
}

/// The reply to a fence request: the request bit is cleared, the payload echoed.
#[must_use]
pub fn fence_reply(flags: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![MSG_FENCE, 0, 0, 0];
    out.extend_from_slice(&(flags & 0x7).to_be_bytes());
    out.push(u8::try_from(payload.len()).unwrap_or(0));
    out.extend_from_slice(payload);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_viewers_pixel_format_is_native() {
        // As crates/atur-screen sends it: 32 bpp, depth 24, little endian, R16 G8 B0.
        let wire = [32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0];
        assert_eq!(PixelFormat::from_bytes(&wire), Some(PixelFormat::NATIVE));
        assert_eq!(PixelFormat::NATIVE.to_bytes()[..13], wire[..13]);
    }

    #[test]
    fn other_layouts_are_refused_or_converted() {
        let mut wire = PixelFormat::NATIVE.to_bytes();
        wire[0] = 16;
        assert_eq!(PixelFormat::from_bytes(&wire), None);
        let mut wire = PixelFormat::NATIVE.to_bytes();
        wire[3] = 0;
        assert_eq!(PixelFormat::from_bytes(&wire), None);
        let rgba = PixelFormat {
            big_endian: true,
            red_shift: 24,
            green_shift: 16,
            blue_shift: 8,
        };
        assert_eq!(PixelFormat::from_bytes(&rgba.to_bytes()), Some(rgba));
        assert_eq!(rgba.pack(1, 2, 3), [3, 2, 1, 0]);
        assert_eq!(PixelFormat::NATIVE.pack(1, 2, 3), [1, 2, 3, 0]);
    }

    #[test]
    fn messages_parse_in_any_split() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&[2, 0, 0, 2, 0, 0, 0, 50, 0xff, 0xff, 0xff, 0x21]);
        stream.extend_from_slice(&[3, 1, 0, 0, 0, 0, 0x07, 0x80, 0x04, 0x38]);
        stream.extend_from_slice(&[4, 1, 0, 0, 0, 0, 0xff, 0x0d]);
        stream.extend_from_slice(&[5, 5, 1, 2, 0, 3]);
        stream.extend_from_slice(&[150, 1, 0, 0, 0, 0, 1, 0, 0, 2]);
        stream.extend_from_slice(&[6, 0, 0, 0, 0, 0, 0, 2, b'h', 0xe9]);
        let mut parser = ClientParser::default();
        let mut all = Vec::new();
        for byte in &stream {
            all.extend(parser.feed(&[*byte]).unwrap());
        }
        assert_eq!(
            all,
            vec![
                ClientMessage::SetEncodings(vec![50, -223]),
                ClientMessage::UpdateRequest { incremental: true },
                ClientMessage::Key {
                    down: true,
                    keysym: 0xff0d
                },
                ClientMessage::Pointer {
                    buttons: 5,
                    x: 258,
                    y: 3
                },
                ClientMessage::EnableContinuousUpdates { enable: true },
                ClientMessage::CutText("h\u{e9}".into()),
            ]
        );
    }

    #[test]
    fn extended_clipboard_keeps_its_body() {
        let mut parser = ClientParser::default();
        let messages = parser
            .feed(&[6, 0, 0, 0, 0xff, 0xff, 0xff, 0xfc, 2, 0, 0, 1])
            .unwrap();
        assert_eq!(
            messages,
            vec![ClientMessage::ExtClipboard(vec![2, 0, 0, 1])]
        );
    }

    #[test]
    fn nonsense_and_oversize_are_errors() {
        assert!(ClientParser::default().feed(&[99]).is_err());
        assert!(
            ClientParser::default()
                .feed(&[6, 0, 0, 0, 0x7f, 0, 0, 0])
                .is_err()
        );
        let mut many = vec![2, 0, 0xff, 0xff];
        many.extend(std::iter::repeat_n(0, 8));
        assert!(ClientParser::default().feed(&many).is_err());
    }

    #[test]
    fn server_messages_have_the_wire_layout() {
        let init = server_init(1280, 720, PixelFormat::NATIVE, "Air");
        assert_eq!(init.len(), 24 + 3);
        assert_eq!(&init[..4], [5, 0, 2, 0xd0]);
        assert_eq!(&init[20..24], 3_u32.to_be_bytes());
        let update = h264_update(1280, 720, &[0, 0, 0, 1, 0x65], true);
        assert_eq!(update[0], 0);
        assert_eq!(&update[2..4], 1_u16.to_be_bytes());
        assert_eq!(&update[12..16], 50_i32.to_be_bytes());
        assert_eq!(&update[16..20], 5_u32.to_be_bytes());
        assert_eq!(&update[20..24], 1_u32.to_be_bytes());
        assert_eq!(&update[24..], [0, 0, 0, 1, 0x65]);
        let size = desktop_size(800, 600, false);
        assert_eq!(&size[12..16], (-223_i32).to_be_bytes());
        let extended = desktop_size(800, 600, true);
        assert_eq!(&extended[12..16], (-308_i32).to_be_bytes());
        assert_eq!(extended[16], 1);
        assert_eq!(
            server_cut_text("h\u{20ac}").unwrap(),
            [3, 0, 0, 0, 0, 0, 0, 2, b'h', b'?']
        );
        assert_eq!(
            fence_reply(0x8000_0001, b"ab"),
            [248, 0, 0, 0, 0, 0, 0, 1, 2, b'a', b'b']
        );
    }
}
