//! The main display: where it is, and how many pixels it really has.
#![allow(
    unsafe_code,
    reason = "CoreGraphics has no safe bindings in the dependency tree"
)]

use std::ffi::c_void;

use crate::encoder_config::capture_size;
use crate::geometry::Geometry;

#[repr(C)]
#[derive(Clone, Copy)]
struct CgPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CgSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CgRect {
    origin: CgPoint,
    size: CgSize,
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGMainDisplayID() -> u32;
    fn CGDisplayBounds(display: u32) -> CgRect;
    fn CGDisplayCopyDisplayMode(display: u32) -> *const c_void;
    fn CGDisplayModeGetPixelWidth(mode: *const c_void) -> usize;
    fn CGDisplayModeGetPixelHeight(mode: *const c_void) -> usize;
    fn CGDisplayModeRelease(mode: *const c_void);
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

/// A display as the system describes it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Display {
    pub id: u32,
    pub origin: (f64, f64),
    /// Size in points, what mouse events use.
    pub points: (f64, f64),
    /// Size in pixels of the display's current mode.
    pub pixels: (u32, u32),
}

impl Display {
    /// The main display, or `None` when there is none (nobody is logged in on
    /// the Mac's screen, or it is asleep and detached).
    #[must_use]
    pub fn main() -> Option<Self> {
        // SAFETY: plain value queries; the mode is released after use.
        unsafe {
            let id = CGMainDisplayID();
            let bounds = CGDisplayBounds(id);
            if bounds.size.width < 1.0 || bounds.size.height < 1.0 {
                return None;
            }
            let mode = CGDisplayCopyDisplayMode(id);
            let mut pixels = (0, 0);
            if !mode.is_null() {
                pixels = (
                    u32::try_from(CGDisplayModeGetPixelWidth(mode)).unwrap_or(0),
                    u32::try_from(CGDisplayModeGetPixelHeight(mode)).unwrap_or(0),
                );
                CGDisplayModeRelease(mode);
            }
            if pixels.0 == 0 || pixels.1 == 0 {
                // Without a mode, a point is a pixel.
                #[allow(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "display sizes are small positive numbers"
                )]
                let rounded = (
                    bounds.size.width.round() as u32,
                    bounds.size.height.round() as u32,
                );
                pixels = rounded;
            }
            Some(Self {
                id,
                origin: (bounds.origin.x, bounds.origin.y),
                points: (bounds.size.width, bounds.size.height),
                pixels,
            })
        }
    }

    /// The size of the shared picture for this display: its pixels, scaled down
    /// to at most `max_edge` on the longest side.
    #[must_use]
    pub fn picture_size(&self, max_edge: u32) -> (u32, u32) {
        capture_size(self.pixels, max_edge)
    }

    /// Where pointer events land for a picture of `picture` pixels.
    #[must_use]
    pub fn geometry(&self, picture: (u32, u32)) -> Geometry {
        Geometry {
            origin_x: self.origin.0,
            origin_y: self.origin.1,
            points_width: self.points.0,
            points_height: self.points.1,
            pixels_width: picture.0,
            pixels_height: picture.1,
        }
    }
}

/// Whether Screen Recording is allowed for this program; never asks.
#[must_use]
pub fn screen_recording_allowed() -> bool {
    // SAFETY: a plain query.
    unsafe { CGPreflightScreenCaptureAccess() }
}

/// Asks macOS for Screen Recording: the system shows its prompt, once per
/// program, and adds the program to the list in System Settings. Returns
/// whether it is allowed right now.
#[must_use]
pub fn request_screen_recording() -> bool {
    // SAFETY: a plain call.
    unsafe { CGRequestScreenCaptureAccess() }
}
