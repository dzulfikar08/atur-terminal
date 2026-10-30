#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use atur_screen::decode::{DecodeError, FrameSink, H264Decoder};
use atur_screen::{Frame, Pixels, ScreenInfo, ScreenOptions, ScreenState};
use support::*;

const CHUNKINGS: [Chunking; 6] = [
    Chunking::Whole,
    Chunking::Each(1),
    Chunking::Each(7),
    Chunking::Random(1),
    Chunking::Random(42),
    Chunking::Random(0xdead_beef),
];

fn options() -> ScreenOptions {
    ScreenOptions::default()
}

fn concat(parts: &[Vec<u8>]) -> Vec<u8> {
    parts.concat()
}

#[test]
fn handshake_and_raw_update_with_any_chunking() {
    for chunking in CHUNKINGS {
        let harness = start(options());
        assert_eq!(harness.screen.state(), ScreenState::Connecting);
        connect(&harness, 4, 3, chunking, false);
        assert_eq!(
            harness.screen.state(),
            ScreenState::Ready(ScreenInfo {
                width: 4,
                height: 3,
                name: "fake-vm".to_owned()
            })
        );
        assert!(harness.screen.frame().is_none());

        let pixels: Vec<[u8; 4]> = (0..4u8)
            .map(|i| [i * 10, i * 10 + 1, i * 10 + 2, 99])
            .collect();
        harness
            .server
            .push(&update(&[raw_rect(1, 1, 2, 2, &pixels)]), chunking);
        let frame = wait_frame(&harness.screen, |_| true);
        assert_eq!((frame.width, frame.height), (4, 3));
        assert_eq!(pixel_at(&frame, 1, 1), [0, 1, 2, 255]);
        assert_eq!(pixel_at(&frame, 2, 1), [10, 11, 12, 255]);
        assert_eq!(pixel_at(&frame, 1, 2), [20, 21, 22, 255]);
        assert_eq!(pixel_at(&frame, 2, 2), [30, 31, 32, 255]);
        assert_eq!(pixel_at(&frame, 0, 0), [0, 0, 0, 255]);
        assert_eq!(harness.server.read(10), update_request(true, 4, 3));

        wait_for("every chunk to be processed", || {
            harness.server.processed.load(Ordering::SeqCst)
                == harness.server.pushed.load(Ordering::SeqCst)
        });
        assert_eq!(harness.screen.stats().codec, "raw");
        assert!(harness.wakes.load(Ordering::SeqCst) > 0);
    }
}

/// Expected BGRA framebuffer, opaque black.
fn blank(width: usize, height: usize) -> Vec<u8> {
    let mut data = vec![0u8; width * height * 4];
    for pixel in data.chunks_exact_mut(4) {
        pixel[3] = 255;
    }
    data
}

