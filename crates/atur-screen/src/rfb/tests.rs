use std::sync::{Arc, Mutex};

use super::*;
use crate::decode::DecodeError;

fn be16(value: usize) -> [u8; 2] {
    u16::try_from(value).unwrap().to_be_bytes()
}

fn be32(value: usize) -> [u8; 4] {
    u32::try_from(value).unwrap().to_be_bytes()
}

fn hello(width: usize, height: usize) -> Vec<u8> {
    let mut bytes = b"RFB 003.008\n".to_vec();
    bytes.extend_from_slice(&[1, 1]);
    bytes.extend_from_slice(&[0, 0, 0, 0]);
    bytes.extend_from_slice(&be16(width));
    bytes.extend_from_slice(&be16(height));
    bytes.extend_from_slice(&[0u8; 16]);
    bytes.extend_from_slice(&be32(4));
    bytes.extend_from_slice(b"test");
    bytes
}

fn feed_chunks(machine: &mut Machine, data: &[u8], size: usize) -> Result<(), ProtocolError> {
    for chunk in data.chunks(size) {
        machine.feed(chunk)?;
    }
    Ok(())
}

fn ready(width: usize, height: usize) -> Machine {
    let mut machine = Machine::new(None);
    machine.feed(&hello(width, height)).unwrap();
    machine.take_out();
    machine.take_events();
    machine
}

fn update_header(rects: usize) -> Vec<u8> {
    let mut bytes = vec![0, 0];
    bytes.extend_from_slice(&be16(rects));
    bytes
}

fn rect_header(x: usize, y: usize, w: usize, h: usize, encoding: i32) -> Vec<u8> {
    let mut bytes = Vec::new();
    for value in [x, y, w, h] {
        bytes.extend_from_slice(&be16(value));
    }
    bytes.extend_from_slice(&encoding.to_be_bytes());
    bytes
}

fn pixel(machine: &Machine, x: usize, y: usize) -> [u8; 4] {
    let (width, _) = machine.size();
    let at = (y * width + x) * 4;
    let data = machine.pixels();
    [data[at], data[at + 1], data[at + 2], data[at + 3]]
}

fn incremental(width: usize, height: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    msg::update_request(&mut bytes, true, width, height);
    bytes
}

fn full(width: usize, height: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    msg::update_request(&mut bytes, false, width, height);
    bytes
}

#[test]
fn handshake_in_single_bytes_replies_exactly() {
    let mut machine = Machine::new(None);
    feed_chunks(&mut machine, &hello(640, 480), 1).unwrap();
    let mut expected = b"RFB 003.008\n".to_vec();
    expected.extend_from_slice(&[1, 1]);
    msg::set_pixel_format(&mut expected);
    msg::set_encodings(&mut expected, &msg::encodings(false));
    msg::update_request(&mut expected, false, 640, 480);
    assert_eq!(machine.take_out(), expected);
    assert_eq!(
        machine.take_events(),
        [Event::Ready {
            width: 640,
            height: 480,
            name: "test".to_owned()
        }]
    );
    assert_eq!(machine.size(), (640, 480));
    assert_eq!(pixel(&machine, 0, 0), [0, 0, 0, 255]);
}

#[test]
fn encodings_without_and_with_h264() {
    assert_eq!(
        msg::encodings(false),
        [
            16,
            1,
            0,
            -223,
            -308,
            -312,
            -313,
            msg::ENC_EXT_CLIPBOARD,
            -224
        ]
    );
    assert_eq!(msg::encodings(true)[..2], [50, 16]);
}

#[test]
fn version_3_7_has_no_security_result() {
    let mut bytes = b"RFB 003.007\n".to_vec();
    bytes.extend_from_slice(&[1, 1]);
    bytes.extend_from_slice(&be16(8));
    bytes.extend_from_slice(&be16(8));
    bytes.extend_from_slice(&[0u8; 16]);
    bytes.extend_from_slice(&be32(0));
    let mut machine = Machine::new(None);
    feed_chunks(&mut machine, &bytes, 3).unwrap();
    let out = machine.take_out();
    assert!(out.starts_with(b"RFB 003.007\n\x01\x01"));
    assert_eq!(machine.size(), (8, 8));
}

