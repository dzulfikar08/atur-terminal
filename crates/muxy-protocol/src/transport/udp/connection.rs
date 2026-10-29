//! A reliable, ordered byte stream over unreliable datagrams, tuned for links
//! with long, jittery round trips and heavy loss.
//!
//! This is only a state machine. It never reads a clock or a socket: callers
//! pass the time, feed it the packets that arrive, and ask it for the packets
//! to send and when to ask again. That keeps every timer testable on a
//! simulated link.
//!
//! What differs from TCP, and why:
//!
//! - Recovery is driven by time, not by counting duplicate acks. A packet is
//!   lost once a later one was acknowledged and one round trip plus a
//!   reordering allowance has passed, and a probe goes out when nothing has
//!   been acknowledged for a little more than one round trip. After the first
//!   probe, probes repeat every half second instead of backing off
//!   exponentially, so a link that returns after an outage is noticed within
//!   a second.
//! - Round trips are measured from echoed timestamps, so a retransmission
//!   never makes a sample ambiguous and an idle ack is still a sample.
//! - A small packet is sent twice, a few milliseconds apart. Keystrokes, acks
//!   and echoes are tiny, and two independent chances turn a 10% loss rate
//!   into 1% for them without waiting a round trip to find out.
//! - The congestion window never shrinks below a floor, because on a link
//!   that loses packets at random a collapse would starve the terminal for
//!   no benefit.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

/// Data bytes in one packet. With the header and sealing a packet stays
/// under 1200 bytes, which fits a path MTU of 1280 with room for tunnels.
pub(super) const MSS: usize = 1100;
const MAX_SACK: usize = 3;
/// Bytes buffered for sending before `write` pushes back.
const SEND_BUFFER: usize = 1 << 20;
/// Bytes accepted ahead of what the reader consumed.
const RECEIVE_BUFFER: usize = 1 << 20;
/// A packet with at most this many data bytes is sent twice.
const DUPLICATE_BELOW: usize = 256;
const INITIAL_RTT: Duration = Duration::from_millis(500);
const MIN_PTO: Duration = Duration::from_millis(40);
const PROBE_INTERVAL: Duration = Duration::from_millis(500);
const LONG_PROBE_INTERVAL: Duration = Duration::from_secs(1);
const LONG_OUTAGE_PROBES: u32 = 8;
const PERSISTENT_PROBES: u32 = 3;
const HEARTBEAT: Duration = Duration::from_secs(2);
const MIN_WINDOW: u64 = 16 * MSS as u64;
const INITIAL_WINDOW: u64 = 24 * MSS as u64;
const MAX_WINDOW: u64 = 8 << 20;

const FIN: u8 = 1;
const HEARTBEAT_FLAG: u8 = 2;
const ECHO: u8 = 4;
const RESET: u8 = 8;

#[derive(Debug, Eq, PartialEq)]
pub(super) struct Malformed;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Ranges(Vec<(u64, u64)>);

impl Ranges {
    fn insert(&mut self, start: u64, end: u64) {
        if start >= end {
            return;
        }
        let (mut start, mut end) = (start, end);
        let mut merged = Vec::with_capacity(self.0.len() + 1);
        let mut placed = false;
        for &(low, high) in &self.0 {
            if high < start {
                merged.push((low, high));
            } else if low > end {
                if !placed {
                    merged.push((start, end));
                    placed = true;
                }
                merged.push((low, high));
            } else {
                start = start.min(low);
                end = end.max(high);
            }
        }
        if !placed {
            merged.push((start, end));
        }
        self.0 = merged;
    }

    fn remove(&mut self, start: u64, end: u64) {
        if start >= end {
            return;
        }
        let mut kept = Vec::with_capacity(self.0.len() + 1);
        for &(low, high) in &self.0 {
            if high <= start || low >= end {
                kept.push((low, high));
                continue;
            }
            if low < start {
                kept.push((low, start));
            }
            if high > end {
                kept.push((end, high));
            }
        }
        self.0 = kept;
    }