#[test]
fn zrle_tiles_of_every_kind_with_any_chunking() {
    let palette4 = [[1, 2, 3], [40, 50, 60], [70, 80, 90], [100, 110, 120]];
    let palette5 = [[9, 9, 9], [8, 8, 8], [7, 7, 7], [6, 6, 6], [5, 5, 5]];
    let palette2 = [[200, 100, 50], [3, 2, 1]];
    for chunking in CHUNKINGS {
        let harness = start(options());
        connect(&harness, 130, 70, chunking, false);
        let mut stream = ZrleStream::new();

        // 130 x 70: tiles of 64, 64, 2 columns and 64, 6 rows.
        let layout = [
            (0, 0, 64, solid(64, 64, [10, 20, 30])),
            (
                64,
                0,
                64,
                packed(64, 64, &palette4, |x, y| (x / 16 + y / 16) % 4),
            ),
            (
                128,
                0,
                2,
                plain_rle(&[([1, 2, 3], 40), ([4, 5, 6], 87), ([7, 8, 9], 1)]),
            ),
            (
                0,
                64,
                64,
                palette_rle(&palette5, &[(0, 100), (1, 1), (2, 256), (3, 2), (4, 25)]),
            ),
            (
                64,
                64,
                64,
                raw_tile(64, 6, |x, y| {
                    [u8::try_from(x).unwrap(), u8::try_from(y).unwrap(), 77]
                }),
            ),
            (128, 64, 2, packed(2, 6, &palette2, |x, y| (x + y) % 2)),
        ];
        let mut expected = blank(130, 70);
        let mut data = Vec::new();
        for (x0, y0, w, tile) in &layout {
            data.extend_from_slice(&tile.bytes);
            paint(&mut expected, 130, *x0, *y0, *w, tile);
        }
        let first = update(&[zrle_rect(0, 0, 130, 70, &stream.pack(&data))]);
        harness.server.push(&first, chunking);
        let frame = wait_frame(&harness.screen, |_| true);
        assert_eq!(bgra(&frame), expected, "{chunking:?}");

        // Long runs (more than 255 pixels) in the same zlib stream, two updates in a row.
        let long = plain_rle(&[([1, 1, 1], 300), ([2, 2, 2], 3796)]);
        paint(&mut expected, 130, 0, 0, 64, &long);
        let second = update(&[zrle_rect(0, 0, 64, 64, &stream.pack(&long.bytes))]);
        let third_tile = solid(1, 1, [250, 251, 252]);
        paint(&mut expected, 130, 129, 69, 1, &third_tile);
        let third = update(&[zrle_rect(129, 69, 1, 1, &stream.pack(&third_tile.bytes))]);
        harness.server.push(&[second, third].concat(), chunking);
        let frame = wait_frame(&harness.screen, |frame| pixel_at(frame, 129, 69)[0] == 250);
        assert_eq!(bgra(&frame), expected, "{chunking:?}");
        assert_eq!(harness.screen.stats().codec, "zrle");
        assert_eq!(harness.server.settle().len(), 3 * 10);
    }
}

#[test]
fn copy_rect_and_desktop_size() {
    for chunking in CHUNKINGS {
        let harness = start(options());
        connect(&harness, 4, 2, chunking, false);
        let pixels: Vec<[u8; 4]> = (1..=8u8).map(|i| [i, i, i, 0]).collect();
        harness
            .server
            .push(&update(&[raw_rect(0, 0, 4, 2, &pixels)]), chunking);
        harness
            .server
            .push(&update(&[copy_rect(1, 0, 3, 2, 0, 0)]), chunking);
        let frame = wait_frame(&harness.screen, |frame| pixel_at(frame, 3, 0)[0] == 3);
        let row0: Vec<u8> = (0..4).map(|x| pixel_at(&frame, x, 0)[0]).collect();
        let row1: Vec<u8> = (0..4).map(|x| pixel_at(&frame, x, 1)[0]).collect();
        assert_eq!(row0, [1, 1, 2, 3]);
        assert_eq!(row1, [5, 5, 6, 7]);

        harness
            .server
            .push(&update(&[rect_header(0, 0, 8, 6, -223)]), chunking);
        let frame = wait_frame(&harness.screen, |frame| frame.width == 8);
        assert_eq!((frame.width, frame.height), (8, 6));
        assert_eq!(bgra(&frame).len(), 8 * 6 * 4);
        assert_eq!(
            harness.screen.state(),
            ScreenState::Ready(ScreenInfo {
                width: 8,
                height: 6,
                name: "fake-vm".to_owned()
            })
        );
        let expected = concat(&[
            update_request(true, 4, 2),
            update_request(true, 4, 2),
            update_request(false, 8, 6),
            update_request(true, 8, 6),
        ]);
        assert_eq!(harness.server.settle(), expected, "{chunking:?}");
    }
}

