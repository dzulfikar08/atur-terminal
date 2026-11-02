//! RFB 3.8 as a server: what a viewer sends, and the pictures and replies it
//! gets back. Sans-IO; [`crate::serve`] moves the bytes.
//!
//! The subset is what the Atur Terminal viewer and the phone app speak: security
//! type None (the connection reached this program through the server's private
//! socket), raw and ZRLE pixels, the open H.264 encoding, `DesktopSize`,
//! continuous updates and the extended clipboard.

pub mod clipboard;
pub mod encode;
pub mod message;

pub use message::{ClientMessage, ClientParser, PixelFormat};

pub const ENC_RAW: i32 = 0;
pub const ENC_ZRLE: i32 = 16;
pub const ENC_H264: i32 = 50;
pub const ENC_DESKTOP_SIZE: i32 = -223;
pub const ENC_EXT_DESKTOP_SIZE: i32 = -308;
pub const ENC_CONTINUOUS_UPDATES: i32 = -313;
/// 0xC0A1E5CE as the signed value that goes on the wire.
pub const ENC_EXT_CLIPBOARD: i32 = -1_063_131_698;

/// Largest clipboard text, in bytes, that is shared in either direction.
pub const MAX_CLIPBOARD: usize = 1 << 20;

/// A rectangle of the picture, in pixels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}