    fn trim_below(&mut self, limit: u64) {
        self.remove(0, limit);
    }

    fn covers(&self, start: u64, end: u64) -> bool {
        start >= end
            || self
                .0
                .iter()
                .any(|&(low, high)| low <= start && end <= high)
    }

    fn first(&self) -> Option<(u64, u64)> {
        self.0.first().copied()
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[derive(Clone, Copy, Debug)]
struct Sent {
    start: u64,
    end: u64,
    at: Instant,
}

#[derive(Clone, Copy, Debug)]
struct Duplicate {
    due: Instant,
    start: u64,
    end: u64,
}

#[derive(Debug)]
struct Rtt {
    smoothed: Option<Duration>,
    variance: Duration,
    minimum: Duration,
}

impl Rtt {
    fn new() -> Self {
        Self {
            smoothed: None,
            variance: INITIAL_RTT / 2,
            minimum: Duration::MAX,
        }
    }

    fn sample(&mut self, rtt: Duration) {
        self.minimum = self.minimum.min(rtt);
        match self.smoothed {
            None => {
                self.smoothed = Some(rtt);
                self.variance = rtt / 2;
            }
            Some(smoothed) => {
                let deviation = smoothed.abs_diff(rtt);
                self.variance = (self.variance * 3 + deviation) / 4;
                self.smoothed = Some((smoothed * 7 + rtt) / 8);
            }
        }
    }

    fn get(&self) -> Duration {
        self.smoothed.unwrap_or(INITIAL_RTT)
    }

    /// How far out of order a packet may arrive before it counts as lost.
    fn reordering(&self) -> Duration {
        let srtt = self.get();
        self.variance
            .min(srtt / 2)
            .max(srtt / 8)
            .max(Duration::from_millis(4))
    }

    fn probe_timeout(&self) -> Duration {
        (self.get()
            + (self.variance * 2).max(Duration::from_millis(10))
            + Duration::from_millis(10))
        .max(MIN_PTO)
    }
}

/// Counters for diagnostics and tests.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Stats {
    pub packets_sent: u64,
    pub packets_received: u64,
    pub retransmitted_packets: u64,
    pub duplicated_packets: u64,
    pub probes: u64,
}

#[derive(Debug)]
pub(super) struct Connection {
    epoch: Instant,

    // Sending. `buffer` holds data bytes `una..data_end`; the FIN, once
    // queued, takes the sequence number `data_end`.
    una: u64,
    nxt: u64,
    data_end: u64,
    buffer: VecDeque<u8>,
    fin_queued: bool,
    sacked: Ranges,
    lost: Ranges,
    sent: VecDeque<Sent>,
    in_flight: u64,
    peer_limit: u64,
    latest_acked: Option<Instant>,
    duplicates: VecDeque<Duplicate>,

    // Congestion control and timers.
    window: u64,
    threshold: u64,
    recovery_end: u64,
    rtt: Rtt,
    probes_unanswered: u32,
    timer_base: Instant,
    paced_until: Instant,
    last_transmit: Instant,

    // Receiving.
    rcv_nxt: u64,
    ready: VecDeque<u8>,
    segments: BTreeMap<u64, Vec<u8>>,
    segment_bytes: usize,
    fin_at: Option<u64>,
    fin_consumed: bool,
    ack_pending: bool,
    last_recent: Option<(u64, u64)>,
    advertised: u64,
    peer_stamp: Option<(u32, Instant)>,
    sampled_stamp: Option<u32>,
    last_heard: Instant,
    reset_pending: u8,
    peer_reset: bool,