#[test]
fn continuous_updates_and_fences() {
    for chunking in CHUNKINGS {
        let harness = start(options());
        connect(&harness, 16, 9, chunking, false);
        let mut bytes = vec![150];
        bytes.extend_from_slice(&[248, 0, 0, 0, 0x80, 0, 0, 0x01, 3]);
        bytes.extend_from_slice(b"xyz");
        harness.server.push(&bytes, chunking);
        let mut expected = enable_continuous(16, 9);
        expected.extend_from_slice(&[248, 0, 0, 0, 0, 0, 0, 1, 3, b'x', b'y', b'z']);
        assert_eq!(harness.server.read(expected.len()), expected);

        harness
            .server
            .push(&update(&[raw_rect(0, 0, 1, 1, &[[1, 2, 3, 0]])]), chunking);
        wait_frame(&harness.screen, |_| true);
        // No incremental request after an update: the server streams on its own.
        assert!(harness.server.settle().is_empty());

        harness
            .server
            .push(&update(&[rect_header(0, 0, 20, 10, -223)]), chunking);
        wait_frame(&harness.screen, |frame| frame.width == 20);
        let mut expected = update_request(false, 20, 10);
        expected.extend_from_slice(&enable_continuous(20, 10));
        assert_eq!(harness.server.settle(), expected);
    }
}

#[test]
fn extended_desktop_size_resizes() {
    let harness = start_ready(options(), 4, 4, Chunking::Each(3));
    let mut rect = rect_header(0, 0, 6, 5, -308);
    rect.extend_from_slice(&[1, 0, 0, 0]);
    rect.extend_from_slice(&[0; 16]);
    harness.server.push(&update(&[rect]), Chunking::Each(5));
    let frame = wait_frame(&harness.screen, |frame| frame.width == 6);
    assert_eq!(frame.height, 5);
}

#[test]
fn protocol_violations_close_the_session_with_a_reason() {
    // Oversized ZRLE rectangle.
    let harness = start_ready(options(), 4, 4, Chunking::Whole);
    let mut bytes = vec![0, 0, 0, 1];
    bytes.extend_from_slice(&rect_header(0, 0, 4, 4, 16));
    bytes.extend_from_slice(&be32(16 * 1024 * 1024 + 1));
    harness.server.push(&bytes, Chunking::Whole);
    let reason = wait_closed(&harness.screen);
    assert!(reason.contains("ZRLE"), "{reason}");
    wait_for("the transport to be closed", || {
        harness.server.closed.load(Ordering::SeqCst)
    });
    // Input after the end is ignored and nothing panics.
    harness.screen.key(0x61, true);
    harness
        .screen
        .pointer(1, 1, atur_screen::PointerButtons::default());
    assert!(harness.screen.frame().is_none());

    // Rectangle outside of the screen.
    let harness = start_ready(options(), 4, 4, Chunking::Whole);
    harness.server.push(
        &update(&[raw_rect(3, 3, 2, 2, &[[0; 4]; 4])]),
        Chunking::Each(3),
    );
    assert!(wait_closed(&harness.screen).contains("outside"));

    // H.264 that was never requested.
    let harness = start_ready(options(), 4, 4, Chunking::Whole);
    let mut rect = rect_header(0, 0, 4, 4, 50);
    rect.extend_from_slice(&be32(2));
    rect.extend_from_slice(&be32(0));
    rect.extend_from_slice(&[0, 0]);
    harness.server.push(&update(&[rect]), Chunking::Whole);
    assert!(wait_closed(&harness.screen).contains("not requested"));

    // Oversized H.264 payload.
    let harness = start_ready(options(), 4, 4, Chunking::Whole);
    let mut rect = rect_header(0, 0, 4, 4, 50);
    rect.extend_from_slice(&be32(16 * 1024 * 1024 + 1));
    rect.extend_from_slice(&be32(0));
    harness.server.push(&update(&[rect]), Chunking::Whole);
    assert!(wait_closed(&harness.screen).contains("H.264"));

    // Corrupt zlib data.
    let harness = start_ready(options(), 4, 4, Chunking::Whole);
    harness.server.push(
        &update(&[zrle_rect(0, 0, 4, 4, &[1, 2, 3, 4, 5, 6, 7, 8])]),
        Chunking::Whole,
    );
    assert!(wait_closed(&harness.screen).contains("ZRLE"));

    // A rectangle that claims more pixels than the screen has.
    let harness = start_ready(options(), 4, 4, Chunking::Whole);
    harness.server.push(
        &update(&[rect_header(0, 0, 65_535, 65_535, 0)]),
        Chunking::Whole,
    );
    assert!(!wait_closed(&harness.screen).is_empty());

    // A refusal during the handshake.
    let harness = start(options());
    let mut bytes = b"RFB 003.008\n".to_vec();
    bytes.push(0);
    bytes.extend_from_slice(&be32(21));
    bytes.extend_from_slice(b"no wayland session :(");
    harness.server.push(&bytes, Chunking::Each(4));
    let reason = wait_closed(&harness.screen);
    assert!(reason.contains("no wayland session :("), "{reason}");
}