#[test]
fn a_newer_server_version_is_answered_with_3_8() {
    let mut machine = Machine::new(None);
    machine.feed(b"RFB 003.889\n").unwrap();
    assert_eq!(machine.take_out(), b"RFB 003.008\n");
}

#[test]
fn refusals_and_bad_greetings_are_errors() {
    let mut machine = Machine::new(None);
    assert!(machine.feed(b"SSH-2.0-OpenSSH\n").is_err());
    assert!(machine.feed(b"more").is_err());

    let mut machine = Machine::new(None);
    let mut bytes = b"RFB 003.008\n".to_vec();
    bytes.push(0);
    bytes.extend_from_slice(&be32(6));
    bytes.extend_from_slice(b"nope!!");
    let error = feed_chunks(&mut machine, &bytes, 2).unwrap_err();
    assert!(error.to_string().contains("nope!!"), "{error}");

    let mut machine = Machine::new(None);
    let error = machine.feed(b"RFB 003.008\n\x01\x02").unwrap_err();
    assert!(error.to_string().contains("login"), "{error}");

    let mut machine = Machine::new(None);
    let mut bytes = b"RFB 003.008\n\x01\x01".to_vec();
    bytes.extend_from_slice(&be32(1));
    bytes.extend_from_slice(&be32(3));
    bytes.extend_from_slice(b"bad");
    let error = machine.feed(&bytes).unwrap_err();
    assert!(error.to_string().contains("bad"), "{error}");
}

#[test]
fn absurd_screen_sizes_are_rejected() {
    for (width, height) in [(0, 10), (10, 0), (16_385, 10), (16_384, 16_384)] {
        let mut machine = Machine::new(None);
        assert!(
            machine.feed(&hello(width, height)).is_err(),
            "{width}x{height}"
        );
    }
}

#[test]
fn raw_rectangle_in_single_bytes() {
    let mut machine = ready(4, 4);
    let mut bytes = update_header(1);
    bytes.extend_from_slice(&rect_header(1, 1, 2, 2, 0));
    for pixel in [[1u8, 2, 3, 0], [4, 5, 6, 77], [7, 8, 9, 0], [10, 11, 12, 0]] {
        bytes.extend_from_slice(&pixel);
    }
    feed_chunks(&mut machine, &bytes, 1).unwrap();
    assert_eq!(pixel(&machine, 1, 1), [1, 2, 3, 255]);
    assert_eq!(pixel(&machine, 2, 1), [4, 5, 6, 255]);
    assert_eq!(pixel(&machine, 1, 2), [7, 8, 9, 255]);
    assert_eq!(pixel(&machine, 2, 2), [10, 11, 12, 255]);
    assert_eq!(pixel(&machine, 0, 0), [0, 0, 0, 255]);
    assert_eq!(
        machine.take_events(),
        [Event::Update {
            codec: Some(Codec::Raw),
            dirty: true
        }]
    );
    assert_eq!(machine.take_out(), incremental(4, 4));
}

#[test]
fn copy_rect_handles_overlap_in_both_directions() {
    let mut machine = ready(4, 2);
    let mut bytes = update_header(1);
    bytes.extend_from_slice(&rect_header(0, 0, 4, 2, 0));
    for value in 1..=8u8 {
        bytes.extend_from_slice(&[value, value, value, 0]);
    }
    machine.feed(&bytes).unwrap();

    // Move columns 0..3 one pixel to the right, same rows.
    let mut bytes = update_header(1);
    bytes.extend_from_slice(&rect_header(1, 0, 3, 2, 1));
    bytes.extend_from_slice(&[0, 0, 0, 0]);
    feed_chunks(&mut machine, &bytes, 3).unwrap();
    let row0: Vec<u8> = (0..4).map(|x| pixel(&machine, x, 0)[0]).collect();
    let row1: Vec<u8> = (0..4).map(|x| pixel(&machine, x, 1)[0]).collect();
    assert_eq!(row0, [1, 1, 2, 3]);
    assert_eq!(row1, [5, 5, 6, 7]);

    // Copy row 0 down onto row 1.
    let mut bytes = update_header(1);
    bytes.extend_from_slice(&rect_header(0, 1, 4, 1, 1));
    bytes.extend_from_slice(&[0, 0, 0, 0]);
    machine.feed(&bytes).unwrap();
    let row1: Vec<u8> = (0..4).map(|x| pixel(&machine, x, 1)[0]).collect();
    assert_eq!(row1, [1, 1, 2, 3]);
}

