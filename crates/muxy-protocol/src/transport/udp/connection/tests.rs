//! The state machine on a simulated link: a virtual clock, one-way delay with
//! jitter, random loss, and outages. Runs are deterministic.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use super::*;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[derive(Clone, Copy)]
struct Link {
    /// One-way delay.
    delay: Duration,
    jitter: Duration,
    loss: f64,
}

struct Flight {
    at: Instant,
    order: u64,
    to: usize,
    bytes: Vec<u8>,
}

impl PartialEq for Flight {
    fn eq(&self, other: &Self) -> bool {
        (self.at, self.order) == (other.at, other.order)
    }
}
impl Eq for Flight {}
impl PartialOrd for Flight {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Flight {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.at, self.order).cmp(&(other.at, other.order))
    }
}

struct Sim {
    now: Instant,
    start: Instant,
    ends: [Connection; 2],
    link: Link,
    rng: Rng,
    flights: BinaryHeap<Reverse<Flight>>,
    order: u64,
    /// While set, every packet is lost.
    blackout_until: Option<Instant>,
    sent_packets: u64,
}

impl Sim {
    fn new(link: Link, seed: u64) -> Self {
        let start = Instant::now();
        Self {
            now: start,
            start,
            ends: [Connection::new(start, true), Connection::new(start, false)],
            link,
            rng: Rng(seed),
            flights: BinaryHeap::new(),
            order: 0,
            blackout_until: None,
            sent_packets: 0,
        }
    }

    fn elapsed(&self) -> Duration {
        self.now - self.start
    }

    fn transmit(&mut self) {
        for from in 0..2 {
            let mut buffer = Vec::new();
            while self.ends[from].poll_transmit(self.now, &mut buffer) {
                self.sent_packets += 1;
                assert!(buffer.len() + crypto::OVERHEAD <= 1200, "{}", buffer.len());
                let lost = self.blackout_until.is_some_and(|until| self.now < until)
                    || self.rng.unit() < self.link.loss;
                if !lost {
                    let offset = self.link.jitter.as_secs_f64() * (self.rng.unit() * 2.0 - 1.0);
                    let seconds = (self.link.delay.as_secs_f64() + offset).max(0.0);
                    let at = self.now + Duration::from_secs_f64(seconds);
                    self.order += 1;
                    self.flights.push(Reverse(Flight {
                        at,
                        order: self.order,
                        to: 1 - from,
                        bytes: buffer.clone(),
                    }));
                }
                buffer.clear();
            }
        }
    }

    /// Runs until `done` or the limit; false on the limit.
    fn run(
        &mut self,
        limit: Duration,
        mut done: impl FnMut(&mut [Connection; 2], Duration) -> bool,
    ) -> bool {
        let deadline = self.start + limit;
        loop {
            self.transmit();
            if {
                let elapsed = self.elapsed();
                done(&mut self.ends, elapsed)
            } {
                return true;
            }
            let mut next = self.flights.peek().map(|flight| flight.0.at);
            for end in &mut self.ends {
                if let Some(wake) = end.next_wakeup(self.now) {
                    next = Some(next.map_or(wake, |current| current.min(wake)));
                }
            }
            let Some(next) = next else {
                return false;
            };
            if next > deadline {
                self.now = deadline;
                return {
                    let elapsed = self.elapsed();
                    done(&mut self.ends, elapsed)
                };
            }
            self.now = next.max(self.now);
            while self
                .flights
                .peek()
                .is_some_and(|flight| flight.0.at <= self.now)
            {
                let Some(Reverse(flight)) = self.flights.pop() else {
                    break;
                };
                self.ends[flight.to]
                    .on_packet(self.now, &flight.bytes)
                    .expect("well-formed packet");
            }
            if self.now >= deadline {
                return {
                    let elapsed = self.elapsed();
                    done(&mut self.ends, elapsed)
                };
            }
        }
    }
}

use super::super::crypto;

fn drain(connection: &mut Connection, into: &mut Vec<u8>) {
    let mut chunk = [0; 4096];
    loop {
        let read = connection.read(&mut chunk);
        if read == 0 {
            break;
        }
        into.extend_from_slice(&chunk[..read]);
    }
}

fn pattern(length: usize) -> Vec<u8> {
    (0..length)
        .map(|index| u8::try_from((index * 31 + index / 251) % 256).unwrap_or(0))
        .collect()
}

