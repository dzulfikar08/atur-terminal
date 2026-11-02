//! A moving test pattern in place of the screen, for tests: it needs no
//! permission and no display, but goes through the same encoder and server.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::frame::Frame;
use crate::serve::Broadcast;

/// Pictures a second.
const RATE: u64 = 30;
/// Pictures each background colour lasts.
const HOLD: u64 = 15;
const SQUARE: usize = 48;
/// B, G, R of the colours the background cycles through.
pub const COLOURS: [[u8; 3]; 3] = [[0, 0, 255], [0, 255, 0], [255, 0, 0]];

/// The test picture number `index`: a solid background, a white square that
/// travels across the middle, and a black border on the first pixel row.
#[must_use]
pub fn picture(width: u32, height: u32, index: u64) -> Frame {
    let columns = usize::try_from(width).unwrap_or(0);
    let rows = usize::try_from(height).unwrap_or(0);
    let colour = COLOURS[usize::try_from(index / HOLD % 3).unwrap_or(0)];
    let travel = columns.saturating_sub(SQUARE).max(1);
    let left = usize::try_from(index * 7).unwrap_or(0) % travel;
    let top = rows.saturating_sub(SQUARE) / 2;
    let mut bytes = Vec::with_capacity(columns * rows * 4);
    for y in 0..rows {
        for x in 0..columns {
            let inside = (left..left + SQUARE).contains(&x) && (top..top + SQUARE).contains(&y);
            let [b, g, r] = if y == 0 {
                [0, 0, 0]
            } else if inside {
                [255, 255, 255]
            } else {
                colour
            };
            bytes.extend_from_slice(&[b, g, r, 255]);
        }
    }
    Frame::cpu(width, height, columns * 4, bytes)
}

/// Publishes test pictures of `size` into `broadcast` until `stop` is set.
pub fn run(broadcast: &Arc<Broadcast>, size: (u32, u32), stop: &Arc<AtomicBool>) {
    let start = Instant::now();
    let mut index = 0_u64;
    while !stop.load(Ordering::Acquire) {
        broadcast.publish(Arc::new(picture(size.0, size.1, index)));
        index += 1;
        let due = start + Duration::from_millis(index * 1000 / RATE);
        thread::sleep(due.saturating_duration_since(Instant::now()));
    }
}