#[test]
fn desktop_size_reallocates_and_requests_everything() {
    let mut machine = ready(4, 4);
    let mut bytes = update_header(1);
    bytes.extend_from_slice(&rect_header(0, 0, 8, 6, -223));
    feed_chunks(&mut machine, &bytes, 5).unwrap();
    assert_eq!(machine.size(), (8, 6));
    assert_eq!(machine.pixels().len(), 8 * 6 * 4);
    let events = machine.take_events();
    assert_eq!(
        events[0],
        Event::Resized {
            width: 8,
            height: 6
        }
    );
    let mut expected = full(8, 6);
    expected.extend_from_slice(&incremental(8, 6));
    assert_eq!(machine.take_out(), expected);
}

#[test]
fn extended_desktop_size_resizes_unless_the_request_failed() {
    let mut machine = ready(4, 4);
    let mut bytes = update_header(2);
    // Same size: nothing happens.
    bytes.extend_from_slice(&rect_header(0, 0, 4, 4, -308));
    bytes.extend_from_slice(&[1, 0, 0, 0]);
    bytes.extend_from_slice(&[0u8; 16]);
    // Failed client-initiated request (reason 1, status 3): ignored.
    bytes.extend_from_slice(&rect_header(1, 3, 9, 9, -308));
    bytes.extend_from_slice(&[0, 0, 0, 0]);
    machine.feed(&bytes).unwrap();
    assert_eq!(machine.size(), (4, 4));

    let mut bytes = update_header(1);
    bytes.extend_from_slice(&rect_header(0, 0, 6, 5, -308));
    bytes.extend_from_slice(&[2, 0, 0, 0]);
    bytes.extend_from_slice(&[0u8; 32]);
    feed_chunks(&mut machine, &bytes, 7).unwrap();
    assert_eq!(machine.size(), (6, 5));
}

#[test]
fn continuous_updates_signal_switches_off_incremental_requests() {
    let mut machine = ready(4, 4);
    machine.feed(&[msg::MSG_ENABLE_CONTINUOUS_UPDATES]).unwrap();
    let mut expected = Vec::new();
    msg::enable_continuous_updates(&mut expected, 4, 4);
    assert_eq!(machine.take_out(), expected);
    machine.feed(&[msg::MSG_ENABLE_CONTINUOUS_UPDATES]).unwrap();
    assert!(machine.take_out().is_empty());
    let mut bytes = update_header(1);
    bytes.extend_from_slice(&rect_header(0, 0, 1, 1, 0));
    bytes.extend_from_slice(&[1, 2, 3, 0]);
    machine.feed(&bytes).unwrap();
    assert!(machine.take_out().is_empty());

    // The pseudo rectangle works as a signal as well.
    let mut machine = ready(4, 4);
    let mut bytes = update_header(2);
    bytes.extend_from_slice(&rect_header(0, 0, 0, 0, -313));
    bytes.extend_from_slice(&rect_header(0, 0, 0, 0, -224));
    machine.feed(&bytes).unwrap();
    assert_eq!(machine.take_out(), expected);
}

