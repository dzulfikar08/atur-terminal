#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::io::{Read, Write};

use atur_screen::{PointerButtons, ScreenOptions};
use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use support::*;

const NONE: PointerButtons = PointerButtons {
    left: false,
    middle: false,
    right: false,
};
const LEFT: PointerButtons = PointerButtons {
    left: true,
    middle: false,
    right: false,
};

fn ready() -> Harness {
    start_ready(ScreenOptions::default(), 100, 80, Chunking::Whole)
}

fn pointer(mask: u8, x: u16, y: u16) -> Input {
    Input::Pointer { mask, x, y }
}

fn key(down: bool, keysym: u32) -> Input {
    Input::Key { down, keysym }
}

#[test]
fn first_pointer_event_is_sent_at_once_and_clamped() {
    let harness = ready();
    harness.screen.pointer(-5, 99_999, NONE);
    assert_eq!(harness.server.read(6), pointer_bytes(0, 0, 79));
    harness.screen.pointer(
        100,
        3,
        PointerButtons {
            left: false,
            middle: true,
            right: true,
        },
    );
    assert_eq!(harness.server.read(6), pointer_bytes(6, 99, 3));
}

#[test]
fn pointer_moves_are_coalesced_to_the_newest_position() {
    let harness = ready();
    harness.screen.pointer(10, 10, NONE);
    for x in 11..=110 {
        harness.screen.pointer(x, 20, NONE);
    }
    let events = parse_input(&harness.server.settle());
    assert_eq!(events[0], pointer(0, 10, 10));
    assert_eq!(*events.last().unwrap(), pointer(0, 99, 20));
    assert!(events.len() < 50, "{} events", events.len());
}

#[test]
fn pending_move_is_flushed_before_a_key_event() {
    let harness = ready();
    harness.screen.pointer(1, 1, NONE);
    harness.screen.pointer(2, 2, NONE);
    harness.screen.pointer(3, 3, NONE);
    harness.screen.key(0x61, true);
    harness.screen.key(0x61, false);
    let events = parse_input(&harness.server.settle());
    let down = events.iter().position(|e| *e == key(true, 0x61)).unwrap();
    assert_eq!(events[down - 1], pointer(0, 3, 3));
    assert_eq!(events[down + 1..], [key(false, 0x61)]);
}

#[test]
fn button_changes_are_never_dropped_or_reordered() {
    let harness = ready();
    for i in 0..50 {
        harness.screen.pointer(i, i, LEFT);
        harness.screen.pointer(i, i, NONE);
    }
    let events = parse_input(&harness.server.settle());
    assert_eq!(events.len(), 100);
    for (index, event) in events.iter().enumerate() {
        let i = u16::try_from(index / 2).unwrap();
        let mask = u8::from(index % 2 == 0);
        assert_eq!(*event, pointer(mask, i, i), "event {index}");
    }
}

#[test]
fn a_button_press_carries_the_newest_position() {
    let harness = ready();
    harness.screen.pointer(5, 5, NONE);
    harness.screen.pointer(6, 6, NONE);
    harness.screen.pointer(6, 6, LEFT);
    let events = parse_input(&harness.server.settle());
    assert_eq!(*events.last().unwrap(), pointer(1, 6, 6));
    assert!(events.len() <= 3);
}