#[test]
fn losing_the_connection_and_closing() {
    let harness = start_ready(options(), 4, 4, Chunking::Whole);
    harness.server.hang_up();
    let reason = wait_closed(&harness.screen);
    assert!(reason.contains("lost"), "{reason}");

    let harness = start_ready(options(), 4, 4, Chunking::Whole);
    let clone = harness.screen.clone();
    drop(clone);
    assert!(matches!(harness.screen.state(), ScreenState::Ready(_)));
    harness.screen.close();
    assert_eq!(wait_closed(&harness.screen), "");
    assert!(harness.server.closed.load(Ordering::SeqCst));

    // The last handle going away closes the transport.
    let Harness { screen, server, .. } = start_ready(options(), 4, 4, Chunking::Whole);
    drop(screen);
    wait_for("the transport to be closed", || {
        server.closed.load(Ordering::SeqCst)
    });
}

type Calls = Arc<Mutex<Vec<(Vec<u8>, bool)>>>;

struct FakeH264 {
    sink: FrameSink,
    calls: Calls,
}

impl H264Decoder for FakeH264 {
    fn name(&self) -> String {
        "fake".to_owned()
    }

    fn decode(&mut self, annex_b: &[u8], reset: bool) -> Result<(), DecodeError> {
        self.calls.lock().unwrap().push((annex_b.to_vec(), reset));
        if annex_b[0] == 0xff {
            return Err(DecodeError("broken stream".to_owned()));
        }
        (self.sink)(Frame {
            id: 0,
            width: 4,
            height: 4,
            pixels: Pixels::Bgra(Arc::from(vec![annex_b[0]; 64])),
        });
        Ok(())
    }
}

fn h264_update(flags: u32, payload: &[u8]) -> Vec<u8> {
    let mut rect = rect_header(0, 0, 4, 4, 50);
    rect.extend_from_slice(&be32(payload.len()));
    rect.extend_from_slice(&flags.to_be_bytes());
    rect.extend_from_slice(payload);
    update(&[rect])
}

#[test]
fn h264_frames_come_from_the_decoder_and_errors_ask_for_a_key_frame() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let decoder_calls = Arc::clone(&calls);
    let harness = start_with(options(), move |sink| {
        Some(Box::new(FakeH264 {
            sink,
            calls: decoder_calls,
        }) as Box<dyn H264Decoder>)
    });
    connect(&harness, 4, 4, Chunking::Each(5), true);

    harness
        .server
        .push(&h264_update(3, &[7, 0, 0, 1]), Chunking::Each(3));
    let first = wait_frame(&harness.screen, |_| true);
    assert_eq!(pixel_at(&first, 0, 0), [7, 7, 7, 7]);
    assert!(first.id >= 1);
    wait_for("the codec name", || {
        harness.screen.stats().codec == "h264 (fake)"
    });

    thread::sleep(Duration::from_millis(10));
    harness
        .server
        .push(&h264_update(0, &[9, 9]), Chunking::Whole);
    let second = wait_frame(&harness.screen, |frame| pixel_at(frame, 0, 0)[0] == 9);
    assert!(second.id > first.id);

    harness
        .server
        .push(&h264_update(0, &[0xff, 1]), Chunking::Whole);
    let expected = concat(&[
        update_request(true, 4, 4),
        update_request(true, 4, 4),
        update_request(false, 4, 4),
        update_request(true, 4, 4),
    ]);
    assert_eq!(harness.server.settle(), expected);
    // The session keeps going after the decoder error.
    assert!(matches!(harness.screen.state(), ScreenState::Ready(_)));
    assert_eq!(
        *calls.lock().unwrap(),
        [
            (vec![7, 0, 0, 1], true),
            (vec![9, 9], false),
            (vec![0xff, 1], false)
        ]
    );
}

