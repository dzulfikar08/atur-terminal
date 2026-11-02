//! How the H.264 encoder is set up for a picture size, decided apart from the
//! encoder itself so that it can be tested anywhere.

/// The H.264 profile to ask for. Phones and desktops decode all of them; High
/// is the best compression, Main is the fallback when an encoder refuses it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Profile {
    High,
    Main,
}

/// Settings of one encoder session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    /// The frame rate the encoder is told to expect; pictures arrive only when
    /// the screen changes, so it is a ceiling.
    pub fps: u32,
    /// Average bits per second.
    pub bitrate: u32,
    /// Most pictures between key frames. A key frame is also made whenever a
    /// viewer asks for one.
    pub key_interval: u32,
    pub profile: Profile,
}

/// Hundredths of a bit per pixel and picture, at the rate basis below: enough
/// for text and windows to stay sharp, cheap for the rest.
const CENTIBITS_PER_PIXEL: u64 = 8;
const MIN_BITRATE: u32 = 1_500_000;
const MAX_BITRATE: u32 = 20_000_000;
/// Rate control sees at most this many pictures a second, however fast the
/// screen changes: more detail per picture beats more pictures for desktops.
const RATE_BASIS_FPS: u32 = 30;
const KEY_INTERVAL_SECONDS: u32 = 10;

impl EncoderConfig {
    /// The settings for pictures of `width` x `height` at up to `fps` a second.
    #[must_use]
    pub fn for_size(width: u32, height: u32, fps: u32) -> Self {
        let fps = fps.max(1);
        let wanted = u64::from(width)
            * u64::from(height)
            * u64::from(fps.min(RATE_BASIS_FPS))
            * CENTIBITS_PER_PIXEL
            / 100;
        let bitrate = u32::try_from(wanted)
            .unwrap_or(MAX_BITRATE)
            .clamp(MIN_BITRATE, MAX_BITRATE);
        Self {
            width,
            height,
            fps,
            bitrate,
            key_interval: fps * KEY_INTERVAL_SECONDS,
            profile: Profile::High,
        }
    }

    /// The same settings with the profile every encoder accepts.
    #[must_use]
    pub fn fallback(self) -> Self {
        Self {
            profile: Profile::Main,
            ..self
        }
    }
}

/// The picture size to capture a display of `pixels` (width, height) at, with
/// its longest side at most `max_edge`: scaled down in proportion, and both
/// sides even because H.264 codes 2x2 blocks of colour.
#[must_use]
pub fn capture_size(pixels: (u32, u32), max_edge: u32) -> (u32, u32) {
    let (width, height) = (pixels.0.max(2), pixels.1.max(2));
    let longest = width.max(height);
    let (width, height) = if longest > max_edge {
        let scale = |side: u32| {
            u32::try_from(u64::from(side) * u64::from(max_edge) / u64::from(longest))
                .unwrap_or(side)
        };
        (scale(width), scale(height))
    } else {
        (width, height)
    };
    (((width + 1) & !1).max(2), ((height + 1) & !1).max(2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bitrate_follows_the_picture_within_bounds() {
        let hd = EncoderConfig::for_size(1920, 1080, 60);
        assert_eq!(hd.bitrate, 4_976_640);
        assert_eq!(hd.key_interval, 600);
        assert_eq!(hd.profile, Profile::High);
        let tiny = EncoderConfig::for_size(320, 240, 60);
        assert_eq!(tiny.bitrate, MIN_BITRATE);
        let huge = EncoderConfig::for_size(7680, 4320, 60);
        assert_eq!(huge.bitrate, MAX_BITRATE);
        // A slow ceiling lowers it; a fast one does not raise it past the basis.
        let slow = EncoderConfig::for_size(1920, 1080, 10);
        assert!(slow.bitrate < hd.bitrate);
        assert_eq!(EncoderConfig::for_size(1920, 1080, 120).bitrate, hd.bitrate);
        assert_eq!(hd.fallback().profile, Profile::Main);
    }

    #[test]
    fn capture_size_scales_down_to_even_sides() {
        // The 13-inch MacBook Air panel fits as it is.
        assert_eq!(capture_size((2560, 1664), 2560), (2560, 1664));
        // A 5K display is scaled in proportion.
        assert_eq!(capture_size((5120, 2880), 2560), (2560, 1440));
        // Portrait displays are limited on their long side.
        assert_eq!(capture_size((2880, 5120), 2560), (1440, 2560));
        // Odd results are rounded up to even.
        assert_eq!(capture_size((1001, 777), 4096), (1002, 778));
        assert_eq!(capture_size((0, 0), 2560), (2, 2));
    }
}
