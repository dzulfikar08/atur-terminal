//! A sans-IO RFB 3.8 client: bytes in, framebuffer changes, events and reply bytes out.
//!
//! [`Machine::feed`] accepts any split of the server's byte stream (one byte at a
//! time works), parses as far as the buffered bytes allow and keeps the rest for the next call.

pub(crate) mod clipboard;
mod inflate;
pub(crate) mod msg;
#[cfg(test)]
mod tests;
mod zrle;

use std::fmt;

use crate::decode::H264Decoder;
use msg::{
    ENC_CONTINUOUS_UPDATES, ENC_COPY_RECT, ENC_DESKTOP_SIZE, ENC_EXT_DESKTOP_SIZE, ENC_FENCE,
    ENC_H264, ENC_LAST_RECT, ENC_RAW, ENC_ZRLE,
};
use zrle::Zrle;

/// Largest clipboard text, in bytes, that is forwarded in either direction.
pub(crate) const MAX_CLIPBOARD: usize = 1 << 20;
const MAX_DIMENSION: usize = 16_384;
const MAX_PIXELS: usize = 64 * 1024 * 1024;
const MAX_ZRLE: usize = 16 << 20;
const MAX_H264: usize = 16 << 20;
const MAX_NAME: usize = 64 * 1024;
const MAX_REASON: usize = 4096;
const MAX_FENCE_PAYLOAD: usize = 64;
const MAX_UNBOUNDED_RECTS: u32 = 1 << 20;
/// Slack on top of [`MAX_CLIPBOARD`] for the compressed form of an extended clipboard message.
const CLIPBOARD_OVERHEAD: usize = 4096;
const KEEP_INPUT_CAPACITY: usize = 1 << 20;
const FENCE_REQUEST: u32 = 1 << 31;
/// The fence flags this client implements: block before, block after, sync next.
const FENCE_UNDERSTOOD: u32 = 0x7;
const UNBOUNDED_RECT_COUNT: u32 = 0xffff;

/// The server violated the protocol or sent something this client cannot handle.
/// The message is a complete English sentence meant for the user.
#[derive(Debug)]
pub(crate) struct ProtocolError(String);

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

pub(crate) fn protocol_error(message: impl Into<String>) -> ProtocolError {
    ProtocolError(message.into())
}

/// A rectangle in framebuffer pixels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Rect {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

/// 32-bit BGRA pixels, alpha always 255.
struct Framebuffer {
    width: usize,
    height: usize,
    data: Vec<u8>,
}

impl Framebuffer {
    fn new(width: usize, height: usize) -> Self {
        let mut data = vec![0u8; width * height * 4];
        for pixel in data.chunks_exact_mut(4) {
            pixel[3] = 255;
        }
        Self {
            width,
            height,
            data,
        }
    }
}

/// How the pixels of an update were encoded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Codec {
    Raw,
    Zrle,
    H264,
}

/// Something the session has to act on.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Event {
    /// `ServerInit` arrived: the connection is usable.
    Ready {
        width: usize,
        height: usize,
        name: String,
    },
    /// The remote screen changed size; the framebuffer was reallocated.
    Resized { width: usize, height: usize },
    /// A `FramebufferUpdate` ended. `dirty` means the framebuffer bytes changed (H.264
    /// pictures arrive through the decoder's sink instead).
    Update { codec: Option<Codec>, dirty: bool },
    /// Remote clipboard text.
    Clipboard(String),
    /// The server's extended clipboard capabilities arrived; `true` when the
    /// Notify / Request / Provide flow with text works.
    ExtClipboard(bool),
    /// The server wants the local clipboard text (answer with a Provide message).
    ClipboardRequest,
    /// The server asks which clipboard formats exist locally (answer with a Notify message).
    ClipboardPeek,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    Version,
    SecurityTypes,
    SecurityResult,
    ServerInit,
    Message,
    /// Between the rectangles of a `FramebufferUpdate`.
    Rects,
    /// Inside a raw rectangle, `row` rows already painted.
    Raw {
        rect: Rect,
        row: usize,
    },
    /// Discarding the rest of an oversized message.
    Skip(u64),
    Failed,
}

#[derive(Default)]
struct UpdateInfo {
    codec: Option<Codec>,
    dirty: bool,
}

