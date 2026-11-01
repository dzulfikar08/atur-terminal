//! The RFB handshake against a server that is strict about it, the way wayvnc
//! is: every client byte before `ServerInit` is read as part of the handshake,
//! so the viewer may send nothing else until the handshake is over.
//!
//! Regression: a clipboard announced while connecting (a Mac with text on the
//! clipboard) went out as `ClientCutText` (type 6) between the version and the
//! security type, and wayvnc answered "Illegal security type" for type 6.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use atur_screen::{PointerButtons, ScreenOptions};
use support::*;

const WIDTH: usize = 64;
const HEIGHT: usize = 48;

fn cut_text(text: &str) -> Vec<u8> {
    let mut bytes = vec![6, 0, 0, 0];
    bytes.extend_from_slice(&be32(text.len()));
    bytes.extend_from_slice(text.as_bytes());
    bytes
}

/// Everything a viewer may do before the server has finished greeting it.
fn fidget(harness: &Harness) {
    harness.screen.send_clipboard("early clipboard");
    harness.screen.key(0x61, true);
    harness.screen.key(0x61, false);
    harness.screen.pointer(5, 5, PointerButtons::default());
    harness.screen.scroll(5, 5, 0, 3);
    harness.screen.release_all();
}

/// The server's first words after `ServerInit`: a one pixel update.
fn first_update() -> Vec<u8> {
    update(&[raw_rect(0, 0, 1, 1, &[[0, 0, 0, 0]])])
}

fn expected() -> Vec<u8> {
    let mut bytes = handshake_reply(WIDTH, HEIGHT, false);
    bytes.extend_from_slice(&cut_text("early clipboard"));
    // The first update is answered with an incremental request.
    bytes.extend_from_slice(&update_request(true, WIDTH, HEIGHT));
    bytes
}

#[test]
fn nothing_but_the_handshake_is_sent_until_server_init() {
    let harness = start(ScreenOptions::default());
    fidget(&harness);
    assert!(
        harness.server.settle().is_empty(),
        "the viewer spoke before the server did"
    );

    let hello = hello(WIDTH, HEIGHT);
    // The version line alone: only our version may come back.
    harness.server.push(&hello[..12], Chunking::Whole);
    assert_eq!(harness.server.read(12), b"RFB 003.008\n");
    fidget(&harness);
    assert!(harness.server.settle().is_empty());

    // The security type count alone, then the type.
    harness.server.push(&hello[12..13], Chunking::Whole);
    fidget(&harness);
    assert!(harness.server.settle().is_empty());
    harness.server.push(&hello[13..14], Chunking::Whole);
    assert_eq!(harness.server.read(1), [1]);
    fidget(&harness);
    assert!(harness.server.settle().is_empty());

    // The security result and ServerInit, a byte at a time.
    harness.server.push(&hello[14..], Chunking::Each(1));
    wait_ready(&harness.screen);
    let reply = handshake_reply(WIDTH, HEIGHT, false);
    assert_eq!(harness.server.read(reply.len() - 13), reply[13..]);
    // The clipboard waits for the server's first words, so its capabilities are known.
    assert!(harness.server.settle().is_empty());
    harness.server.push(&first_update(), Chunking::Whole);
    let clip = cut_text("early clipboard");
    assert_eq!(harness.server.read(clip.len()), clip);
    assert_eq!(harness.server.read(10), update_request(true, WIDTH, HEIGHT));
    assert!(harness.server.settle().is_empty());
}

#[test]
fn input_flows_again_once_the_handshake_is_over() {
    let harness = start(ScreenOptions::default());
    harness.server.push(&hello(WIDTH, HEIGHT), Chunking::Whole);
    wait_ready(&harness.screen);
    let reply = handshake_reply(WIDTH, HEIGHT, false);
    assert_eq!(harness.server.read(reply.len()), reply);
    harness.screen.key(0x61, true);
    assert_eq!(harness.server.read(8), [4, 1, 0, 0, 0, 0, 0, 0x61]);
}

#[test]
fn every_split_of_the_greeting_keeps_the_handshake_clean() {
    let greeting = hello(WIDTH, HEIGHT);
    let wanted = expected();
    // Cut in two at every position, including between the count and the types.
    for at in 1..greeting.len() {
        let harness = start(ScreenOptions::default());
        fidget(&harness);
        harness.server.push(&greeting[..at], Chunking::Whole);
        fidget(&harness);
        harness.server.push(&greeting[at..], Chunking::Whole);
        wait_ready(&harness.screen);
        harness.server.push(&first_update(), Chunking::Whole);
        assert_eq!(
            harness.server.read(wanted.len()),
            wanted,
            "greeting cut at {at}"
        );
        assert!(harness.server.settle().is_empty(), "greeting cut at {at}");
    }
}

#[test]
fn the_greeting_in_odd_chunks_keeps_the_handshake_clean() {
    let greeting = hello(WIDTH, HEIGHT);
    let wanted = expected();
    let chunkings = [
        Chunking::Each(1),
        Chunking::Each(2),
        Chunking::Each(3),
        Chunking::Each(13),
        Chunking::Random(1),
        Chunking::Random(7),
        Chunking::Random(0xfeed_f00d),
    ];
    for chunking in chunkings {
        let harness = start(ScreenOptions::default());
        // The clipboard races the greeting from another thread.
        let screen = harness.screen.clone();
        let racer = std::thread::spawn(move || {
            for _ in 0..50 {
                screen.key(0x62, true);
                std::thread::yield_now();
            }
            screen.send_clipboard("early clipboard");
        });
        harness.server.push(&greeting, chunking);
        racer.join().unwrap();
        wait_ready(&harness.screen);
        harness.server.push(&first_update(), Chunking::Whole);
        // Keys that raced in after the handshake are fine; the stream must still
        // start with the handshake, exactly once, and carry the clipboard once.
        let sent = harness.server.settle();
        let handshake = &handshake_reply(WIDTH, HEIGHT, false);
        assert!(sent.starts_with(handshake), "{chunking:?}: {sent:?}");
        let clip = cut_text("early clipboard");
        let count = sent
            .windows(clip.len())
            .filter(|part| *part == clip)
            .count();
        assert_eq!(count, 1, "{chunking:?}");
        assert!(sent.len() >= wanted.len(), "{chunking:?}");
    }
}

#[test]
fn a_clipboard_set_before_the_version_is_sent_after_the_handshake_in_legacy_form() {
    let harness = start(ScreenOptions::default());
    harness.screen.send_clipboard("h\u{e9}");
    harness
        .server
        .push(&hello(WIDTH, HEIGHT), Chunking::Each(5));
    wait_ready(&harness.screen);
    harness.server.push(&first_update(), Chunking::Whole);
    let reply = handshake_reply(WIDTH, HEIGHT, false);
    assert_eq!(harness.server.read(reply.len()), reply);
    assert_eq!(
        harness.server.read(10),
        [6, 0, 0, 0, 0, 0, 0, 2, 0x68, 0xe9]
    );
}
