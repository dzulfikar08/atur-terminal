//! How many bytes of a screen stream may be on their way to a client that has not yet
//! reported them handled.
//!
//! The capture program (`wayvnc`) paces itself by how fast the relay takes its bytes:
//! while the relay waits, its socket fills and it sends only the newest picture. So this
//! window is the whole flow control of a screen, and two things pull it apart:
//!
//! - A long link needs a lot in flight to stay busy: 28 Mbit/s at a round trip of 150 ms
//!   is 525 KiB.
//! - A slow link must not be given more than it can carry: every byte beyond what fills
//!   the pipe waits in a queue, and the picture shown is that much older. 512 KiB on a
//!   4 Mbit/s link is a second of delay.
//!
//! So the window follows the link, in the manner of a bottleneck-bandwidth estimate: the
//! highest rate at which the client reported bytes handled over the last ten seconds,
//! times one and a half round trips (the shortest seen) plus a tenth of a second: room for
//! a burst, such as a key frame, to pass without waiting for an acknowledgement, and for
//! the acknowledgements' own delay, but on a link that is the limit never more than a
//! round trip's half and a tenth of a second of queue.
//! Before the first measurement it is [`START`], which is what a fixed window gave.
//! Pauses in the stream do not lower the estimate: only a higher sample, or ten seconds,
//! replace it.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// The window before the link has been measured.
pub(super) const START: usize = 512 * 1024;
/// Never less than this, so a key frame is not held up for long after a quiet spell.
const FLOOR: usize = 160 * 1024;
/// Never more than this: the UDP stream accepts a mebibyte ahead of its reader.
const CEILING: usize = 1024 * 1024;
/// The window, in round trips of the link's rate; the link itself holds one.
const ROUND_TRIPS: f64 = 1.5;
/// More time of the link's rate the window holds.
const QUEUE: Duration = Duration::from_millis(100);
/// How long a measurement is believed.
const MEMORY: Duration = Duration::from_secs(10);
/// The shortest span a rate is measured over.
const SHORTEST_SPAN: Duration = Duration::from_millis(100);

#[derive(Debug, Default)]
pub(super) struct ScreenWindow {
    /// Bytes sent so far when each chunk went out, and when, until reported.
    sent: VecDeque<(u64, Instant)>,
    reported: u64,
    /// The shortest time from sending to the client's report, and when it was seen.
    round_trip: Option<(Duration, Instant)>,
    /// Where the span being measured began, and what was reported within it.
    span: Option<Instant>,
    span_bytes: u64,
    /// The highest rate seen, in bytes a second, and when.
    rate: Option<(f64, Instant)>,
}

impl ScreenWindow {
    /// A chunk went out; `total` is the number of bytes sent up to and with it.
    pub(super) fn sent(&mut self, total: u64, now: Instant) {
        if self.sent.is_empty() {
            // Time with nothing outstanding is not part of any rate.
            self.span = Some(now);
            self.span_bytes = 0;
        }
        self.sent.push_back((total, now));
    }

    /// The client reported `total` bytes handled in all.
    pub(super) fn reported(&mut self, total: u64, now: Instant) {
        if total <= self.reported {
            return;
        }
        self.span_bytes += total - self.reported;
        self.reported = total;
        let mut last = None;
        while let Some(&(end, at)) = self.sent.front() {
            if end > total {
                break;
            }
            last = Some(at);
            self.sent.pop_front();
        }
        if let Some(at) = last {
            let sample = now.saturating_duration_since(at);
            if self
                .round_trip
                .is_none_or(|(shortest, seen)| sample <= shortest || now - seen > MEMORY)
            {
                self.round_trip = Some((sample, now));
            }
        }
        let Some(start) = self.span else { return };
        let elapsed = now.saturating_duration_since(start);
        let shortest = self.round_trip.map_or(Duration::ZERO, |(rtt, _)| rtt);
        if elapsed >= shortest.max(SHORTEST_SPAN) {
            #[allow(
                clippy::cast_precision_loss,
                reason = "byte counts far below where f64 loses precision"
            )]
            let sample = self.span_bytes as f64 / elapsed.as_secs_f64();
            if self
                .rate
                .is_none_or(|(highest, seen)| sample >= highest || now - seen > MEMORY)
            {
                self.rate = Some((sample, now));
            }
            self.span = Some(now);
            self.span_bytes = 0;
        }
    }

    /// The most bytes that may be queued or outstanding.
    pub(super) fn limit(&self) -> usize {
        let (Some((rate, _)), Some((round_trip, _))) = (self.rate, self.round_trip) else {
            return START;
        };
        let window = rate * (ROUND_TRIPS * round_trip.as_secs_f64() + QUEUE.as_secs_f64());
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "clamped to the floor and ceiling right after"
        )]
        let window = window as usize;
        window.clamp(FLOOR, CEILING)
    }
}