/// The client side of one RFB connection.
pub(crate) struct Machine {
    input: Vec<u8>,
    pos: usize,
    state: State,
    out: Vec<u8>,
    events: Vec<Event>,
    minor: u8,
    fb: Framebuffer,
    zrle: Zrle,
    h264: Option<Box<dyn H264Decoder>>,
    continuous: bool,
    rects_left: u32,
    unbounded: bool,
    rect_count: u32,
    update: UpdateInfo,
}

impl Machine {
    /// `h264` is the decoder to use; encoding 50 is requested exactly when it is `Some`.
    pub(crate) fn new(h264: Option<Box<dyn H264Decoder>>) -> Self {
        Self {
            input: Vec::new(),
            pos: 0,
            state: State::Version,
            out: Vec::new(),
            events: Vec::new(),
            minor: 8,
            fb: Framebuffer::new(0, 0),
            zrle: Zrle::new(),
            h264,
            continuous: false,
            rects_left: 0,
            unbounded: false,
            rect_count: 0,
            update: UpdateInfo::default(),
        }
    }

    /// Name of the H.264 decoder, if one is attached.
    pub(crate) fn h264_name(&self) -> Option<String> {
        self.h264.as_ref().map(|decoder| decoder.name())
    }

    /// Feeds the next bytes of the server stream and parses everything that is complete.
    pub(crate) fn feed(&mut self, data: &[u8]) -> Result<(), ProtocolError> {
        if self.state == State::Failed {
            return Err(protocol_error("The screen connection already failed."));
        }
        self.input.extend_from_slice(data);
        let result = loop {
            match self.step() {
                Ok(true) => {}
                Ok(false) => break Ok(()),
                Err(error) => break Err(error),
            }
        };
        if result.is_err() {
            self.state = State::Failed;
        }
        self.input.drain(..self.pos);
        self.pos = 0;
        if self.input.is_empty() && self.input.capacity() > KEEP_INPUT_CAPACITY {
            self.input = Vec::new();
        }
        result
    }

    /// Bytes to send to the server, in order.
    pub(crate) fn take_out(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
    }

    pub(crate) fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    /// Framebuffer width and height in pixels.
    pub(crate) fn size(&self) -> (usize, usize) {
        (self.fb.width, self.fb.height)
    }

    /// The framebuffer as packed B, G, R, 255 bytes.
    pub(crate) fn pixels(&self) -> &[u8] {
        &self.fb.data
    }

    /// Parses one step; `Ok(false)` means more bytes are needed.
    fn step(&mut self) -> Result<bool, ProtocolError> {
        match self.state {
            State::Version => self.step_version(),
            State::SecurityTypes => self.step_security_types(),
            State::SecurityResult => self.step_security_result(),
            State::ServerInit => self.step_server_init(),
            State::Message => self.step_message(),
            State::Rects => self.step_rect(),
            State::Raw { rect, row } => self.step_raw(rect, row),
            State::Skip(remaining) => Ok(self.step_skip(remaining)),
            State::Failed => Err(protocol_error("The screen connection already failed.")),
        }
    }

    fn step_version(&mut self) -> Result<bool, ProtocolError> {
        let data = tail(&self.input, self.pos);
        if data.len() < 12 {
            return Ok(false);
        }
        let Some((major, minor)) = parse_version(&data[..12]) else {
            return Err(protocol_error(
                "The remote end did not answer like a screen server (no RFB greeting).",
            ));
        };
        if major < 3 || (major == 3 && minor < 7) {
            return Err(protocol_error(format!(
                "The screen server speaks RFB {major}.{minor}, which is too old."
            )));
        }
        self.minor = if major > 3 || minor >= 8 { 8 } else { 7 };
        let greeting: &[u8] = if self.minor == 8 {
            b"RFB 003.008\n"
        } else {
            b"RFB 003.007\n"
        };
        self.out.extend_from_slice(greeting);
        self.pos += 12;
        self.state = State::SecurityTypes;
        Ok(true)
    }

