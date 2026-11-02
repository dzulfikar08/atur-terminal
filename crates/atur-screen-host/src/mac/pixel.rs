//! `CoreVideo` pixel buffers: the pictures `ScreenCaptureKit` delivers (backed by
//! an `IOSurface`, so the encoder reads them without a copy).
#![allow(
    unsafe_code,
    reason = "CoreVideo has no safe bindings in the dependency tree"
)]

use std::ffi::c_void;
use std::fmt;
use std::ptr;

/// `CVPixelBufferRef`.
pub type CvPixelBufferRef = *mut c_void;

/// `kCVPixelFormatType_32BGRA`: 'BGRA'.
const PIXEL_FORMAT_BGRA: u32 = 0x4247_5241;
/// `kCVPixelBufferLock_ReadOnly`.
const LOCK_READ_ONLY: u64 = 1;
const RETURN_SUCCESS: i32 = 0;

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVPixelBufferCreate(
        allocator: *const c_void,
        width: usize,
        height: usize,
        pixel_format_type: u32,
        pixel_buffer_attributes: *const c_void,
        pixel_buffer_out: *mut CvPixelBufferRef,
    ) -> i32;
    fn CVPixelBufferLockBaseAddress(buffer: CvPixelBufferRef, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(buffer: CvPixelBufferRef, flags: u64) -> i32;
    fn CVPixelBufferGetBaseAddress(buffer: CvPixelBufferRef) -> *mut c_void;
    fn CVPixelBufferGetBytesPerRow(buffer: CvPixelBufferRef) -> usize;
    fn CVPixelBufferGetWidth(buffer: CvPixelBufferRef) -> usize;
    fn CVPixelBufferGetHeight(buffer: CvPixelBufferRef) -> usize;
    fn CVPixelBufferGetPixelFormatType(buffer: CvPixelBufferRef) -> u32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRetain(object: *const c_void) -> *const c_void;
    fn CFRelease(object: *const c_void);
}

/// A reference to a `CVPixelBuffer`, released when dropped.
pub struct PixelBuffer {
    raw: CvPixelBufferRef,
}

// SAFETY: a CVPixelBuffer is reference counted and safe to use from any thread;
// its pixels are only read, under a read-only lock.
unsafe impl Send for PixelBuffer {}
// SAFETY: see above; concurrent readers each take their own read-only lock.
unsafe impl Sync for PixelBuffer {}

impl fmt::Debug for PixelBuffer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PixelBuffer")
            .field("width", &self.width())
            .field("height", &self.height())
            .finish()
    }
}

impl Drop for PixelBuffer {
    fn drop(&mut self) {
        // SAFETY: `raw` is a buffer this value holds one reference to.
        unsafe { CFRelease(self.raw.cast_const()) };
    }
}

impl PixelBuffer {
    /// Takes a new reference to `raw`, which the caller keeps its own to.
    ///
    /// # Safety
    /// `raw` must be a valid `CVPixelBufferRef`, or null.
    #[must_use]
    pub unsafe fn retain(raw: CvPixelBufferRef) -> Option<Self> {
        if raw.is_null() {
            return None;
        }
        // SAFETY: the caller guarantees `raw` is a valid object.
        let raw = unsafe { CFRetain(raw.cast_const()) }.cast_mut();
        Some(Self { raw })
    }

    /// The buffer for calls into `VideoToolbox`.
    #[must_use]
    pub fn as_ptr(&self) -> CvPixelBufferRef {
        self.raw
    }

    #[must_use]
    pub fn width(&self) -> u32 {
        // SAFETY: `raw` is valid for the life of `self`.
        u32::try_from(unsafe { CVPixelBufferGetWidth(self.raw) }).unwrap_or(0)
    }

    #[must_use]
    pub fn height(&self) -> u32 {
        // SAFETY: `raw` is valid for the life of `self`.
        u32::try_from(unsafe { CVPixelBufferGetHeight(self.raw) }).unwrap_or(0)
    }

