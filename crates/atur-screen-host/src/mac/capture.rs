//! ScreenCaptureKit capture of one display and the pasteboard, through the
//! small Objective-C layer in `capture.m`.
#![allow(
    unsafe_code,
    reason = "the capture layer is a C interface to Objective-C"
)]

use std::ffi::{CStr, c_char, c_void};
use std::fmt;
use std::sync::Arc;

use super::pixel::{CvPixelBufferRef, PixelBuffer};
use crate::frame::{Frame, FrameData};
use crate::serve::Broadcast;

type FrameFn = extern "C" fn(ctx: *mut c_void, frame: CvPixelBufferRef);
type StopFn = extern "C" fn(ctx: *mut c_void, code: i32, message: *const c_char);

const CODE_PERMISSION: i32 = 1;
const ERROR_LENGTH: usize = 512;

unsafe extern "C" {
    fn atur_app_init();
    fn atur_app_run();
    fn atur_capture_start(
        display_id: u32,
        width: i32,
        height: i32,
        fps: i32,
        on_frame: FrameFn,
        on_stop: StopFn,
        ctx: *mut c_void,
        error: *mut c_char,
        error_length: usize,
        code: *mut i32,
    ) -> *mut c_void;
    fn atur_capture_stop(handle: *mut c_void);
    fn atur_pasteboard_change_count() -> std::ffi::c_long;
    fn atur_pasteboard_copy_text() -> *mut c_char;
    fn atur_pasteboard_set_text(utf8: *const c_char) -> std::ffi::c_long;
    fn atur_free(pointer: *mut c_void);
}

/// Connects this process to the window server, as ScreenCaptureKit needs.
/// Call it first, on the main thread.
pub fn init_application() {
    // SAFETY: called once on the main thread before anything else.
    unsafe { atur_app_init() };
}

/// Runs the main thread's run loop; never returns.
pub fn run_application() {
    // SAFETY: called on the main thread after `init_application`.
    unsafe { atur_app_run() };
}

/// Why a capture could not start.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StartError {
    /// Screen Recording is not allowed for this program.
    Permission(String),
    Failed(String),
}

/// What a capture reports when it stops by itself.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Stopped {
    /// Screen Recording was taken away.
    pub permission: bool,
    pub message: String,
}

struct Context {
    broadcast: Arc<Broadcast>,
    on_stop: Box<dyn Fn(Stopped) + Send + Sync>,
}

/// A running capture; stops when dropped.
pub struct Capture {
    handle: *mut c_void,
    // Kept alive for the callbacks, which hold a pointer to it.
    _context: Box<Context>,
}

// SAFETY: the handle is only used to stop the capture, which may happen on any thread.
unsafe impl Send for Capture {}

impl fmt::Debug for Capture {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Capture").finish_non_exhaustive()
    }
}

extern "C" fn frame_callback(ctx: *mut c_void, frame: CvPixelBufferRef) {
    // SAFETY: `ctx` is the context the capture was started with, alive until the
    // capture is stopped, and `frame` is valid for this call.
    let (context, buffer) = unsafe {
        (
            &*ctx.cast_const().cast::<Context>(),
            PixelBuffer::retain(frame),
        )
    };
    if let Some(buffer) = buffer {
        context.broadcast.publish(Arc::new(Frame {
            width: buffer.width(),
            height: buffer.height(),
            data: FrameData::Surface(buffer),
        }));
    }
}

extern "C" fn stop_callback(ctx: *mut c_void, code: i32, message: *const c_char) {
    // SAFETY: as in `frame_callback`; `message` is a C string valid for this call.
    let (context, message) = unsafe {
        (
            &*ctx.cast_const().cast::<Context>(),
            CStr::from_ptr(message).to_string_lossy().into_owned(),
        )
    };
    (context.on_stop)(Stopped {
        permission: code == CODE_PERMISSION,
        message,
    });
}

impl Capture {
    /// Starts capturing display `display` as pictures of `size` pixels, at most
    /// `fps` a second, into `broadcast`. `on_stop` runs when the capture ends
    /// on its own, for example because Screen Recording was turned off.
    ///
    /// # Errors
    /// Why macOS did not start it.
    pub fn start(
        display: u32,
        size: (u32, u32),
        fps: u32,
        broadcast: Arc<Broadcast>,
        on_stop: impl Fn(Stopped) + Send + Sync + 'static,
    ) -> Result<Self, StartError> {
        let mut context = Box::new(Context {
            broadcast,
            on_stop: Box::new(on_stop),
        });
        let mut error: [c_char; ERROR_LENGTH] = [0; ERROR_LENGTH];
        let ctx = (&raw mut *context).cast::<c_void>();
        let mut code = 0_i32;
        let to_int = |value: u32| i32::try_from(value).unwrap_or(i32::MAX);
        // SAFETY: the context outlives the capture (it is stored in the result and
        // only dropped after `atur_capture_stop`); the buffers are valid for the call.
        let handle = unsafe {
            atur_capture_start(
                display,
                to_int(size.0),
                to_int(size.1),
                to_int(fps),
                frame_callback,
                stop_callback,
                ctx,
                error.as_mut_ptr(),
                ERROR_LENGTH,
                &raw mut code,
            )
        };
        if handle.is_null() {
            // SAFETY: the layer wrote a NUL terminated string into the buffer.
            let message = unsafe { CStr::from_ptr(error.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            return Err(if code == CODE_PERMISSION {
                StartError::Permission(message)
            } else {
                StartError::Failed(message)
            });
        }
        Ok(Self {
            handle,
            _context: context,
        })
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // SAFETY: `handle` came from `atur_capture_start` and is stopped once.
        unsafe { atur_capture_stop(self.handle) };
    }
}

/// The text of the general pasteboard.
pub mod pasteboard {
    use super::{
        CStr, atur_free, atur_pasteboard_change_count, atur_pasteboard_copy_text,
        atur_pasteboard_set_text,
    };

    /// Changes whenever the clipboard does.
    #[must_use]
    pub fn change_count() -> u64 {
        // SAFETY: a plain query.
        u64::try_from(unsafe { atur_pasteboard_change_count() }).unwrap_or(0)
    }

    /// The clipboard's text, when it holds some.
    #[must_use]
    pub fn text() -> Option<String> {
        // SAFETY: the layer returns a malloc'ed C string or null; it is freed once.
        unsafe {
            let raw = atur_pasteboard_copy_text();
            if raw.is_null() {
                return None;
            }
            let text = CStr::from_ptr(raw).to_string_lossy().into_owned();
            atur_free(raw.cast());
            Some(text)
        }
    }

    /// Replaces the clipboard's content with `text`; returns [`change_count`] afterwards.
    #[must_use]
    pub fn set_text(text: &str) -> u64 {
        let Ok(c_text) = std::ffi::CString::new(text) else {
            return change_count();
        };
        // SAFETY: `c_text` is a valid C string for the call.
        u64::try_from(unsafe { atur_pasteboard_set_text(c_text.as_ptr()) }).unwrap_or(0)
    }
}