    heartbeats: bool,
    stats: Stats,
}

impl Connection {
    /// Only the client heartbeats: that keeps its NAT mapping open, tells the
    /// server where it is, and tells it when it is gone. The server answers.
    pub(super) fn new(now: Instant, heartbeats: bool) -> Self {
        Self {
            epoch: now,
            heartbeats,
            una: 0,
            nxt: 0,
            data_end: 0,
            buffer: VecDeque::new(),
            fin_queued: false,
            sacked: Ranges::default(),
            lost: Ranges::default(),
            sent: VecDeque::new(),
            in_flight: 0,
            peer_limit: u64::try_from(RECEIVE_BUFFER).unwrap_or(u64::MAX),
            latest_acked: None,
            duplicates: VecDeque::new(),
            window: INITIAL_WINDOW,
            threshold: u64::MAX,
            recovery_end: 0,
            rtt: Rtt::new(),
            probes_unanswered: 0,
            timer_base: now,
            paced_until: now,
            last_transmit: now,
            rcv_nxt: 0,
            ready: VecDeque::new(),
            segments: BTreeMap::new(),
            segment_bytes: 0,
            fin_at: None,
            fin_consumed: false,
            ack_pending: false,
            last_recent: None,
            advertised: 0,
            peer_stamp: None,
            sampled_stamp: None,
            last_heard: now,
            reset_pending: 0,
            peer_reset: false,
            stats: Stats::default(),
        }
    }

    pub(super) fn stats(&self) -> Stats {
        self.stats
    }

    pub(super) fn smoothed_rtt(&self) -> Option<Duration> {
        self.rtt.smoothed
    }

    pub(super) fn last_heard(&self) -> Instant {
        self.last_heard
    }

    /// Tells the peer to drop the connection, in a few packets sent at once.
    pub(super) fn abort(&mut self) {
        self.reset_pending = 3;
    }

    /// The peer aborted the connection.
    pub(super) fn reset_by_peer(&self) -> bool {
        self.peer_reset
    }

    /// A packet that asks the peer to answer, whatever the state, for
    /// finding out whether it is there.
    pub(super) fn hello(&mut self, now: Instant, out: &mut Vec<u8>) {
        self.encode(now, HEARTBEAT_FLAG, self.nxt, self.nxt, out);
        self.stats.packets_sent += 1;
        self.last_transmit = now;
    }

    // ----- application side -----

    /// Queues as much of `data` as the buffer takes.
    pub(super) fn write(&mut self, data: &[u8]) -> usize {
        if self.fin_queued {
            return 0;
        }
        let take = data.len().min(self.writable());
        self.buffer.extend(&data[..take]);
        self.data_end += as_u64(take);
        take
    }

    pub(super) fn writable(&self) -> usize {
        SEND_BUFFER.saturating_sub(self.buffer.len())
    }

    /// Ends the stream after the queued bytes.
    pub(super) fn finish(&mut self) {
        self.fin_queued = true;
    }

    pub(super) fn read(&mut self, out: &mut [u8]) -> usize {
        let take = out.len().min(self.ready.len());
        for (slot, byte) in out.iter_mut().zip(self.ready.drain(..take)) {
            *slot = byte;
        }
        if take > 0 && self.window_edge_growth() >= as_u64(RECEIVE_BUFFER / 4) {
            self.ack_pending = true;
        }
        take
    }

    /// The peer ended its side and everything it sent was read.
    pub(super) fn eof(&self) -> bool {
        self.fin_consumed && self.ready.is_empty()
    }

    /// Our FIN was acknowledged and so was nothing less.
    pub(super) fn fin_acknowledged(&self) -> bool {
        self.fin_queued && self.una > self.data_end
    }

    pub(super) fn unsent_or_unacked(&self) -> bool {
        self.una < self.stream_end()
    }

    fn stream_end(&self) -> u64 {
        self.data_end + u64::from(self.fin_queued)
    }

    fn free_receive(&self) -> u64 {
        as_u64(RECEIVE_BUFFER.saturating_sub(self.ready.len() + self.segment_bytes))
    }

