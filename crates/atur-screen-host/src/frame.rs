//! One picture of the shared screen.

use std::fmt;

/// A picture of `width` x `height` pixels, in memory the capture owns.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub data: FrameData,
}

/// Where the pixels live.
pub enum FrameData {
    /// B, G, R, A bytes, `stride` bytes per row.
    Cpu { stride: usize, bytes: Vec<u8> },
    /// A `CoreVideo` pixel buffer (an `IOSurface`) as `ScreenCaptureKit` delivers it.
    #[cfg(target_os = "macos")]
    Surface(crate::mac::pixel::PixelBuffer),
}

impl fmt::Debug for FrameData {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cpu { stride, bytes } => formatter
                .debug_struct("Cpu")
                .field("stride", stride)
                .field("bytes", &bytes.len())
                .finish(),
            #[cfg(target_os = "macos")]
            Self::Surface(buffer) => formatter.debug_tuple("Surface").field(buffer).finish(),
        }
    }
}

impl fmt::Debug for Frame {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Frame")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

impl Frame {
    /// A picture in ordinary memory.
    #[must_use]
    pub fn cpu(width: u32, height: u32, stride: usize, bytes: Vec<u8>) -> Self {
        Self {
            width,
            height,
            data: FrameData::Cpu { stride, bytes },
        }
    }

    /// Runs `read` with the pixels as B, G, R, A bytes and the row length in
    /// bytes. `None` when the pixels cannot be read.
    pub fn with_bgra<R>(&self, read: impl FnOnce(usize, &[u8]) -> R) -> Option<R> {
        match &self.data {
            FrameData::Cpu { stride, bytes } => Some(read(*stride, bytes)),
            #[cfg(target_os = "macos")]
            FrameData::Surface(buffer) => buffer.with_bgra(read),
        }
    }
}
