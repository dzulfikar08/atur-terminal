//! Where the remote picture is drawn in the view, and which remote pixel a
//! position in the view is over.

/// How the picture fills the view.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum Fit {
    /// Scaled to the view, keeping its proportions.
    #[default]
    Window,
    /// One picture pixel for each screen pixel. A picture larger than the view
    /// pans with the pointer.
    Actual,
}

impl Fit {
    pub(super) fn toggled(self) -> Self {
        match self {
            Self::Window => Self::Actual,
            Self::Actual => Self::Window,
        }
    }
}

/// The picture's rectangle in the view, in logical pixels from its top left.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Rect {
    pub(super) x: f32,
    pub(super) y: f32,
    pub(super) width: f32,
    pub(super) height: f32,
}

/// Logical pixels a smooth scroll moves for one wheel notch.
pub(super) const PIXELS_PER_NOTCH: f32 = 40.0;

/// Lines GPUI reports for one notch of a mouse wheel: three on Linux, one on macOS.
pub(super) const LINES_PER_NOTCH: f32 = if cfg!(target_os = "macos") { 1.0 } else { 3.0 };

/// The most wheel notches kept for later events.
const MAX_PENDING_NOTCHES: f32 = 100.0;

#[allow(
    clippy::cast_precision_loss,
    reason = "Screen sizes are far below the range where f32 loses whole numbers"
)]
fn float(value: u32) -> f32 {
    value as f32
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "The value is clamped to the picture's size first"
)]
fn whole(value: f32) -> i32 {
    value as i32
}

/// Where the picture of `picture` pixels sits in a view of `view` logical
/// pixels on a display scaled by `scale`. `pan` says where the pointer is in the
/// view, from 0 to 1 on each axis, which is how an `Actual` picture larger than
/// the view slides. `None` when there is nothing to draw.
pub(super) fn placement(
    fit: Fit,
    view: (f32, f32),
    picture: (u32, u32),
    scale: f32,
    pan: (f32, f32),
) -> Option<Rect> {
    let (view_width, view_height) = view;
    if picture.0 == 0 || picture.1 == 0 || view_width <= 0.0 || view_height <= 0.0 {
        return None;
    }
    let (picture_width, picture_height) = (float(picture.0), float(picture.1));
    match fit {
        Fit::Window => {
            let factor = (view_width / picture_width).min(view_height / picture_height);
            let (width, height) = (picture_width * factor, picture_height * factor);
            Some(Rect {
                x: (view_width - width) / 2.0,
                y: (view_height - height) / 2.0,
                width,
                height,
            })
        }
        Fit::Actual => {
            let scale = if scale > 0.0 { scale } else { 1.0 };
            let (width, height) = (picture_width / scale, picture_height / scale);
            Some(Rect {
                x: axis_offset(view_width, width, pan.0),
                y: axis_offset(view_height, height, pan.1),
                width,
                height,
            })
        }
    }
}

/// Where the picture starts along one axis: centered when it fits, else
/// slid by how far along the view the pointer is.
fn axis_offset(view: f32, picture: f32, pan: f32) -> f32 {
    if picture <= view {
        (view - picture) / 2.0
    } else {
        -(picture - view) * pan.clamp(0.0, 1.0)
    }
}

/// How far along each axis of the view `point` is, from 0 to 1.
pub(super) fn pan(view: (f32, f32), point: (f32, f32)) -> (f32, f32) {
    let along = |position: f32, size: f32| {
        if size > 0.0 {
            (position / size).clamp(0.0, 1.0)
        } else {
            0.5
        }
    };
    (along(point.0, view.0), along(point.1, view.1))
}

/// The picture pixel under `point`, a position in the view, kept inside the picture.
pub(super) fn picture_point(rect: Rect, picture: (u32, u32), point: (f32, f32)) -> (i32, i32) {
    let along = |position: f32, start: f32, length: f32, pixels: u32| {
        let value = ((position - start) / length * float(pixels)).floor();
        if value.is_nan() {
            0
        } else {
            whole(value.clamp(0.0, float(pixels.saturating_sub(1))))
        }
    };
    (
        along(point.0, rect.x, rect.width, picture.0),
        along(point.1, rect.y, rect.height, picture.1),
    )
}

/// Wheel notches in a scroll event whose `delta` counts `unit` for one notch
/// ([`LINES_PER_NOTCH`] or [`PIXELS_PER_NOTCH`]). What is left of a notch waits
/// in `remainder` for the next event. Positive is up and to the left, as GPUI
/// reports it.
pub(super) fn notches(delta: (f32, f32), unit: f32, remainder: &mut (f32, f32)) -> (i32, i32) {
    (
        take(&mut remainder.0, delta.0 / unit),
        take(&mut remainder.1, delta.1 / unit),
    )
}

fn take(remainder: &mut f32, added: f32) -> i32 {
    if added.abs() > f32::EPSILON && added.signum() != remainder.signum() {
        *remainder = 0.0;
    }
    *remainder = (*remainder + added).clamp(-MAX_PENDING_NOTCHES, MAX_PENDING_NOTCHES);
    let notches = remainder.trunc();
    *remainder -= notches;
    whole(notches)
}

#[cfg(test)]
mod tests;