    fn window_edge_growth(&self) -> u64 {
        (self.rcv_nxt + self.free_receive()).saturating_sub(self.advertised)
    }

    // ----- receiving -----

    pub(super) fn on_packet(&mut self, now: Instant, plain: &[u8]) -> Result<(), Malformed> {
        let packet = Packet::parse(plain).ok_or(Malformed)?;
        self.stats.packets_received += 1;
        self.last_heard = now;

        if packet.flags & ECHO != 0 {
            self.sample_rtt(now, packet.echo, packet.hold);
        }
        if self
            .peer_stamp
            .is_none_or(|(stamp, _)| packet.stamp.wrapping_sub(stamp) < 1 << 31)
        {
            self.peer_stamp = Some((packet.stamp, now));
        }

        if packet.flags & RESET != 0 {
            self.peer_reset = true;
            return Ok(());
        }
        self.on_ack(now, &packet);

        let data_len = as_u64(packet.data.len());
        let fin = packet.flags & FIN != 0;
        if packet.flags & HEARTBEAT_FLAG != 0 || data_len > 0 || fin {
            self.ack_pending = true;
        }
        if data_len > 0 || fin {
            self.receive_data(packet.seq, packet.data, fin);
        }
        Ok(())
    }

    fn sample_rtt(&mut self, now: Instant, echo: u32, hold: u16) {
        if self.sampled_stamp == Some(echo) {
            return;
        }
        self.sampled_stamp = Some(echo);
        let elapsed = self
            .stamp(now)
            .wrapping_sub(echo)
            .wrapping_sub(u32::from(hold));
        if (1..60_000).contains(&elapsed) {
            self.rtt.sample(Duration::from_millis(u64::from(elapsed)));
        }
    }

    fn stamp(&self, now: Instant) -> u32 {
        // Wraps after 49 days, which differences handle.
        u32::try_from(now.saturating_duration_since(self.epoch).as_millis() & 0xffff_ffff)
            .unwrap_or(0)
    }

    fn receive_data(&mut self, seq: u64, data: &[u8], fin: bool) {
        let end = seq + as_u64(data.len());
        if fin {
            self.fin_at = Some(end);
        }
        if end > self.rcv_nxt && !data.is_empty() {
            let edge = self.rcv_nxt + self.free_receive();
            if end <= edge {
                if seq <= self.rcv_nxt {
                    let skip = len(self.rcv_nxt - seq);
                    self.ready.extend(&data[skip..]);
                    self.rcv_nxt = end;
                    self.last_recent = None;
                    self.drain_segments();
                } else if self
                    .segments
                    .get(&seq)
                    .is_none_or(|existing| existing.len() < data.len())
                {
                    self.segment_bytes += data.len();
                    if let Some(old) = self.segments.insert(seq, data.to_vec()) {
                        self.segment_bytes -= old.len();
                    }
                    self.last_recent = Some((seq, end));
                }
            }
        }
        if self.fin_at == Some(self.rcv_nxt) && !self.fin_consumed {
            self.fin_consumed = true;
            self.rcv_nxt += 1;
        }
    }

    fn drain_segments(&mut self) {
        while let Some((&seq, _)) = self.segments.first_key_value() {
            if seq > self.rcv_nxt {
                break;
            }
            let Some(segment) = self.segments.remove(&seq) else {
                break;
            };
            self.segment_bytes -= segment.len();
            let end = seq + as_u64(segment.len());
            if end > self.rcv_nxt {
                let skip = len(self.rcv_nxt - seq);
                self.ready.extend(&segment[skip..]);
                self.rcv_nxt = end;
            }
        }
    }

