//! H.264 decoding for RFB encoding 50.
//!
//! macOS decodes in `VideoToolbox`. Elsewhere an `ffmpeg` process on the PATH
//! decodes, on the GPU through VA-API when the machine has it and in software
//! otherwise. When neither exists [`new_h264`] returns `None` and the session
//! never offers H.264, so the server sends ZRLE instead.

use std::sync::Arc;

use crate::Frame;

#[cfg(all(unix, not(target_os = "macos")))]
mod ffmpeg;
mod nal;
#[cfg(target_os = "macos")]
mod videotoolbox;
mod yuv;

/// Receives every decoded picture, in order. A decoder may call it from its own thread.
pub type FrameSink = Arc<dyn Fn(Frame) + Send + Sync>;

/// Why an access unit could not be decoded; the session then asks the server
/// for a key frame by resetting its H.264 context.
#[derive(Debug)]
pub struct DecodeError(pub String);

impl std::fmt::Display for DecodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for DecodeError {}

/// Decodes the Annex B byte stream of one RFB connection, access unit by access unit.
pub trait H264Decoder: Send {
    /// What decodes: `"videotoolbox"`, `"ffmpeg (vaapi)"`, ...
    fn name(&self) -> String;

    /// Feeds one access unit (the payload of one RFB H.264 rectangle, SPS and
    /// PPS included on key frames). `reset` means the server started a new
    /// context, so reference state must be dropped first. Decoded pictures go
    /// to the sink given at construction, possibly after this returns.
    fn decode(&mut self, annex_b: &[u8], reset: bool) -> Result<(), DecodeError>;
}

/// The best decoder this machine has, or `None` when it has no way to decode H.264.
pub fn new_h264(sink: FrameSink) -> Option<Box<dyn H264Decoder>> {
    platform_decoder(sink)
}

#[cfg(target_os = "macos")]
fn platform_decoder(sink: FrameSink) -> Option<Box<dyn H264Decoder>> {
    Some(Box::new(videotoolbox::VideoToolboxDecoder::new(sink)))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_decoder(sink: FrameSink) -> Option<Box<dyn H264Decoder>> {
    let decoder = ffmpeg::FfmpegDecoder::new(sink)?;
    Some(Box::new(decoder))
}

#[cfg(not(unix))]
fn platform_decoder(sink: FrameSink) -> Option<Box<dyn H264Decoder>> {
    let _ = sink;
    None
}
