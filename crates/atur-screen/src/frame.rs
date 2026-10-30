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
    Surface(Surface),
}

/// A CoreVideo pixel buffer that can cross threads.
///
/// CoreVideo buffers are reference counted with atomic operations and are
/// only read once decoded, but the binding does not declare them `Send`, so
/// a frame could not move from the decoder's thread to the window's.
#[cfg(target_os = "macos")]
#[derive(Clone)]
pub struct Surface(core_video::pixel_buffer::CVPixelBuffer);

#[cfg(target_os = "macos")]
impl Surface {
    pub fn new(buffer: core_video::pixel_buffer::CVPixelBuffer) -> Self {
        Self(buffer)
    }

    pub fn buffer(&self) -> &core_video::pixel_buffer::CVPixelBuffer {
        &self.0
    }
}

// SAFETY: a CVPixelBuffer is a CoreFoundation object, whose retain and release
// are atomic, and decoded buffers are never written again after the decoder
// hands them over. Apple documents CVPixelBuffer as usable from any thread.
#[cfg(target_os = "macos")]
#[allow(unsafe_code, reason = "see the safety note above")]
unsafe impl Send for Surface {}

// SAFETY: as above; only reads happen through a shared reference.
#[cfg(target_os = "macos")]
#[allow(unsafe_code, reason = "see the safety note above")]
unsafe impl Sync for Surface {}

impl fmt::Debug for Pixels {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bgra(bytes) => write!(formatter, "Bgra({} bytes)", bytes.len()),
            #[cfg(target_os = "macos")]
            Self::Surface(_) => formatter.write_str("Surface"),
        }
    }
}