    /// Up to `MAX_SACK` blocks received beyond the cumulative ack, the most
    /// recent first.
    fn sack_blocks(&self) -> Vec<(u64, u64)> {
        let mut merged: Vec<(u64, u64)> = Vec::new();
        for (&seq, segment) in &self.segments {
            let end = seq + as_u64(segment.len());
            match merged.last_mut() {
                Some(last) if seq <= last.1 => last.1 = last.1.max(end),
                _ => merged.push((seq, end)),
            }
        }
        if let Some((start, _)) = self.last_recent
            && let Some(position) = merged
                .iter()
                .position(|&(low, high)| low <= start && start < high)
        {
            let recent = merged.remove(position);
            merged.insert(0, recent);
        }
        merged.truncate(MAX_SACK);
        merged
    }

    // ----- acknowledgements and loss -----

    fn on_ack(&mut self, now: Instant, packet: &Packet<'_>) {
        let ack = packet.ack.min(self.nxt);
        let mut progressed = false;
        let mut acked_bytes = 0;

        if ack > self.una {
            progressed = true;
            let advance = ack - self.una;
            acked_bytes += advance;
            let data_advance = advance.min(self.data_end.saturating_sub(self.una));
            self.buffer.drain(..len(data_advance));
            self.una = ack;
            self.nxt = self.nxt.max(ack);
            self.sacked.trim_below(ack);
            self.lost.trim_below(ack);
        }
        for &(start, length) in &packet.sacks {
            let (start, stop) = (
                packet.ack.saturating_add(start),
                packet.ack.saturating_add(start).saturating_add(length),
            );
            let (start, stop) = (start.max(self.una), stop.min(self.nxt));
            if start < stop && !self.sacked.covers(start, stop) {
                progressed = true;
                acked_bytes += stop - start;
                self.sacked.insert(start, stop);
                self.lost.remove(start, stop);
            }
        }
        self.peer_limit = self
            .peer_limit
            .max(packet.ack.saturating_add(u64::from(packet.window)));

        // Forget what was delivered.
        let (una, sacked) = (self.una, &self.sacked);
        let mut released = 0;
        let mut latest = self.latest_acked;
        self.sent.retain(|sent| {
            let done = sent.end <= una || sacked.covers(sent.start.max(una), sent.end);
            if done {
                released += sent.end - sent.start;
                latest = Some(latest.map_or(sent.at, |seen| seen.max(sent.at)));
            }
            !done
        });
        self.latest_acked = latest;
        self.in_flight = self.in_flight.saturating_sub(released);
        if self.una >= self.nxt && self.sent.is_empty() {
            self.in_flight = 0;
        }

        if progressed {
            self.probes_unanswered = 0;
            self.timer_base = now;
            self.grow(acked_bytes);
        }
        self.detect_losses(now);
        self.duplicates.retain(|duplicate| {
            duplicate.end > self.una
                && !self
                    .sacked
                    .covers(duplicate.start.max(self.una), duplicate.end)
        });
    }

    fn grow(&mut self, acked: u64) {
        if self.in_flight + acked < self.window / 2 {
            return;
        }
        if self.window < self.threshold {
            self.window += acked;
        } else {
            self.window += as_u64(MSS) * acked / self.window.max(1);
        }
        self.window = self.window.min(MAX_WINDOW);
    }

    fn on_congestion(&mut self, start: u64) {
        if start >= self.recovery_end {
            self.threshold = (self.window * 7 / 10).max(MIN_WINDOW);
            self.window = self.threshold;
            self.recovery_end = self.nxt;
        }
    }

    /// A packet is lost once a later one was acknowledged and its own ack is
    /// overdue by the reordering allowance.
    fn detect_losses(&mut self, now: Instant) {
        let Some(latest) = self.latest_acked else {
            return;
        };
        let overdue = self.rtt.get() + self.rtt.reordering();
        let mut index = 0;
        while index < self.sent.len() {
            let sent = self.sent[index];
            if sent.at <= latest && now >= sent.at + overdue {
                self.sent.remove(index);
                self.in_flight = self.in_flight.saturating_sub(sent.end - sent.start);
                self.mark_lost(sent.start, sent.end);
            } else {
                index += 1;
            }
        }
    }