#[test]
fn scroll_sends_wheel_button_presses() {
    let harness = ready();
    harness.screen.scroll(3, 4, 0, 2);
    let mut expected = Vec::new();
    for _ in 0..2 {
        expected.extend_from_slice(&pointer_bytes(8, 3, 4));
        expected.extend_from_slice(&pointer_bytes(0, 3, 4));
    }
    assert_eq!(harness.server.read(expected.len()), expected);

    harness.screen.scroll(3, 4, 0, -1);
    harness.screen.scroll(3, 4, 1, 0);
    harness.screen.scroll(3, 4, -1, 0);
    harness.screen.scroll(3, 4, 0, 0);
    let mut expected = Vec::new();
    for mask in [16, 64, 32] {
        expected.extend_from_slice(&pointer_bytes(mask, 3, 4));
        expected.extend_from_slice(&pointer_bytes(0, 3, 4));
    }
    assert_eq!(harness.server.settle(), expected);

    // Held buttons stay held while scrolling; the notch count is bounded.
    harness.screen.pointer(3, 4, LEFT);
    harness.screen.scroll(3, 4, 0, 1000);
    let events = parse_input(&harness.server.settle());
    assert_eq!(events[0], pointer(1, 3, 4));
    assert_eq!(events[1], pointer(9, 3, 4));
    assert_eq!(events[2], pointer(1, 3, 4));
    assert_eq!(events.len(), 1 + 20);
}

#[test]
fn release_all_releases_keys_and_buttons_in_order() {
    let harness = ready();
    harness.screen.key(0x61, true);
    harness.screen.key(0xffe1, true);
    harness.screen.key(0x61, true);
    harness.screen.key(0x62, true);
    harness.screen.key(0x62, false);
    harness.screen.pointer(1, 1, LEFT);
    assert_eq!(
        parse_input(&harness.server.settle()),
        [
            key(true, 0x61),
            key(true, 0xffe1),
            key(true, 0x61),
            key(true, 0x62),
            key(false, 0x62),
            pointer(1, 1, 1),
        ]
    );
    harness.screen.release_all();
    assert_eq!(
        parse_input(&harness.server.settle()),
        [key(false, 0x61), key(false, 0xffe1), pointer(0, 1, 1)]
    );
    harness.screen.release_all();
    assert!(harness.server.settle().is_empty());
}

#[test]
fn view_only_ignores_input_but_still_releases() {
    let harness = start_ready(
        ScreenOptions {
            view_only: true,
            prefer_h264: true,
        },
        100,
        80,
        Chunking::Whole,
    );
    harness.screen.pointer(1, 1, LEFT);
    harness.screen.scroll(1, 1, 0, 1);
    harness.screen.key(0x61, true);
    harness.screen.send_clipboard("text");
    assert!(harness.server.settle().is_empty());

    harness.screen.set_view_only(false);
    harness.screen.key(0x61, true);
    harness.screen.set_view_only(true);
    assert_eq!(
        parse_input(&harness.server.settle()),
        [key(true, 0x61), key(false, 0x61)]
    );
}

fn caps_body(flags: u32) -> Vec<u8> {
    let mut body = flags.to_be_bytes().to_vec();
    body.extend_from_slice(&[0, 0, 0, 0]);
    body
}

const OUR_CAPS: [u8; 16] = [
    6, 0, 0, 0, 0xff, 0xff, 0xff, 0xf8, 0x1f, 0, 0, 1, 0, 0, 0, 0,
];
const NOTIFY_TEXT: [u8; 12] = [6, 0, 0, 0, 0xff, 0xff, 0xff, 0xfc, 0x08, 0, 0, 1];
const REQUEST_TEXT: [u8; 12] = [6, 0, 0, 0, 0xff, 0xff, 0xff, 0xfc, 0x02, 0, 0, 1];

fn wait_clipboard(harness: &Harness) -> String {
    let mut text = None;
    wait_for("remote clipboard text", || {
        text = harness.screen.take_clipboard();
        text.is_some()
    });
    text.unwrap()
}