fn link(rtt_ms: u64, jitter_ms: u64, loss: f64) -> Link {
    Link {
        delay: Duration::from_millis(rtt_ms / 2),
        jitter: Duration::from_millis(jitter_ms),
        loss,
    }
}

/// Sends `bytes` one way and returns how long it took.
fn transfer(link: Link, bytes: usize, seed: u64) -> Duration {
    let mut sim = Sim::new(link, seed);
    let data = pattern(bytes);
    let mut sent = 0;
    let mut received = Vec::new();
    let finished = sim.run(Duration::from_secs(600), |ends, _| {
        if sent < data.len() {
            sent += ends[0].write(&data[sent..]);
        }
        drain(&mut ends[1], &mut received);
        received.len() == data.len()
    });
    assert!(finished, "stuck at {} of {}", received.len(), data.len());
    assert_eq!(received, data);
    sim.elapsed()
}

#[test]
fn bulk_data_arrives_intact_on_a_clean_link() {
    let took = transfer(link(100, 0, 0.0), 2 << 20, 1);
    assert!(took < Duration::from_secs(20), "{took:?}");
}

#[test]
fn bulk_data_survives_heavy_loss_and_reordering() {
    for (rtt, jitter, loss) in [(300, 100, 0.05), (800, 400, 0.10), (50, 20, 0.2)] {
        let took = transfer(link(rtt, jitter, loss), 256 << 10, 7);
        assert!(took < Duration::from_secs(300), "{rtt}ms {loss}: {took:?}");
    }
}

/// Writes `count` small messages `gap` apart both ways and returns each
/// one-way delivery latency, sorted.
fn small_messages(link: Link, count: usize, gap: Duration, seed: u64) -> Vec<Duration> {
    let mut sim = Sim::new(link, seed);
    let mut written = 0usize;
    let mut next_write = Duration::ZERO;
    let mut received = Vec::new();
    let mut latencies = Vec::new();
    let mut seen = 0usize;
    sim.run(
        gap * u32::try_from(count + 40).unwrap_or(u32::MAX),
        |ends, elapsed| {
            if written < count && elapsed >= next_write {
                // Message `written` is 16 bytes: its index, then padding.
                let mut message = [0u8; 16];
                message[..8].copy_from_slice(&(written as u64).to_be_bytes());
                assert_eq!(ends[0].write(&message), 16);
                latencies.push((elapsed, None::<Duration>));
                written += 1;
                next_write += gap;
            }
            drain(&mut ends[1], &mut received);
            while received.len() >= (seen + 1) * 16 {
                latencies[seen].1 = Some(elapsed - latencies[seen].0);
                seen += 1;
            }
            seen == count
        },
    );
    assert_eq!(seen, count, "every message arrives");
    let mut sorted: Vec<Duration> = latencies
        .into_iter()
        .filter_map(|(_, latency)| latency)
        .collect();
    sorted.sort();
    sorted
}

fn percentile(sorted: &[Duration], fraction: f64) -> Duration {
    sorted[((sorted.len() as f64 * fraction) as usize).min(sorted.len() - 1)]
}

#[test]
fn interactive_latency_stays_near_the_path_delay_at_five_percent_loss() {
    let latencies = small_messages(link(300, 0, 0.05), 400, Duration::from_millis(250), 11);
    let median = percentile(&latencies, 0.5);
    let p90 = percentile(&latencies, 0.9);
    assert!(median < Duration::from_millis(170), "p50 {median:?}");
    assert!(p90 < Duration::from_millis(500), "p90 {p90:?}");
}

#[test]
fn interactive_latency_has_no_multi_second_stall_at_ten_percent_loss_and_heavy_jitter() {
    let latencies = small_messages(link(800, 400, 0.10), 400, Duration::from_millis(300), 5);
    let median = percentile(&latencies, 0.5);
    let worst = *latencies.last().unwrap();
    assert!(median < Duration::from_millis(500), "p50 {median:?}");
    assert!(worst < Duration::from_secs(3), "worst {worst:?}");
}