#[test]
fn fence_requests_are_answered_with_the_request_bit_cleared() {
    let mut machine = ready(4, 4);
    let mut bytes = vec![248, 0, 0, 0];
    bytes.extend_from_slice(&0x8000_0003u32.to_be_bytes());
    bytes.push(2);
    bytes.extend_from_slice(b"ab");
    // A fence that is not a request gets no answer.
    bytes.extend_from_slice(&[248, 0, 0, 0, 0, 0, 0, 1, 0]);
    feed_chunks(&mut machine, &bytes, 1).unwrap();
    assert_eq!(
        machine.take_out(),
        [248, 0, 0, 0, 0, 0, 0, 3, 2, b'a', b'b']
    );
    let mut oversized = vec![248, 0, 0, 0, 0, 0, 0, 0, 200];
    oversized.extend_from_slice(&[0u8; 200]);
    assert!(machine.feed(&oversized).is_err());
}

#[test]
fn bell_and_colour_map_are_skipped() {
    let mut machine = ready(4, 4);
    let mut bytes = vec![2];
    bytes.extend_from_slice(&[1, 0, 0, 0, 0, 2]);
    bytes.extend_from_slice(&[0u8; 12]);
    bytes.push(2);
    feed_chunks(&mut machine, &bytes, 2).unwrap();
    assert!(machine.take_events().is_empty());
    // Still in sync: a raw update works.
    let mut bytes = update_header(1);
    bytes.extend_from_slice(&rect_header(0, 0, 1, 1, 0));
    bytes.extend_from_slice(&[9, 9, 9, 0]);
    machine.feed(&bytes).unwrap();
    assert_eq!(pixel(&machine, 0, 0), [9, 9, 9, 255]);
}

#[test]
fn legacy_cut_text_is_latin1_and_oversized_text_is_skipped() {
    let mut machine = ready(4, 4);
    let mut bytes = vec![3, 0, 0, 0];
    bytes.extend_from_slice(&be32(5));
    bytes.extend_from_slice(&[b'h', 0xe9, b'\r', b'\n', b'!']);
    feed_chunks(&mut machine, &bytes, 3).unwrap();
    assert_eq!(
        machine.take_events(),
        [Event::Clipboard("h\u{e9}\n!".to_owned())]
    );

    // An oversized text is skipped without breaking the stream.
    let size = MAX_CLIPBOARD + 1;
    let mut machine = ready(4, 4);
    let mut bytes = vec![3, 0, 0, 0];
    bytes.extend_from_slice(&be32(size));
    bytes.extend_from_slice(&vec![b'x'; size]);
    bytes.push(2);
    let mut update = update_header(1);
    update.extend_from_slice(&rect_header(0, 0, 1, 1, 0));
    update.extend_from_slice(&[5, 5, 5, 0]);
    bytes.extend_from_slice(&update);
    feed_chunks(&mut machine, &bytes, 100_000).unwrap();
    assert_eq!(pixel(&machine, 0, 0), [5, 5, 5, 255]);
    assert!(
        machine
            .take_events()
            .iter()
            .all(|event| !matches!(event, Event::Clipboard(_)))
    );
}

fn server_ext(mut message: Vec<u8>) -> Vec<u8> {
    message[0] = 3;
    message
}

#[test]
fn extended_clipboard_flow() {
    let mut machine = ready(4, 4);
    // Capabilities: answered with ours.
    let mut caps = Vec::new();
    clipboard::caps(&mut caps);
    let mut server_caps = server_ext(caps.clone());
    // The server accepts everything.
    server_caps[8..12].copy_from_slice(&0x1f00_0001u32.to_be_bytes());
    feed_chunks(&mut machine, &server_caps, 1).unwrap();
    assert_eq!(machine.take_out(), caps);
    assert_eq!(machine.take_events(), [Event::ExtClipboard(true)]);

    // A server that cannot take Notify is not usable for the flow.
    let mut limited = server_caps.clone();
    limited[8..12].copy_from_slice(&(clipboard::CAPS | clipboard::FORMAT_TEXT).to_be_bytes());
    machine.feed(&limited).unwrap();
    machine.take_out();
    assert_eq!(machine.take_events(), [Event::ExtClipboard(false)]);

    // Notify with text: we request it.
    let mut notify = Vec::new();
    clipboard::notify(&mut notify, true);
    machine.feed(&server_ext(notify)).unwrap();
    let mut request = Vec::new();
    clipboard::request(&mut request);
    assert_eq!(machine.take_out(), request);

    // Provide: delivered as text.
    let mut provide = Vec::new();
    assert!(clipboard::provide(&mut provide, "a\nb \u{20ac}"));
    feed_chunks(&mut machine, &server_ext(provide), 5).unwrap();
    assert_eq!(
        machine.take_events(),
        [Event::Clipboard("a\nb \u{20ac}".to_owned())]
    );

    // Request and Peek reach the session.
    machine.feed(&server_ext(request)).unwrap();
    let mut peek = vec![3, 0, 0, 0];
    peek.extend_from_slice(&(-4i32).to_be_bytes());
    peek.extend_from_slice(&clipboard::PEEK.to_be_bytes());
    machine.feed(&peek).unwrap();
    assert_eq!(
        machine.take_events(),
        [Event::ClipboardRequest, Event::ClipboardPeek]
    );
}