    /// A buffer of `width` x `height` BGRA pixels holding a copy of `bytes`
    /// (`stride` bytes per row).
    #[must_use]
    pub fn from_bgra(width: u32, height: u32, stride: usize, bytes: &[u8]) -> Option<Self> {
        let (columns, rows) = (usize::try_from(width).ok()?, usize::try_from(height).ok()?);
        let needed = rows
            .checked_sub(1)?
            .checked_mul(stride)?
            .checked_add(columns * 4)?;
        if bytes.len() < needed {
            return None;
        }
        let mut raw: CvPixelBufferRef = ptr::null_mut();
        // SAFETY: the out pointer is valid; no attributes or allocator are given.
        let status = unsafe {
            CVPixelBufferCreate(
                ptr::null(),
                columns,
                rows,
                PIXEL_FORMAT_BGRA,
                ptr::null(),
                &raw mut raw,
            )
        };
        if status != RETURN_SUCCESS || raw.is_null() {
            return None;
        }
        // The create rule gave us the one reference this value releases.
        let buffer = Self { raw };
        let copied = buffer.with_bgra_mut(|target_stride, target| {
            for row in 0..rows {
                let from = &bytes[row * stride..row * stride + columns * 4];
                target[row * target_stride..row * target_stride + columns * 4]
                    .copy_from_slice(from);
            }
        });
        copied.map(|()| buffer)
    }

    /// Runs `read` with the pixels as B, G, R, A bytes and the row length in
    /// bytes. `None` when the buffer is not BGRA or cannot be locked.
    pub fn with_bgra<R>(&self, read: impl FnOnce(usize, &[u8]) -> R) -> Option<R> {
        // SAFETY: `raw` is valid for the life of `self`.
        if unsafe { CVPixelBufferGetPixelFormatType(self.raw) } != PIXEL_FORMAT_BGRA {
            return None;
        }
        // SAFETY: `raw` is valid; the lock is released below on every path.
        if unsafe { CVPixelBufferLockBaseAddress(self.raw, LOCK_READ_ONLY) } != RETURN_SUCCESS {
            return None;
        }
        // SAFETY: the buffer is locked, so the address and row length stay valid
        // until the unlock; the slice does not escape `read`.
        let result = unsafe {
            let base = CVPixelBufferGetBaseAddress(self.raw)
                .cast_const()
                .cast::<u8>();
            let stride = CVPixelBufferGetBytesPerRow(self.raw);
            let height = CVPixelBufferGetHeight(self.raw);
            if base.is_null() {
                None
            } else {
                Some(read(
                    stride,
                    std::slice::from_raw_parts(base, stride * height),
                ))
            }
        };
        // SAFETY: matches the lock above.
        unsafe { CVPixelBufferUnlockBaseAddress(self.raw, LOCK_READ_ONLY) };
        result
    }

    /// Like [`PixelBuffer::with_bgra`], for writing.
    fn with_bgra_mut<R>(&self, write: impl FnOnce(usize, &mut [u8]) -> R) -> Option<R> {
        // SAFETY: `raw` is valid and not shared yet; the lock is released below.
        if unsafe { CVPixelBufferLockBaseAddress(self.raw, 0) } != RETURN_SUCCESS {
            return None;
        }
        // SAFETY: the buffer is locked for writing and only this code holds it.
        let result = unsafe {
            let base = CVPixelBufferGetBaseAddress(self.raw).cast::<u8>();
            let stride = CVPixelBufferGetBytesPerRow(self.raw);
            let height = CVPixelBufferGetHeight(self.raw);
            if base.is_null() {
                None
            } else {
                Some(write(
                    stride,
                    std::slice::from_raw_parts_mut(base, stride * height),
                ))
            }
        };
        // SAFETY: matches the lock above.
        unsafe { CVPixelBufferUnlockBaseAddress(self.raw, 0) };
        result
    }
}