#[test]
fn a_ten_second_outage_ends_within_two_seconds_of_the_link_returning() {
    for rtt in [100u64, 300, 800] {
        let mut sim = Sim::new(link(rtt, 0, 0.0), 3);
        // Warm up so both ends know the round trip.
        sim.ends[0].write(b"warm");
        let mut received = Vec::new();
        sim.run(Duration::from_secs(5), |ends, _| {
            drain(&mut ends[1], &mut received);
            received == b"warm"
        });
        let outage_start = sim.now;
        sim.blackout_until = Some(outage_start + Duration::from_secs(10));
        sim.ends[0].write(b"during");
        let restored = outage_start + Duration::from_secs(10);
        let mut arrived = None;
        let deadline = sim.elapsed() + Duration::from_secs(30);
        sim.run(deadline, |ends, _| {
            drain(&mut ends[1], &mut received);
            received.ends_with(b"during")
        });
        if received.ends_with(b"during") {
            arrived = Some(sim.now);
        }
        let arrived = arrived.expect("delivered after the outage");
        let wait = arrived.saturating_duration_since(restored);
        assert!(wait < Duration::from_secs(2), "rtt {rtt}ms: {wait:?}");
    }
}

#[test]
fn both_ends_close_cleanly() {
    let mut sim = Sim::new(link(100, 20, 0.1), 9);
    sim.ends[0].write(b"ping");
    sim.ends[1].write(b"pong");
    sim.ends[0].finish();
    sim.ends[1].finish();
    let (mut left, mut right) = (Vec::new(), Vec::new());
    let closed = sim.run(Duration::from_secs(60), |ends, _| {
        drain(&mut ends[0], &mut left);
        drain(&mut ends[1], &mut right);
        ends[0].eof() && ends[1].eof() && ends[0].fin_acknowledged() && ends[1].fin_acknowledged()
    });
    assert!(closed);
    assert_eq!((left, right), (b"pong".to_vec(), b"ping".to_vec()));
}

#[test]
fn small_packets_go_out_twice_and_large_ones_once() {
    let mut sim = Sim::new(link(200, 0, 0.0), 2);
    sim.ends[0].write(&[1; 20]);
    sim.run(Duration::from_secs(1), |_, _| false);
    assert_eq!(sim.ends[0].stats().duplicated_packets, 1);
    let mut sim = Sim::new(link(200, 0, 0.0), 2);
    sim.ends[0].write(&[1; 1000]);
    sim.run(Duration::from_secs(1), |_, _| false);
    assert_eq!(sim.ends[0].stats().duplicated_packets, 0);
}

#[test]
fn an_idle_connection_stays_quiet() {
    let mut sim = Sim::new(link(100, 0, 0.0), 2);
    sim.run(Duration::from_secs(60), |_, _| false);
    assert!(
        sim.sent_packets <= 64,
        "{} packets in a minute",
        sim.sent_packets
    );
}

#[test]
fn a_slow_reader_stops_the_writer_instead_of_growing_memory() {
    let mut sim = Sim::new(link(20, 0, 0.0), 4);
    let data = pattern(8 << 20);
    let mut sent = 0;
    sim.run(Duration::from_secs(20), |ends, _| {
        sent += ends[0].write(&data[sent..]);
        false
    });
    let held = sim.ends[1].ready.len() + sim.ends[1].segment_bytes;
    assert!(held <= RECEIVE_BUFFER, "{held}");
    assert!(sent < data.len(), "the writer was not held back");
}

#[test]
fn malformed_packets_are_rejected() {
    let mut connection = Connection::new(Instant::now(), true);
    assert_eq!(
        connection.on_packet(Instant::now(), &[1, 2, 3]),
        Err(Malformed)
    );
}

#[test]
fn ranges_merge_and_split() {
    let mut ranges = Ranges::default();
    ranges.insert(10, 20);
    ranges.insert(30, 40);
    ranges.insert(20, 30);
    assert_eq!(ranges.0, vec![(10, 40)]);
    ranges.remove(15, 25);
    assert_eq!(ranges.0, vec![(10, 15), (25, 40)]);
    assert!(ranges.covers(26, 39));
    assert!(!ranges.covers(14, 26));
    ranges.trim_below(30);
    assert_eq!(ranges.0, vec![(30, 40)]);
}

#[test]
#[ignore = "prints the latency of the state machine on the lab links"]
fn latency_table() {
    for (rtt, jitter, loss) in [(50, 0, 0.0), (300, 0, 0.05), (800, 400, 0.10)] {
        let latencies =
            small_messages(link(rtt, jitter, loss), 600, Duration::from_millis(250), 21);
        println!(
            "rtt {rtt} jitter {jitter} loss {loss}: one-way p50 {:?} p90 {:?} p99 {:?} max {:?}",
            percentile(&latencies, 0.5),
            percentile(&latencies, 0.9),
            percentile(&latencies, 0.99),
            latencies.last().unwrap()
        );
    }
}
