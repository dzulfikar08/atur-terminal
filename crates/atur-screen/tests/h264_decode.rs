//! Decodes a real H.264 stream (see `fixtures/solid-cbp.txt`) through the platform decoder.
//!
//! Skipped (with a message) when the machine has no decoder, e.g. Linux without `ffmpeg`.
#![cfg_attr(
    target_os = "macos",
    allow(
        unsafe_code,
        reason = "the test reads a CVPixelBuffer through its raw plane addresses"
    )
)]

use std::sync::mpsc;
use std::time::{Duration, Instant};

use atur_screen::decode::{FrameSink, new_h264};
use atur_screen::{Frame, Pixels};

const FIXTURE: &[u8] = include_bytes!("fixtures/solid-cbp.h264");
const FRAME_COUNT: usize = 20;
const WIDTH: u32 = 320;
const HEIGHT: u32 = 180;

/// Centre colour of frames 0-4, 5-9, 10-14 and 15-19, measured from ffmpeg's own decode.
const EXPECTED: [[u8; 3]; 4] = [[253, 0, 0], [0, 254, 0], [0, 0, 254], [255, 255, 255]];

/// What the sink observed about one picture.
struct Seen {
    width: u32,
    height: u32,
    /// R, G, B of the pixel in the middle of the picture.
    centre: [u8; 3],
    /// Delivered as a `CoreVideo` surface (macOS) rather than BGRA bytes.
    surface: bool,
    /// Only meaningful for surfaces: the pixel format is `420YpCbCr8BiPlanarFullRange`.
    full_range: bool,
}

/// Splits Annex B data into NAL units without their start codes.
fn split_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut zeros = 0;
    for (index, &byte) in data.iter().enumerate() {
        if byte == 1 && zeros >= 2 {
            starts.push(index + 1);
            zeros = 0;
        } else if byte == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
    }
    let mut units = Vec::new();
    for (position, &start) in starts.iter().enumerate() {
        let end = starts.get(position + 1).map_or(data.len(), |next| next - 3);
        let mut unit = &data[start..end];
        while let [rest @ .., 0] = unit {
            unit = rest;
        }
        units.push(unit);
    }
    units
}

/// Groups NAL units into access units, the way neatvnc sends one per RFB rectangle:
/// a new unit starts at an SEI/SPS/PPS/AUD that follows picture data, or at a slice with
/// `first_mb_in_slice == 0` that follows picture data.
fn access_units(data: &[u8]) -> Vec<Vec<u8>> {
    let mut units: Vec<Vec<u8>> = Vec::new();
    let mut has_picture = false;
    for nal in split_nals(data) {
        let kind = nal[0] & 0x1f;
        let is_slice = kind == 1 || kind == 5;
        let starts_unit = has_picture
            && ((6..=9).contains(&kind)
                || (is_slice && nal.get(1).is_some_and(|byte| byte & 0x80 != 0)));
        if starts_unit || units.is_empty() {
            units.push(Vec::new());
            has_picture = false;
        }
        if let Some(unit) = units.last_mut() {
            unit.extend_from_slice(&[0, 0, 0, 1]);
            unit.extend_from_slice(nal);
        }
        has_picture |= is_slice;
    }
    units
}

fn centre_of(frame: &Frame) -> Option<Seen> {
    match &frame.pixels {
        Pixels::Bgra(bytes) => {
            let width = usize::try_from(frame.width).ok()?;
            let height = usize::try_from(frame.height).ok()?;
            let offset = ((height / 2) * width + width / 2) * 4;
            let pixel = bytes.get(offset..offset + 4)?;
            Some(Seen {
                width: frame.width,
                height: frame.height,
                centre: [pixel[2], pixel[1], pixel[0]],
                surface: false,
                full_range: false,
            })
        }
        #[cfg(target_os = "macos")]
        Pixels::Surface(surface) => surface_centre(frame, surface.buffer()),
    }
}