#[test]
fn h264_is_not_offered_when_not_preferred() {
    let asked = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&asked);
    let harness = start_with(
        ScreenOptions {
            view_only: false,
            prefer_h264: false,
        },
        move |_sink| {
            flag.store(true, Ordering::SeqCst);
            None
        },
    );
    connect(&harness, 4, 4, Chunking::Whole, false);
    assert!(!asked.load(Ordering::SeqCst));
}

#[test]
fn newest_frame_wins_and_dropped_frames_are_counted() {
    let harness = start_ready(options(), 2, 1, Chunking::Whole);
    for value in 1..=3u8 {
        harness.server.push(
            &update(&[raw_rect(0, 0, 1, 1, &[[value, value, value, 0]])]),
            Chunking::Whole,
        );
        thread::sleep(Duration::from_millis(20));
    }
    // Three frames were published and nobody looked at them.
    wait_for("two dropped frames", || harness.screen.stats().dropped >= 2);
    let frame = harness.screen.frame().unwrap();
    assert_eq!(pixel_at(&frame, 0, 0), [3, 3, 3, 255]);
    assert!(frame.id >= 3);
    assert_eq!(harness.screen.stats().dropped, 2);

    // The frame was taken, so replacing it is not a drop.
    harness.server.push(
        &update(&[raw_rect(0, 0, 1, 1, &[[4, 4, 4, 0]])]),
        Chunking::Whole,
    );
    let newest = wait_frame(&harness.screen, |frame| pixel_at(frame, 0, 0)[0] == 4);
    assert!(newest.id > frame.id);
    assert_eq!(harness.screen.stats().dropped, 2);
}

#[test]
fn a_throttled_update_is_published_when_the_stream_goes_idle() {
    let harness = start_ready(options(), 2, 1, Chunking::Whole);
    for value in [1u8, 2] {
        harness.server.push(
            &update(&[raw_rect(0, 0, 1, 1, &[[value, value, value, 0]])]),
            Chunking::Whole,
        );
    }
    wait_frame(&harness.screen, |frame| pixel_at(frame, 0, 0)[0] == 2);
}

#[test]
fn wake_calls_are_rate_limited() {
    let harness = start_ready(options(), 2, 1, Chunking::Whole);
    let before = harness.wakes.load(Ordering::SeqCst);
    let started = Instant::now();
    for index in 0..600usize {
        let value = u8::try_from(index % 250).unwrap();
        harness.server.push(
            &update(&[raw_rect(0, 0, 1, 1, &[[value, value, value, 0]])]),
            Chunking::Whole,
        );
        thread::sleep(Duration::from_micros(200));
    }
    wait_for("the last update", || {
        harness.server.processed.load(Ordering::SeqCst)
            == harness.server.pushed.load(Ordering::SeqCst)
    });
    thread::sleep(Duration::from_millis(50));
    let calls = harness.wakes.load(Ordering::SeqCst) - before;
    let seconds = started.elapsed().as_secs_f64();
    let allowed = seconds * 118.0 + 3.0;
    assert!(calls >= 1);
    assert!(
        f64::from(u32::try_from(calls).unwrap()) <= allowed,
        "{calls} wakes in {seconds} s"
    );
}

#[test]
fn stats_report_codec_rate_and_throughput() {
    let harness = start_ready(options(), 2, 1, Chunking::Whole);
    assert_eq!(harness.screen.stats().codec, "");
    for value in 0..12u8 {
        harness.server.push(
            &update(&[raw_rect(0, 0, 1, 1, &[[value, value, value, 0]])]),
            Chunking::Whole,
        );
        thread::sleep(Duration::from_millis(100));
    }
    let mut seen = None;
    wait_for("a frame rate", || {
        let stats = harness.screen.stats();
        if stats.fps > 0.0 {
            seen = Some(stats);
            return true;
        }
        false
    });
    let stats = seen.unwrap();
    assert_eq!(stats.codec, "raw");
    assert!(stats.kbps > 0.0);
    assert!(stats.fps < 100.0, "{}", stats.fps);
    assert!(stats.decode_ms >= 0.0);
}
