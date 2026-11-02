//! Shares this Mac's screen over RFB, for the Atur Terminal server to relay.
//!
//! This is the program `atur-term-screen`, the macOS counterpart of `wayvnc`.
//! It captures the main display with `ScreenCaptureKit`, encodes it as H.264 on
//! the Mac's video hardware (`VideoToolbox`), serves it as RFB with the open
//! H.264 encoding (ZRLE and raw pixels for viewers that cannot decode video),
//! and carries the viewer's mouse, keyboard and clipboard back with `CGEvent` and
//! the pasteboard.
//!
//! The protocol and the server are portable and tested everywhere; everything
//! that touches macOS lives in [`mac`].
//!
//! It runs as a small application of its own (`Atur Terminal Screen.app`, inside
//! the Atur Terminal app) so that macOS asks for Screen Recording and
//! Accessibility permission for that name; see `atur-screen-source` for why.

pub mod encoder_config;
pub mod frame;
pub mod geometry;
pub mod h264;
pub mod keymap;
#[cfg(target_os = "macos")]
pub mod mac;
pub mod options;
pub mod rfb;
pub mod serve;
pub mod synthetic;