#[cfg(test)]
#[allow(
    clippy::cast_precision_loss,
    reason = "a simulation over small byte counts"
)]
mod tests {
    use super::*;

    const CHUNK: u64 = 16 * 1024;

    /// A link carrying `bytes_per_second` at most, with `rtt` as its round trip, fed by a
    /// sender that offers `offered` bytes a second, for `seconds`.
    fn run(bytes_per_second: f64, rtt: Duration, offered: f64, seconds: u64) -> ScreenWindow {
        let start = Instant::now();
        let mut window = ScreenWindow::default();
        let step = Duration::from_millis(5);
        let mut sent_total = 0_u64;
        let mut reports: VecDeque<(Instant, u64)> = VecDeque::new();
        let mut link_free = start;
        let mut now = start;
        let mut owed = 0.0_f64;
        while now < start + Duration::from_secs(seconds) {
            owed += offered * step.as_secs_f64();
            while owed >= CHUNK as f64 && (sent_total - window.reported) < window.limit() as u64 {
                owed -= CHUNK as f64;
                sent_total += CHUNK;
                window.sent(sent_total, now);
                // Through the bottleneck, then the rest of the way, then the report back.
                link_free =
                    link_free.max(now) + Duration::from_secs_f64(CHUNK as f64 / bytes_per_second);
                reports.push_back((link_free + rtt, sent_total));
            }
            while reports.front().is_some_and(|(at, _)| *at <= now) {
                if let Some((_, total)) = reports.pop_front() {
                    window.reported(total, now);
                }
            }
            now += step;
        }
        window
    }

    #[test]
    fn it_starts_where_a_fixed_window_was() {
        assert_eq!(ScreenWindow::default().limit(), START);
    }

    #[test]
    fn a_fast_long_link_gets_room_for_what_it_carries() {
        // 2 MB/s (16 Mbit/s) at 150 ms, offered more than it carries: the window opens up
        // to one and a half round trips and a tenth of a second at that rate.
        let window = run(2_000_000.0, Duration::from_millis(150), 4_000_000.0, 12);
        let limit = window.limit();
        assert!(limit >= 550_000, "{limit}");
        assert!(limit <= CEILING, "{limit}");
    }

    #[test]
    fn a_slow_link_is_not_given_a_second_of_queue() {
        // 4 Mbit/s at 150 ms, offered 10 Mbit/s.
        let window = run(500_000.0, Duration::from_millis(150), 1_250_000.0, 20);
        let limit = window.limit();
        assert!(limit >= FLOOR, "{limit}");
        // Well under the 512 KiB, a second of queue at this rate, of a fixed window.
        assert!(limit <= 300_000, "{limit}");
    }

    #[test]
    fn a_quiet_stream_keeps_what_it_learned() {
        let mut window = run(2_000_000.0, Duration::from_millis(150), 4_000_000.0, 12);
        let learned = window.limit();
        // A trickle afterwards, far below the link's capacity.
        let now = Instant::now() + Duration::from_secs(20);
        window.sent(window.reported + CHUNK, now);
        window.reported(window.reported + CHUNK, now + Duration::from_millis(160));
        assert!(window.limit() >= FLOOR);
        assert!(learned >= 550_000, "{learned}");
    }

    #[test]
    fn the_window_stays_between_the_floor_and_the_ceiling() {
        let mut window = ScreenWindow::default();
        let start = Instant::now();
        window.sent(CHUNK, start);
        window.reported(CHUNK, start + Duration::from_millis(500));
        assert!(window.limit() >= FLOOR);
        let mut window = ScreenWindow::default();
        window.sent(100 * CHUNK, start);
        window.reported(100 * CHUNK, start + Duration::from_millis(1));
        window.sent(200 * CHUNK, start + Duration::from_millis(2));
        window.reported(200 * CHUNK, start + Duration::from_millis(300));
        assert!(window.limit() <= CEILING);
    }
}