    fn mark_lost(&mut self, start: u64, end: u64) {
        let start = start.max(self.una);
        if start >= end || self.sacked.covers(start, end) {
            return;
        }
        // Still outstanding in a later packet? Then it is not lost.
        if self
            .sent
            .iter()
            .any(|other| other.start <= start && end <= other.end)
        {
            return;
        }
        self.lost.insert(start, end);
        for &(low, high) in &self.sacked.0 {
            self.lost.remove(low, high);
        }
        self.on_congestion(start);
    }

    fn mark_all_lost(&mut self) {
        let outstanding: Vec<Sent> = self.sent.drain(..).collect();
        self.in_flight = 0;
        for sent in outstanding {
            let start = sent.start.max(self.una);
            if start < sent.end {
                self.lost.insert(start, sent.end);
            }
        }
        for &(low, high) in &self.sacked.0 {
            self.lost.remove(low, high);
        }
        self.threshold = (self.window / 2).max(MIN_WINDOW);
        self.window = MIN_WINDOW;
        self.recovery_end = self.nxt;
    }

    // ----- sending -----

    fn probe_interval(&self) -> Duration {
        let base = self.rtt.probe_timeout();
        match self.probes_unanswered {
            0 => base,
            n if n > LONG_OUTAGE_PROBES => LONG_PROBE_INTERVAL,
            n => (base * (1 << n.min(6))).min(PROBE_INTERVAL),
        }
    }

    fn probe_deadline(&self) -> Option<Instant> {
        (self.una < self.nxt).then(|| self.timer_base + self.probe_interval())
    }

    fn heartbeat_deadline(&self) -> Option<Instant> {
        self.heartbeats.then(|| self.last_transmit + HEARTBEAT)
    }

    /// The next packet to send now, or when to ask again.
    fn plan(&mut self, now: Instant) -> Plan {
        self.detect_losses(now);

        if let Some(deadline) = self.probe_deadline()
            && deadline <= now
        {
            return Plan::Now(Pick::Probe);
        }

        let mut wake: Option<Instant> = self.probe_deadline();
        let mut earlier = |time: Instant| {
            wake = Some(wake.map_or(time, |current| current.min(time)));
        };

        if let Some(latest) = self.latest_acked {
            let overdue = self.rtt.get() + self.rtt.reordering();
            if let Some(sent) = self.sent.iter().find(|sent| sent.at <= latest) {
                earlier(sent.at + overdue);
            }
        }

        let paced = now < self.paced_until;
        let can_send = self.in_flight < self.window;
        if let Some((start, end)) = self.lost.first()
            && can_send
        {
            if paced {
                earlier(self.paced_until);
            } else {
                return Plan::Now(Pick::Range(
                    start,
                    end.min(start + as_u64(MSS)),
                    Kind::Retransmit,
                ));
            }
        }
        if let Some(duplicate) = self.duplicates.front() {
            if duplicate.due <= now {
                return Plan::Now(Pick::Range(duplicate.start, duplicate.end, Kind::Duplicate));
            }
            earlier(duplicate.due);
        }
        if self.nxt < self.stream_end()
            && can_send
            && self.nxt < self.peer_limit
            && self.lost.is_empty()
        {
            if paced {
                earlier(self.paced_until);
            } else {
                let limit = self.peer_limit - self.nxt;
                let end = (self.nxt + as_u64(MSS).min(limit)).min(self.stream_end());
                return Plan::Now(Pick::Range(self.nxt, end, Kind::New));
            }
        }

        if self.ack_pending {
            return Plan::Now(Pick::Ack);
        }
        if let Some(heartbeat) = self.heartbeat_deadline() {
            if heartbeat <= now {
                return Plan::Now(Pick::Heartbeat);
            }
            earlier(heartbeat);
        }
        match wake {
            Some(time) => Plan::At(time),
            None => Plan::Idle,
        }
    }