    fn step_security_types(&mut self) -> Result<bool, ProtocolError> {
        let data = tail(&self.input, self.pos);
        let Some(&count) = data.first() else {
            return Ok(false);
        };
        let count = usize::from(count);
        if count == 0 {
            return match read_reason(data, 1) {
                Some(reason) => Err(refused(&reason)),
                None => Ok(false),
            };
        }
        if data.len() < 1 + count {
            return Ok(false);
        }
        let offered = &data[1..=count];
        if !offered.contains(&1) {
            return Err(protocol_error(format!(
                "The screen server requires a login this viewer does not support \
                 (security types {offered:?})."
            )));
        }
        self.pos += 1 + count;
        self.out.push(1);
        if self.minor >= 8 {
            self.state = State::SecurityResult;
        } else {
            self.out.push(1);
            self.state = State::ServerInit;
        }
        Ok(true)
    }

    fn step_security_result(&mut self) -> Result<bool, ProtocolError> {
        let data = tail(&self.input, self.pos);
        if data.len() < 4 {
            return Ok(false);
        }
        if u32_at(data, 0) == 0 {
            self.pos += 4;
            self.out.push(1);
            self.state = State::ServerInit;
            return Ok(true);
        }
        match read_reason(data, 4) {
            Some(reason) => Err(refused(&reason)),
            None => Ok(false),
        }
    }

    fn step_server_init(&mut self) -> Result<bool, ProtocolError> {
        let data = tail(&self.input, self.pos);
        if data.len() < 24 {
            return Ok(false);
        }
        let width = usize::from(u16_at(data, 0));
        let height = usize::from(u16_at(data, 2));
        let name_length = to_usize(u32_at(data, 20));
        if name_length > MAX_NAME {
            return Err(protocol_error("The screen server sent an oversized name."));
        }
        validate_size(width, height)?;
        if data.len() < 24 + name_length {
            return Ok(false);
        }
        let name = String::from_utf8_lossy(&data[24..24 + name_length]).into_owned();
        self.pos += 24 + name_length;
        self.fb = Framebuffer::new(width, height);
        msg::set_pixel_format(&mut self.out);
        msg::set_encodings(&mut self.out, &msg::encodings(self.h264.is_some()));
        msg::update_request(&mut self.out, false, width, height);
        self.events.push(Event::Ready {
            width,
            height,
            name,
        });
        self.state = State::Message;
        Ok(true)
    }

    fn step_message(&mut self) -> Result<bool, ProtocolError> {
        let data = tail(&self.input, self.pos);
        let Some(&kind) = data.first() else {
            return Ok(false);
        };
        match kind {
            0 => {
                if data.len() < 4 {
                    return Ok(false);
                }
                let count = u32::from(u16_at(data, 2));
                self.pos += 4;
                self.begin_update(count);
                Ok(true)
            }
            1 => {
                if data.len() < 6 {
                    return Ok(false);
                }
                let total = 6 + usize::from(u16_at(data, 4)) * 6;
                if data.len() < total {
                    return Ok(false);
                }
                self.pos += total;
                Ok(true)
            }
            2 => {
                self.pos += 1;
                Ok(true)
            }
            3 => Ok(self.step_cut_text()),
            msg::MSG_ENABLE_CONTINUOUS_UPDATES => {
                self.pos += 1;
                self.continuous_signal();
                Ok(true)
            }
            msg::MSG_FENCE => self.step_fence(),
            other => Err(protocol_error(format!(
                "The screen server sent a message this viewer does not know (type {other})."
            ))),
        }
    }

    fn step_fence(&mut self) -> Result<bool, ProtocolError> {
        let data = tail(&self.input, self.pos);
        if data.len() < 9 {
            return Ok(false);
        }
        let length = usize::from(data[8]);
        if length > MAX_FENCE_PAYLOAD {
            return Err(protocol_error("The screen server sent an oversized fence."));
        }
        if data.len() < 9 + length {
            return Ok(false);
        }
        let flags = u32_at(data, 4);
        if flags & FENCE_REQUEST != 0 {
            msg::fence(
                &mut self.out,
                flags & FENCE_UNDERSTOOD,
                &data[9..9 + length],
            );
        }
        self.pos += 9 + length;
        Ok(true)
    }

