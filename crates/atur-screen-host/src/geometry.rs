//! Where the shared picture sits on the Mac's desktop.

/// The shared display: its rectangle in points (what mouse events use) and the
/// size of the picture in pixels (what viewers see and point at).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geometry {
    pub origin_x: f64,
    pub origin_y: f64,
    pub points_width: f64,
    pub points_height: f64,
    pub pixels_width: u32,
    pub pixels_height: u32,
}

impl Geometry {
    /// The desktop point under picture pixel `(x, y)`, clamped to the picture.
    #[must_use]
    pub fn point(&self, x: u16, y: u16) -> (f64, f64) {
        let last_x = f64::from(self.pixels_width.saturating_sub(1));
        let last_y = f64::from(self.pixels_height.saturating_sub(1));
        let across = if last_x > 0.0 {
            (f64::from(x).min(last_x)) / last_x
        } else {
            0.0
        };
        let down = if last_y > 0.0 {
            (f64::from(y).min(last_y)) / last_y
        } else {
            0.0
        };
        // The last pixel maps just inside the display, not onto its edge.
        let inside = |size: f64| (size - 1.0).max(0.0);
        (
            self.origin_x + across * inside(self.points_width),
            self.origin_y + down * inside(self.points_height),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn retina() -> Geometry {
        Geometry {
            origin_x: 0.0,
            origin_y: 0.0,
            points_width: 1470.0,
            points_height: 956.0,
            pixels_width: 2560,
            pixels_height: 1664,
        }
    }

    #[test]
    fn corners_map_to_the_display_corners() {
        let geometry = retina();
        assert_eq!(geometry.point(0, 0), (0.0, 0.0));
        assert_eq!(geometry.point(2559, 1663), (1469.0, 955.0));
    }

    #[test]
    fn outside_the_picture_is_clamped() {
        let geometry = retina();
        assert_eq!(geometry.point(60_000, 60_000), (1469.0, 955.0));
    }

    #[test]
    fn a_display_away_from_the_origin_keeps_its_offset() {
        let geometry = Geometry {
            origin_x: -1920.0,
            origin_y: 100.0,
            points_width: 1920.0,
            points_height: 1080.0,
            pixels_width: 1920,
            pixels_height: 1080,
        };
        assert_eq!(geometry.point(0, 0), (-1920.0, 100.0));
        assert_eq!(geometry.point(960, 540).0, -1920.0 + 960.0);
    }
}