    pub(super) fn next_wakeup(&mut self, now: Instant) -> Option<Instant> {
        if self.reset_pending > 0 {
            return Some(now);
        }
        match self.plan(now) {
            Plan::Now(_) => Some(now),
            Plan::At(time) => Some(time),
            Plan::Idle => None,
        }
    }

    /// Writes the next packet's bytes to `out`; false when there is none to
    /// send yet.
    pub(super) fn poll_transmit(&mut self, now: Instant, out: &mut Vec<u8>) -> bool {
        if self.reset_pending > 0 {
            self.reset_pending -= 1;
            self.encode(now, RESET, self.nxt, self.nxt, out);
            self.stats.packets_sent += 1;
            return true;
        }
        let Plan::Now(pick) = self.plan(now) else {
            return false;
        };
        let mut flags = 0;
        let (start, end) = match pick {
            Pick::Ack => (self.nxt, self.nxt),
            Pick::Heartbeat => {
                flags |= HEARTBEAT_FLAG;
                (self.nxt, self.nxt)
            }
            Pick::Probe => {
                self.stats.probes += 1;
                self.probes_unanswered += 1;
                self.timer_base = now;
                if self.probes_unanswered == PERSISTENT_PROBES {
                    self.mark_all_lost();
                }
                match self.probe_range() {
                    Some((start, end)) => {
                        self.lost.remove(start, end);
                        (start, end)
                    }
                    None => {
                        flags |= HEARTBEAT_FLAG;
                        (self.nxt, self.nxt)
                    }
                }
            }
            Pick::Range(start, end, kind) => {
                match kind {
                    Kind::New => {
                        if end - start <= as_u64(DUPLICATE_BELOW) && !self.fin_queued {
                            let gap = (self.rtt.get() / 16)
                                .clamp(Duration::from_millis(5), Duration::from_millis(40));
                            self.duplicates.push_back(Duplicate {
                                due: now + gap,
                                start,
                                end,
                            });
                        }
                    }
                    Kind::Retransmit => {
                        self.stats.retransmitted_packets += 1;
                        self.lost.remove(start, end);
                    }
                    Kind::Duplicate => {
                        self.stats.duplicated_packets += 1;
                        self.duplicates.pop_front();
                    }
                }
                (start, end)
            }
        };
        let carries_data = end > start;
        self.encode(now, flags, start, end, out);
        self.stats.packets_sent += 1;
        self.last_transmit = now;
        self.ack_pending = false;
        if carries_data {
            let length = end - start;
            self.sent.push_back(Sent {
                start,
                end,
                at: now,
            });
            self.in_flight += length;
            self.nxt = self.nxt.max(end);
            if !matches!(pick, Pick::Probe) {
                self.timer_base = now;
            }
            self.pace(now, length);
        }
        true
    }

    /// The oldest bytes still unacknowledged, as one packet.
    fn probe_range(&self) -> Option<(u64, u64)> {
        let mut start = self.una;
        for &(low, high) in &self.sacked.0 {
            if low <= start {
                start = start.max(high);
            }
        }
        (start < self.nxt).then(|| {
            let mut end = (start + as_u64(MSS)).min(self.nxt);
            if let Some(&(low, _)) = self.sacked.0.iter().find(|&&(low, _)| low > start) {
                end = end.min(low);
            }
            (start, end)
        })
    }

    fn pace(&mut self, now: Instant, length: u64) {
        let srtt = self.rtt.get().as_secs_f64().max(0.001);
        // Twice the window per round trip: enough to fill the pipe while
        // sparing a router's queue a whole window at once.
        let rate = 2.0 * (self.window as f64) / srtt;
        let gap = Duration::from_secs_f64((length as f64 + 60.0) / rate);
        let floor = now.checked_sub(gap * 4).unwrap_or(now);
        self.paced_until = self.paced_until.max(floor) + gap;
    }