    /// `ServerCutText`, legacy (Latin-1) or extended. Returns `false` when it needs more bytes.
    fn step_cut_text(&mut self) -> bool {
        let data = tail(&self.input, self.pos);
        if data.len() < 8 {
            return false;
        }
        let length = i32_at(data, 4);
        let extended = length < 0;
        let body = u64::from(length.unsigned_abs());
        let limit = if extended {
            MAX_CLIPBOARD + CLIPBOARD_OVERHEAD
        } else {
            MAX_CLIPBOARD
        };
        let Some(body_length) = usize::try_from(body).ok().filter(|&length| length <= limit) else {
            self.pos += 8;
            self.state = State::Skip(body);
            return true;
        };
        if data.len() < 8 + body_length {
            return false;
        }
        let body_bytes = &data[8..8 + body_length];
        if extended {
            extended_clipboard(body_bytes, &mut self.out, &mut self.events);
        } else if !body_bytes.is_empty() {
            let text: String = body_bytes.iter().map(|&byte| char::from(byte)).collect();
            self.events
                .push(Event::Clipboard(text.replace("\r\n", "\n")));
        }
        self.pos += 8 + body_length;
        true
    }

    fn step_skip(&mut self, remaining: u64) -> bool {
        let available = u64::try_from(tail(&self.input, self.pos).len()).unwrap_or(u64::MAX);
        let take = available.min(remaining);
        if take == 0 {
            return false;
        }
        self.pos += usize::try_from(take).unwrap_or(0);
        let left = remaining - take;
        self.state = if left == 0 {
            State::Message
        } else {
            State::Skip(left)
        };
        true
    }

    fn begin_update(&mut self, count: u32) {
        self.unbounded = count == UNBOUNDED_RECT_COUNT;
        self.rects_left = if self.unbounded { 0 } else { count };
        self.rect_count = 0;
        self.update = UpdateInfo::default();
        if count == 0 {
            self.finish_update();
        } else {
            self.state = State::Rects;
        }
    }

    fn finish_update(&mut self) {
        self.state = State::Message;
        let update = std::mem::take(&mut self.update);
        self.events.push(Event::Update {
            codec: update.codec,
            dirty: update.dirty,
        });
        if !self.continuous {
            msg::update_request(&mut self.out, true, self.fb.width, self.fb.height);
        }
    }

    /// Called after a rectangle was fully consumed.
    fn rect_done(&mut self) -> Result<(), ProtocolError> {
        self.state = State::Rects;
        self.rect_count += 1;
        if self.unbounded {
            if self.rect_count > MAX_UNBOUNDED_RECTS {
                return Err(protocol_error(
                    "The screen server sent too many rectangles in one update.",
                ));
            }
        } else {
            self.rects_left = self.rects_left.saturating_sub(1);
            if self.rects_left == 0 {
                self.finish_update();
            }
        }
        Ok(())
    }

    fn step_rect(&mut self) -> Result<bool, ProtocolError> {
        let data = tail(&self.input, self.pos);
        if data.len() < 12 {
            return Ok(false);
        }
        let rect = Rect {
            x: usize::from(u16_at(data, 0)),
            y: usize::from(u16_at(data, 2)),
            w: usize::from(u16_at(data, 4)),
            h: usize::from(u16_at(data, 6)),
        };
        match i32_at(data, 8) {
            ENC_RAW => {
                self.check_rect(rect)?;
                self.pos += 12;
                self.update.codec = Some(Codec::Raw);
                self.update.dirty = true;
                if rect.w == 0 || rect.h == 0 {
                    self.rect_done()?;
                } else {
                    self.state = State::Raw { rect, row: 0 };
                }
                Ok(true)
            }
            ENC_COPY_RECT => self.rect_copy(rect),
            ENC_ZRLE => self.rect_zrle(rect),
            ENC_H264 => self.rect_h264(),
            ENC_DESKTOP_SIZE => {
                self.pos += 12;
                self.resize(rect.w, rect.h)?;
                self.rect_done()?;
                Ok(true)
            }
            ENC_EXT_DESKTOP_SIZE => self.rect_ext_desktop_size(rect),
            ENC_CONTINUOUS_UPDATES => {
                self.pos += 12;
                self.continuous_signal();
                self.rect_done()?;
                Ok(true)
            }
            ENC_FENCE => {
                self.pos += 12;
                self.rect_done()?;
                Ok(true)
            }
            ENC_LAST_RECT => {
                self.pos += 12;
                self.unbounded = false;
                self.rects_left = 0;
                self.finish_update();
                Ok(true)
            }
            other => Err(protocol_error(format!(
                "The screen server used an encoding this viewer does not support ({other})."
            ))),
        }
    }