#[test]
fn extended_provide_is_consumed_completely_even_when_unusable() {
    let mut machine = ready(4, 4);
    let mut provide = vec![3, 0, 0, 0];
    let garbage = [0x55u8; 40];
    provide.extend_from_slice(&(-(4 + 40i32)).to_be_bytes());
    provide.extend_from_slice(&(clipboard::PROVIDE | clipboard::FORMAT_TEXT).to_be_bytes());
    provide.extend_from_slice(&garbage);
    let mut update = update_header(1);
    update.extend_from_slice(&rect_header(0, 0, 1, 1, 0));
    update.extend_from_slice(&[6, 6, 6, 0]);
    provide.extend_from_slice(&update);
    feed_chunks(&mut machine, &provide, 4).unwrap();
    assert_eq!(pixel(&machine, 0, 0), [6, 6, 6, 255]);
    assert!(
        machine
            .take_events()
            .iter()
            .all(|event| !matches!(event, Event::Clipboard(_)))
    );
}

fn failing(machine: &mut Machine, bytes: &[u8]) -> String {
    machine.feed(bytes).unwrap_err().to_string()
}

#[test]
fn lengths_from_the_wire_are_bounded() {
    // Rectangle outside of the screen.
    let mut machine = ready(4, 4);
    let mut bytes = update_header(1);
    bytes.extend_from_slice(&rect_header(3, 3, 2, 2, 0));
    assert!(failing(&mut machine, &bytes).contains("outside"));

    // Huge rectangle.
    let mut machine = ready(4, 4);
    let mut bytes = update_header(1);
    bytes.extend_from_slice(&rect_header(0, 0, 65_535, 65_535, 0));
    assert!(machine.feed(&bytes).is_err());

    // Oversized ZRLE and H.264 payloads are refused before they are buffered.
    let mut machine = ready(4, 4);
    let mut bytes = update_header(1);
    bytes.extend_from_slice(&rect_header(0, 0, 4, 4, 16));
    bytes.extend_from_slice(&be32(MAX_ZRLE + 1));
    assert!(failing(&mut machine, &bytes).contains("ZRLE"));

    let mut machine = ready(4, 4);
    let mut bytes = update_header(1);
    bytes.extend_from_slice(&rect_header(0, 0, 4, 4, 50));
    bytes.extend_from_slice(&be32(MAX_H264 + 1));
    bytes.extend_from_slice(&be32(0));
    assert!(failing(&mut machine, &bytes).contains("H.264"));

    // H.264 that was never requested.
    let mut machine = ready(4, 4);
    let mut bytes = update_header(1);
    bytes.extend_from_slice(&rect_header(0, 0, 4, 4, 50));
    bytes.extend_from_slice(&be32(2));
    bytes.extend_from_slice(&be32(0));
    bytes.extend_from_slice(&[0, 0]);
    assert!(failing(&mut machine, &bytes).contains("not requested"));

    // Unknown encoding and unknown message type.
    let mut machine = ready(4, 4);
    let mut bytes = update_header(1);
    bytes.extend_from_slice(&rect_header(0, 0, 1, 1, 7));
    assert!(failing(&mut machine, &bytes).contains("encoding"));
    let mut machine = ready(4, 4);
    assert!(failing(&mut machine, &[200]).contains("200"));

    // A failed machine stays failed.
    assert!(machine.feed(&[2]).is_err());
}

