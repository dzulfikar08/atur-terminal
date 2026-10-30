use std::fmt;
use std::sync::Arc;

/// One complete picture of the remote screen, cheap to clone.
#[derive(Clone, Debug)]
pub struct Frame {
    /// Increases with every frame a session publishes.
    pub id: u64,
    pub width: u32,
    pub height: u32,
    pub pixels: Pixels,
}

/// The pixels of a [`Frame`].
#[derive(Clone)]
pub enum Pixels {
    /// Packed B, G, R, A bytes, A always 255; row stride is `width * 4`.
    Bgra(Arc<[u8]>),
    /// A CoreVideo buffer in `kCVPixelFormatType_420YpCbCr8BiPlanarFullRange`
    /// (the only format GPUI's surface renderer accepts) straight from VideoToolbox.
    #[cfg(target_os = "macos")]
    Surface(core_video::pixel_buffer::CVPixelBuffer),
}

impl fmt::Debug for Pixels {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bgra(bytes) => write!(formatter, "Bgra({} bytes)", bytes.len()),
            #[cfg(target_os = "macos")]
            Self::Surface(_) => formatter.write_str("Surface"),
        }
    }
}