    fn check_rect(&self, rect: Rect) -> Result<(), ProtocolError> {
        if rect.x + rect.w > self.fb.width || rect.y + rect.h > self.fb.height {
            return Err(protocol_error(
                "The screen server drew outside of the screen.",
            ));
        }
        Ok(())
    }

    fn step_raw(&mut self, rect: Rect, row: usize) -> Result<bool, ProtocolError> {
        let row_bytes = rect.w * 4;
        let data = tail(&self.input, self.pos);
        let rows = (data.len() / row_bytes).min(rect.h - row);
        if rows == 0 {
            return Ok(false);
        }
        for index in 0..rows {
            let source = &data[index * row_bytes..(index + 1) * row_bytes];
            let start = ((rect.y + row + index) * self.fb.width + rect.x) * 4;
            let Some(target) = self.fb.data.get_mut(start..start + row_bytes) else {
                return Err(protocol_error(
                    "The screen server drew outside of the screen.",
                ));
            };
            for (to, from) in target.chunks_exact_mut(4).zip(source.chunks_exact(4)) {
                to[0] = from[0];
                to[1] = from[1];
                to[2] = from[2];
                to[3] = 255;
            }
        }
        self.pos += rows * row_bytes;
        let done = row + rows;
        if done == rect.h {
            self.rect_done()?;
        } else {
            self.state = State::Raw { rect, row: done };
        }
        Ok(true)
    }

    fn rect_copy(&mut self, rect: Rect) -> Result<bool, ProtocolError> {
        let data = tail(&self.input, self.pos);
        if data.len() < 16 {
            return Ok(false);
        }
        let source_x = usize::from(u16_at(data, 12));
        let source_y = usize::from(u16_at(data, 14));
        self.check_rect(rect)?;
        self.check_rect(Rect {
            x: source_x,
            y: source_y,
            ..rect
        })?;
        let row_bytes = rect.w * 4;
        for index in 0..rect.h {
            let row = if rect.y <= source_y {
                index
            } else {
                rect.h - 1 - index
            };
            let from = ((source_y + row) * self.fb.width + source_x) * 4;
            let to = ((rect.y + row) * self.fb.width + rect.x) * 4;
            self.fb.data.copy_within(from..from + row_bytes, to);
        }
        self.pos += 16;
        self.update.codec = Some(Codec::Raw);
        self.update.dirty = true;
        self.rect_done()?;
        Ok(true)
    }

    fn rect_zrle(&mut self, rect: Rect) -> Result<bool, ProtocolError> {
        let data = tail(&self.input, self.pos);
        if data.len() < 16 {
            return Ok(false);
        }
        let length = to_usize(u32_at(data, 12));
        if length > MAX_ZRLE {
            return Err(protocol_error(
                "The screen server sent an oversized ZRLE rectangle.",
            ));
        }
        if data.len() < 16 + length {
            return Ok(false);
        }
        self.check_rect(rect)?;
        self.zrle
            .decode(&data[16..16 + length], rect, &mut self.fb)?;
        self.pos += 16 + length;
        self.update.codec = Some(Codec::Zrle);
        self.update.dirty = true;
        self.rect_done()?;
        Ok(true)
    }

    fn rect_h264(&mut self) -> Result<bool, ProtocolError> {
        let data = tail(&self.input, self.pos);
        if data.len() < 20 {
            return Ok(false);
        }
        let length = to_usize(u32_at(data, 12));
        let flags = u32_at(data, 16);
        if length > MAX_H264 {
            return Err(protocol_error(
                "The screen server sent an oversized H.264 rectangle.",
            ));
        }
        if data.len() < 20 + length {
            return Ok(false);
        }
        let Some(decoder) = self.h264.as_mut() else {
            return Err(protocol_error(
                "The screen server sent H.264 video although it was not requested.",
            ));
        };
        let mut failed = false;
        if length > 0
            && let Err(error) = decoder.decode(&data[20..20 + length], flags & 3 != 0)
        {
            log::warn!("H.264 decoding failed: {error}");
            failed = true;
        }
        if failed {
            // A full request makes the server restart the stream with a key frame.
            msg::update_request(&mut self.out, false, self.fb.width, self.fb.height);
        }
        self.pos += 20 + length;
        self.update.codec = Some(Codec::H264);
        self.rect_done()?;
        Ok(true)
    }