    fn encode(&mut self, now: Instant, mut flags: u8, start: u64, end: u64, out: &mut Vec<u8>) {
        let data_start = start.min(self.data_end);
        let data_stop = end.min(self.data_end);
        if self.fin_queued && end > self.data_end {
            flags |= FIN;
        }
        let echo = self.peer_stamp.and_then(|(stamp, received)| {
            let hold = now.saturating_duration_since(received);
            (hold < Duration::from_secs(5))
                .then(|| (stamp, u16::try_from(hold.as_millis()).unwrap_or(u16::MAX)))
        });
        if echo.is_some() {
            flags |= ECHO;
        }
        let (echo_stamp, hold) = echo.unwrap_or((0, 0));
        out.push(flags);
        out.extend_from_slice(&self.stamp(now).to_be_bytes());
        out.extend_from_slice(&echo_stamp.to_be_bytes());
        out.extend_from_slice(&hold.to_be_bytes());
        out.extend_from_slice(&self.rcv_nxt.to_be_bytes());
        let window = self.free_receive();
        self.advertised = self.advertised.max(self.rcv_nxt + window);
        out.extend_from_slice(&u32::try_from(window).unwrap_or(u32::MAX).to_be_bytes());
        let blocks = self.sack_blocks();
        out.push(u8::try_from(blocks.len()).unwrap_or(0));
        for (low, high) in blocks {
            let offset = u32::try_from(low.saturating_sub(self.rcv_nxt)).unwrap_or(u32::MAX);
            let length = u32::try_from(high - low).unwrap_or(u32::MAX);
            out.extend_from_slice(&offset.to_be_bytes());
            out.extend_from_slice(&length.to_be_bytes());
        }
        out.extend_from_slice(&start.to_be_bytes());
        if data_stop > data_start {
            let from = len(data_start - self.una);
            let to = len(data_stop - self.una);
            let (front, back) = self.buffer.as_slices();
            if to <= front.len() {
                out.extend_from_slice(&front[from..to]);
            } else if from >= front.len() {
                out.extend_from_slice(&back[from - front.len()..to - front.len()]);
            } else {
                out.extend_from_slice(&front[from..]);
                out.extend_from_slice(&back[..to - front.len()]);
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    New,
    Retransmit,
    Duplicate,
}

#[derive(Clone, Copy, Debug)]
enum Pick {
    Range(u64, u64, Kind),
    Probe,
    Ack,
    Heartbeat,
}

#[derive(Clone, Copy, Debug)]
enum Plan {
    Now(Pick),
    At(Instant),
    Idle,
}

struct Packet<'a> {
    flags: u8,
    stamp: u32,
    echo: u32,
    hold: u16,
    ack: u64,
    window: u32,
    sacks: Vec<(u64, u64)>,
    seq: u64,
    data: &'a [u8],
}

impl<'a> Packet<'a> {
    fn parse(bytes: &'a [u8]) -> Option<Self> {
        let mut reader = Reader(bytes);
        let flags = reader.u8()?;
        let stamp = reader.u32()?;
        let echo = reader.u32()?;
        let hold = reader.u16()?;
        let ack = reader.u64()?;
        let window = reader.u32()?;
        let count = usize::from(reader.u8()?);
        if count > MAX_SACK {
            return None;
        }
        let mut sacks = Vec::with_capacity(count);
        for _ in 0..count {
            sacks.push((u64::from(reader.u32()?), u64::from(reader.u32()?)));
        }
        let seq = reader.u64()?;
        Some(Self {
            flags,
            stamp,
            echo,
            hold,
            ack,
            window,
            sacks,
            seq,
            data: reader.0,
        })
    }
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take(&mut self, count: usize) -> Option<&[u8]> {
        let (head, tail) = self.0.split_at_checked(count)?;
        self.0 = tail;
        Some(head)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|bytes| bytes[0])
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_be_bytes(self.take(2)?.try_into().ok()?))
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }
}

fn as_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn len(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests;