#[test]
fn extended_clipboard_in_both_directions() {
    for chunking in [Chunking::Whole, Chunking::Each(1), Chunking::Random(5)] {
        let harness = start_ready(ScreenOptions::default(), 100, 80, chunking);
        harness
            .server
            .push(&ext_clipboard(&caps_body(0x1f00_0001)), chunking);
        assert_eq!(harness.server.read(16), OUR_CAPS);

        // Local to remote: Notify, then the server's Request is answered with Provide.
        harness.screen.send_clipboard("h\u{e9}llo\nworld");
        assert_eq!(harness.server.read(12), NOTIFY_TEXT);
        harness
            .server
            .push(&ext_clipboard(&0x0200_0001u32.to_be_bytes()), chunking);
        let header = harness.server.read(8);
        assert_eq!(header[..4], [6, 0, 0, 0]);
        let length = -i32::from_be_bytes([header[4], header[5], header[6], header[7]]);
        let body = harness.server.read(usize::try_from(length).unwrap());
        assert_eq!(body[..4], 0x1000_0001u32.to_be_bytes());
        let mut payload = Vec::new();
        ZlibDecoder::new(&body[4..])
            .read_to_end(&mut payload)
            .unwrap();
        let text = "h\u{e9}llo\r\nworld\0";
        assert_eq!(payload[..4], be32(text.len()));
        assert_eq!(&payload[4..], text.as_bytes());

        // A Peek is answered with a Notify that lists the text.
        harness
            .server
            .push(&ext_clipboard(&0x0400_0000u32.to_be_bytes()), chunking);
        assert_eq!(harness.server.read(12), NOTIFY_TEXT);

        // Remote to local: Notify is answered with Request, Provide delivers the text.
        harness
            .server
            .push(&ext_clipboard(&0x0800_0001u32.to_be_bytes()), chunking);
        assert_eq!(harness.server.read(12), REQUEST_TEXT);
        harness
            .server
            .push(&ext_provide_text("remote\r\ntext \u{20ac}"), chunking);
        assert_eq!(wait_clipboard(&harness), "remote\ntext \u{20ac}");
        assert_eq!(harness.screen.take_clipboard(), None);

        // An absurdly large Provide is dropped but consumed; the session carries on.
        let mut big = ZlibEncoder::new(Vec::new(), Compression::fast());
        big.write_all(&be32(3 << 20)).unwrap();
        big.write_all(&vec![b'a'; 3 << 20]).unwrap();
        let mut body = 0x1000_0001u32.to_be_bytes().to_vec();
        body.extend_from_slice(&big.finish().unwrap());
        harness.server.push(&ext_clipboard(&body), chunking);
        harness
            .server
            .push(&update(&[raw_rect(0, 0, 1, 1, &[[9, 9, 9, 0]])]), chunking);
        let frame = wait_frame(&harness.screen, |_| true);
        assert_eq!(pixel_at(&frame, 0, 0), [9, 9, 9, 255]);
        assert_eq!(harness.screen.take_clipboard(), None);
    }
}

#[test]
fn legacy_clipboard_in_both_directions() {
    let harness = ready();
    harness.screen.send_clipboard("h\u{e9}\u{20ac}");
    assert_eq!(
        harness.server.read(11),
        [6, 0, 0, 0, 0, 0, 0, 3, b'h', 0xe9, b'?']
    );
    harness
        .server
        .push(&cut_text(b"caf\xe9\r\n"), Chunking::Each(2));
    assert_eq!(wait_clipboard(&harness), "caf\u{e9}\n");

    // An empty or oversized clipboard is not sent.
    harness.screen.send_clipboard("");
    harness.screen.send_clipboard(&"a".repeat((1 << 20) + 1));
    assert!(harness.server.settle().is_empty());
}

#[test]
fn a_server_without_the_notify_flow_gets_legacy_cut_text() {
    let harness = ready();
    harness
        .server
        .push(&ext_clipboard(&caps_body(0x0100_0001)), Chunking::Whole);
    assert_eq!(harness.server.read(16), OUR_CAPS);
    harness.screen.send_clipboard("plain");
    let mut expected = vec![6, 0, 0, 0, 0, 0, 0, 5];
    expected.extend_from_slice(b"plain");
    assert_eq!(harness.server.read(expected.len()), expected);
}