    fn rect_ext_desktop_size(&mut self, rect: Rect) -> Result<bool, ProtocolError> {
        let data = tail(&self.input, self.pos);
        if data.len() < 16 {
            return Ok(false);
        }
        let total = 16 + usize::from(data[12]) * 16;
        if data.len() < total {
            return Ok(false);
        }
        self.pos += total;
        // x is the reason, y the result code; a failed client-initiated request changes nothing.
        let failed_request = rect.x == 1 && rect.y != 0;
        if !failed_request {
            self.resize(rect.w, rect.h)?;
        }
        self.rect_done()?;
        Ok(true)
    }

    fn resize(&mut self, width: usize, height: usize) -> Result<(), ProtocolError> {
        validate_size(width, height)?;
        if width == self.fb.width && height == self.fb.height {
            return Ok(());
        }
        self.fb = Framebuffer::new(width, height);
        self.update.dirty = true;
        self.events.push(Event::Resized { width, height });
        msg::update_request(&mut self.out, false, width, height);
        if self.continuous {
            msg::enable_continuous_updates(&mut self.out, width, height);
        }
        Ok(())
    }

    /// The server announced continuous updates: switch them on for the whole screen.
    fn continuous_signal(&mut self) {
        if !self.continuous {
            self.continuous = true;
            msg::enable_continuous_updates(&mut self.out, self.fb.width, self.fb.height);
        }
    }
}

/// Reacts to an extended clipboard message body.
fn extended_clipboard(body: &[u8], out: &mut Vec<u8>, events: &mut Vec<Event>) {
    match clipboard::parse_incoming(body) {
        clipboard::Incoming::Caps(flags) => {
            clipboard::caps(out);
            events.push(Event::ExtClipboard(clipboard::server_accepts_text_flow(
                flags,
            )));
        }
        clipboard::Incoming::Request => events.push(Event::ClipboardRequest),
        clipboard::Incoming::Peek => events.push(Event::ClipboardPeek),
        clipboard::Incoming::Notify { text: true } => clipboard::request(out),
        clipboard::Incoming::Provide(text) if !text.is_empty() => {
            events.push(Event::Clipboard(text));
        }
        clipboard::Incoming::Notify { text: false }
        | clipboard::Incoming::Provide(_)
        | clipboard::Incoming::Ignored => {}
    }
}

fn tail(input: &[u8], pos: usize) -> &[u8] {
    input.get(pos..).unwrap_or(&[])
}

fn u16_at(data: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([data[at], data[at + 1]])
}

fn u32_at(data: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
}

fn i32_at(data: &[u8], at: usize) -> i32 {
    i32::from_be_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
}

fn to_usize(value: u32) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

/// `RFB xxx.yyy\n` to `(major, minor)`.
fn parse_version(greeting: &[u8]) -> Option<(u32, u32)> {
    if !greeting.starts_with(b"RFB ") || greeting[7] != b'.' || greeting[11] != b'\n' {
        return None;
    }
    let major = std::str::from_utf8(&greeting[4..7]).ok()?.parse().ok()?;
    let minor = std::str::from_utf8(&greeting[8..11]).ok()?.parse().ok()?;
    Some((major, minor))
}

/// A `u32` length followed by text at `at`; `None` while incomplete (an absurd length
/// yields empty text so that the caller fails instead of waiting for it).
fn read_reason(data: &[u8], at: usize) -> Option<String> {
    if data.len() < at + 4 {
        return None;
    }
    let length = to_usize(u32_at(data, at));
    if length > MAX_REASON {
        return Some(String::new());
    }
    let text = data.get(at + 4..at + 4 + length)?;
    Some(String::from_utf8_lossy(text).into_owned())
}

fn refused(reason: &str) -> ProtocolError {
    if reason.is_empty() {
        protocol_error("The screen server refused the connection.")
    } else {
        protocol_error(format!(
            "The screen server refused the connection: {reason}"
        ))
    }
}

fn validate_size(width: usize, height: usize) -> Result<(), ProtocolError> {
    if width == 0 || height == 0 {
        return Err(protocol_error("The remote screen has no size."));
    }
    if width > MAX_DIMENSION || height > MAX_DIMENSION || width * height > MAX_PIXELS {
        return Err(protocol_error(format!(
            "The remote screen ({width}x{height}) is larger than this viewer supports."
        )));
    }
    Ok(())
}