#[test]
fn too_many_unbounded_rectangles_are_refused() {
    let mut machine = ready(4, 4);
    machine.feed(&update_header(0xffff)).unwrap();
    let rect = rect_header(0, 0, 0, 0, 0);
    let mut chunk = Vec::new();
    for _ in 0..4096 {
        chunk.extend_from_slice(&rect);
    }
    let mut failed = false;
    for _ in 0..300 {
        if machine.feed(&chunk).is_err() {
            failed = true;
            break;
        }
    }
    assert!(failed);
}

#[test]
fn last_rect_ends_an_unbounded_update() {
    let mut machine = ready(4, 4);
    let mut bytes = update_header(0xffff);
    bytes.extend_from_slice(&rect_header(0, 0, 1, 1, 0));
    bytes.extend_from_slice(&[1, 1, 1, 0]);
    bytes.extend_from_slice(&rect_header(0, 0, 0, 0, -224));
    machine.feed(&bytes).unwrap();
    assert_eq!(
        machine.take_events(),
        [Event::Update {
            codec: Some(Codec::Raw),
            dirty: true
        }]
    );
}

type Calls = Arc<Mutex<Vec<(Vec<u8>, bool)>>>;

struct FakeDecoder {
    calls: Calls,
}

impl H264Decoder for FakeDecoder {
    fn name(&self) -> String {
        "fake".to_owned()
    }

    fn decode(&mut self, annex_b: &[u8], reset: bool) -> Result<(), DecodeError> {
        self.calls.lock().unwrap().push((annex_b.to_vec(), reset));
        if annex_b.first() == Some(&0xff) {
            return Err(DecodeError("broken".to_owned()));
        }
        Ok(())
    }
}

fn h264_rect(flags: u32, payload: &[u8]) -> Vec<u8> {
    let mut bytes = rect_header(0, 0, 4, 4, 50);
    bytes.extend_from_slice(&be32(payload.len()));
    bytes.extend_from_slice(&flags.to_be_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

#[test]
fn h264_is_advertised_only_with_a_decoder_and_errors_request_a_key_frame() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut machine = Machine::new(Some(Box::new(FakeDecoder {
        calls: Arc::clone(&calls),
    })));
    assert_eq!(machine.h264_name().as_deref(), Some("fake"));
    machine.feed(&hello(4, 4)).unwrap();
    let mut expected = b"RFB 003.008\n\x01\x01".to_vec();
    msg::set_pixel_format(&mut expected);
    msg::set_encodings(&mut expected, &msg::encodings(true));
    msg::update_request(&mut expected, false, 4, 4);
    assert_eq!(machine.take_out(), expected);
    machine.take_events();

    let mut bytes = update_header(1);
    bytes.extend_from_slice(&h264_rect(1, &[0, 0, 0, 1, 0x67]));
    feed_chunks(&mut machine, &bytes, 3).unwrap();
    assert_eq!(machine.take_out(), incremental(4, 4));
    assert_eq!(
        machine.take_events(),
        [Event::Update {
            codec: Some(Codec::H264),
            dirty: false
        }]
    );

    let mut bytes = update_header(1);
    bytes.extend_from_slice(&h264_rect(0, &[0xff, 1]));
    machine.feed(&bytes).unwrap();
    let mut expected = full(4, 4);
    expected.extend_from_slice(&incremental(4, 4));
    assert_eq!(machine.take_out(), expected);

    // A rectangle that only resets the context carries no picture.
    let mut bytes = update_header(1);
    bytes.extend_from_slice(&h264_rect(2, &[]));
    machine.feed(&bytes).unwrap();

    let calls = calls.lock().unwrap();
    assert_eq!(
        *calls,
        [(vec![0, 0, 0, 1, 0x67], true), (vec![0xff, 1], false)]
    );
}