#[cfg(target_os = "macos")]
fn surface_centre(frame: &Frame, buffer: &core_video::pixel_buffer::CVPixelBuffer) -> Option<Seen> {
    use core_video::pixel_buffer::{
        kCVPixelBufferLock_ReadOnly, kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
    };

    let full_range = buffer.get_pixel_format() == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange;
    if !full_range || buffer.get_plane_count() != 2 {
        return Some(Seen {
            width: frame.width,
            height: frame.height,
            centre: [0; 3],
            surface: true,
            full_range,
        });
    }
    if buffer.lock_base_address(kCVPixelBufferLock_ReadOnly) != 0 {
        return None;
    }
    let row = buffer.get_height() / 2;
    let column = buffer.get_width() / 2;
    // SAFETY: the buffer is locked, so both planes are mapped; the offsets stay inside
    // the picture (row and column are in range, chroma is subsampled by two).
    let (y, cb, cr) = unsafe {
        let luma = buffer
            .get_base_address_of_plane(0)
            .cast::<u8>()
            .cast_const();
        let chroma = buffer
            .get_base_address_of_plane(1)
            .cast::<u8>()
            .cast_const();
        let y = *luma.add(row * buffer.get_bytes_per_row_of_plane(0) + column);
        let chroma_row = chroma.add((row / 2) * buffer.get_bytes_per_row_of_plane(1));
        (
            y,
            *chroma_row.add((column / 2) * 2),
            *chroma_row.add((column / 2) * 2 + 1),
        )
    };
    buffer.unlock_base_address(kCVPixelBufferLock_ReadOnly);

    // Full-range BT.601.
    let (y, cb, cr) = (i32::from(y), i32::from(cb) - 128, i32::from(cr) - 128);
    let channel = |value: i32| u8::try_from(value.clamp(0, 255)).unwrap_or(u8::MAX);
    Some(Seen {
        width: frame.width,
        height: frame.height,
        centre: [
            channel((256 * y + 359 * cr + 128) >> 8),
            channel((256 * y - 88 * cb - 183 * cr + 128) >> 8),
            channel((256 * y + 454 * cb + 128) >> 8),
        ],
        surface: true,
        full_range,
    })
}

fn channel_distance(a: [u8; 3], b: [u8; 3]) -> u8 {
    a.iter()
        .zip(b)
        .map(|(&left, right)| left.abs_diff(right))
        .max()
        .unwrap_or(0)
}

/// A sink that records what each picture looked like and nothing else (a macOS `Frame`
/// need not be `Send`, so pictures are summarised on the decoding thread).
fn recording_sink() -> (FrameSink, mpsc::Receiver<Seen>) {
    let (sender, receiver) = mpsc::channel();
    let sink: FrameSink = std::sync::Arc::new(move |frame: Frame| {
        if let Some(seen) = centre_of(&frame) {
            let _ = sender.send(seen);
        }
    });
    (sink, receiver)
}

/// Feeds the whole stream (resetting at the first unit) and collects the pictures.
fn decode_stream(attempts: usize) -> Option<(String, Vec<Seen>)> {
    let units = access_units(FIXTURE);
    assert_eq!(
        units.len(),
        FRAME_COUNT,
        "the splitter must find one unit per picture"
    );

    for attempt in 0..attempts {
        let (sink, receiver) = recording_sink();
        let mut decoder = new_h264(sink)?;
        let mut errors = Vec::new();
        for (index, unit) in units.iter().enumerate() {
            if let Err(error) = decoder.decode(unit, index == 0) {
                errors.push(format!("unit {index}: {error}"));
            }
        }

        let mut seen = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        while seen.len() < FRAME_COUNT {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            let Ok(picture) = receiver.recv_timeout(remaining) else {
                break;
            };
            seen.push(picture);
        }
        if seen.len() == FRAME_COUNT || attempt + 1 == attempts {
            assert!(
                seen.len() == FRAME_COUNT,
                "decoder {} produced {} of {FRAME_COUNT} pictures; errors: {errors:?}",
                decoder.name(),
                seen.len()
            );
            return Some((decoder.name(), seen));
        }
        // A hardware path that failed makes the decoder fall back; the session recovers
        // the same way, by resetting and sending a key frame again.
    }
    None
}

#[test]
fn decodes_every_picture_in_order_with_the_right_colours() {
    let Some((name, pictures)) = decode_stream(3) else {
        eprintln!("skipped: no H.264 decoder on this machine (install ffmpeg)");
        return;
    };
    eprintln!("decoder under test: {name}");

    for (index, picture) in pictures.iter().enumerate() {
        assert_eq!(
            (picture.width, picture.height),
            (WIDTH, HEIGHT),
            "picture {index}"
        );
        if picture.surface {
            assert!(
                picture.full_range,
                "picture {index}: surfaces must be full-range 4:2:0"
            );
        }
        // BGRA comes straight from ffmpeg. VideoToolbox converts the picture to full range on
        // its own terms, so surfaces get a wider tolerance.
        let tolerance = if picture.surface { 48 } else { 12 };
        let expected = EXPECTED[index / 5];
        let distance = channel_distance(picture.centre, expected);
        assert!(
            distance <= tolerance,
            "picture {index}: centre {:?} differs from {expected:?} by {distance}",
            picture.centre
        );
    }
}

#[test]
fn a_delta_frame_without_a_key_frame_is_an_error() {
    let (sink, _receiver) = recording_sink();
    let Some(mut decoder) = new_h264(sink) else {
        eprintln!("skipped: no H.264 decoder on this machine (install ffmpeg)");
        return;
    };
    let units = access_units(FIXTURE);
    assert!(decoder.decode(&units[1], true).is_err());
    assert!(decoder.decode(&[1, 2, 3], false).is_err());
}
