//! Predictive local echo, after mosh.
//!
//! A [`Predictor`] watches the bytes a client sends and the grid the server answers with.
//! For printable keys, backspace and left/right arrows it guesses what the terminal will look
//! like and offers those guesses as an overlay, so typing feels instant over a slow link. Each
//! server frame then confirms the guesses or throws them away.
//!
//! Guesses are grouped in epochs, as in mosh. An epoch starts at the first key after anything
//! unpredictable (Enter, a control key, an escape sequence, a paste), after a wrong guess, and
//! when the alternate screen toggles. Guesses of an epoch stay hidden until one of them has been
//! confirmed by the server. This is what keeps password prompts (the server never echoes),
//! full-screen programs and vim normal mode from ever showing invented text: the first key
//! there is checked silently, and nothing shows until the server proved it echoes.
//!
//! The predictor owns no clock, socket or screen; callers pass `now` and render
//! [`Predictor::cells`] and [`Predictor::cursor`] over the real grid.

use std::collections::{BTreeMap, HashMap};
use std::str::FromStr;
use std::time::{Duration, Instant};

use muxy_protocol::Style;

use crate::RunGrid;

/// Round trip time above which predictions switch on in [`PredictMode::Auto`].
const RTT_ON: Duration = Duration::from_millis(30);
/// Round trip time below which they switch off again.
const RTT_OFF: Duration = Duration::from_millis(20);
/// A shown prediction still unconfirmed after this long is underlined.
pub const UNDERLINE_AFTER: Duration = Duration::from_millis(250);
/// After a ping answer, input sent before that ping is given this long for its echo to appear.
const SETTLE: Duration = Duration::from_millis(250);
/// Without pings, an unresolved control key stops blocking predictions after this long.
const UNBLOCK_AFTER: Duration = Duration::from_secs(2);
/// A slow confirmation keeps `Auto` predicting for this long even when the RTT looks low.
const SLOW_MEMORY: Duration = Duration::from_secs(10);
/// More printable characters than this in one write is a paste, which is never predicted.
const MAX_TYPED: usize = 8;

/// When predictions are shown.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PredictMode {
    /// While the measured round trip is above ~30 ms or echo was recently slow.
    #[default]
    Auto,
    Always,
    Never,
}

impl PredictMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Always => "always",
            Self::Never => "never",
        }
    }
}

impl FromStr for PredictMode {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        match text.trim().to_ascii_lowercase().as_str() {
            "auto" | "adaptive" => Ok(Self::Auto),
            "always" | "on" => Ok(Self::Always),
            "never" | "off" => Ok(Self::Never),
            other => Err(format!(
                "unknown local echo mode `{other}`; use auto, always or never"
            )),
        }
    }
}

/// Counters of what happened to predictions, for diagnostics and tests.
/// "Visible" means the prediction's epoch was already trusted, so a user would have seen it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PredictStats {
    pub keys: u64,
    pub visible_confirmed: u64,
    pub hidden_confirmed: u64,
    pub visible_wrong: u64,
    pub hidden_wrong: u64,
}

/// One predicted screen cell to draw over the grid.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PredictedCell {
    pub row: u16,
    pub col: u16,
    pub text: String,
    pub style: Style,
    /// Unconfirmed for longer than [`UNDERLINE_AFTER`].
    pub underline: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MidLine {
    /// Text right of the cursor moves along, as readline does.
    Shift,
    /// Text right of the cursor stays, as in a redrawn input box.
    Overwrite,
}

#[derive(Clone, Debug)]
struct Glyph {
    text: String,
    style: Style,
    /// Half of a double-width character.
    wide: bool,
}

impl Glyph {
    fn blank() -> Self {
        Self {
            text: " ".into(),
            style: Style::default(),
            wide: false,
        }
    }

    fn is_blank(&self) -> bool {
        self.text == " "
    }
}

#[derive(Clone, Debug)]
struct Cell {
    text: String,
    /// What the server may still show before it catches up: the grid's own text and
    /// every earlier guess for this cell.
    seen: Vec<String>,
    style: Style,
    sent: Instant,
    epoch: u64,
    settled: Option<Instant>,
    shifted: bool,
}

impl Cell {
    /// The guess equals what was already there, so seeing it proves nothing.
    fn ambiguous(&self) -> bool {
        self.seen.first() == Some(&self.text)
    }
}

type Point = (u16, u16);

#[derive(Clone, Debug)]
struct CursorGuess {
    at: Point,
    seen: Vec<Point>,
    epoch: u64,
    sent: Instant,
    settled: Option<Instant>,
}

/// After an unpredictable key, guesses wait until the server has caught up with it.
#[derive(Clone, Copy, Debug)]
struct Blocked {
    since: Instant,
    settled: Option<Instant>,
}

#[derive(Clone, Debug)]
pub struct Predictor {
    mode: PredictMode,
    cells: BTreeMap<Point, Cell>,
    cursor: Option<CursorGuess>,
    blocked: Option<Blocked>,
    /// Where the line being typed began: backspace and left arrow stay right of it.
    line_start: Option<Point>,
    epoch: u64,
    confirmed_epoch: u64,
    rtt_high: bool,
    slow_until: Option<Instant>,
    alternate: bool,
    mid_line: MidLine,
    stats: PredictStats,
}

impl Default for Predictor {
    fn default() -> Self {
        Self::new(PredictMode::default())
    }
}

enum Key {
    Char(char),
    Backspace,
    Left,
    Right,
    Other,
}

impl Predictor {
    pub fn new(mode: PredictMode) -> Self {
        Self {
            mode,
            cells: BTreeMap::new(),
            cursor: None,
            blocked: None,
            line_start: None,
            epoch: 1,
            confirmed_epoch: 0,
            rtt_high: false,
            slow_until: None,
            alternate: false,
            mid_line: MidLine::Shift,
            stats: PredictStats::default(),
        }
    }

    pub fn mode(&self) -> PredictMode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: PredictMode) {
        self.mode = mode;
        if mode == PredictMode::Never {
            self.reset();
        }
    }

    pub fn stats(&self) -> PredictStats {
        self.stats
    }

    /// Feeds the latest round trip estimate; `Auto` turns on above 30 ms and off below 20 ms.
    pub fn set_rtt(&mut self, rtt: Option<Duration>) {
        match rtt {
            Some(rtt) if rtt > RTT_ON => self.rtt_high = true,
            Some(rtt) if rtt < RTT_OFF => self.rtt_high = false,
            _ => {}
        }
    }

    /// Tells whether the program runs on the alternate screen. Toggling drops guesses, and
    /// the new screen has to confirm one before any show.
    pub fn set_alternate_screen(&mut self, alternate: bool) {
        if self.alternate != alternate {
            self.alternate = alternate;
            self.abandon();
        }
    }

    /// Forgets everything: the grid was replaced, resized or detached.
    pub fn reset(&mut self) {
        self.abandon();
        self.blocked = None;
    }

    fn abandon(&mut self) {
        self.cells.clear();
        self.cursor = None;
        self.epoch += 1;
    }

    fn active(&self, now: Instant) -> bool {
        match self.mode {
            PredictMode::Always => true,
            PredictMode::Never => false,
            PredictMode::Auto => self.rtt_high || self.slow_until.is_some_and(|until| now < until),
        }
    }

    /// Whether a ping answered after the latest input would help resolve something.
    pub fn wants_barrier(&self) -> bool {
        self.mode != PredictMode::Never && (self.pending() || self.blocked.is_some())
    }

    /// Whether anything is guessed and not yet resolved.
    pub fn pending(&self) -> bool {
        !self.cells.is_empty() || self.cursor.is_some()
    }

    /// Cells to draw over the grid right now.
    pub fn cells(&self, now: Instant) -> Vec<PredictedCell> {
        if !self.active(now) {
            return Vec::new();
        }
        self.cells
            .iter()
            .filter(|(_, cell)| cell.epoch <= self.confirmed_epoch)
            .map(|(&(row, col), cell)| PredictedCell {
                row,
                col,
                text: cell.text.clone(),
                style: cell.style,
                underline: now.saturating_duration_since(cell.sent) >= UNDERLINE_AFTER,
            })
            .collect()
    }

    /// Where to draw the cursor instead of the server's, as `(row, col)`.
    pub fn cursor(&self, now: Instant) -> Option<Point> {
        let guess = self.cursor.as_ref()?;
        (self.active(now) && guess.epoch <= self.confirmed_epoch).then_some(guess.at)
    }

    /// When something needs a redraw or a `tick` even if no frame arrives.
    pub fn next_deadline(&self, now: Instant) -> Option<Instant> {
        let mut next: Option<Instant> = None;
        let mut consider = |at: Instant| next = Some(next.map_or(at, |old| old.min(at)));
        let active = self.active(now);
        for cell in self.cells.values() {
            if active && cell.epoch <= self.confirmed_epoch && cell.sent + UNDERLINE_AFTER > now {
                consider(cell.sent + UNDERLINE_AFTER);
            }
            if let Some(settled) = cell.settled {
                consider(settled + SETTLE);
            }
        }
        if let Some(settled) = self.cursor.as_ref().and_then(|guess| guess.settled) {
            consider(settled + SETTLE);
        }
        if let Some(blocked) = self.blocked {
            consider(blocked.settled.map_or(blocked.since + UNBLOCK_AFTER, |at| at + SETTLE));
        }
        next
    }

    /// Records that the server has answered a request sent right after the input stamped
    /// `stamp`, so every frame it had for that input has arrived (plus a short grace).
    pub fn barrier(&mut self, stamp: Instant, now: Instant) {
        for cell in self.cells.values_mut() {
            if cell.sent <= stamp && cell.settled.is_none() {
                cell.settled = Some(now);
            }
        }
        if let Some(guess) = &mut self.cursor
            && guess.sent <= stamp
            && guess.settled.is_none()
        {
            guess.settled = Some(now);
        }
        if let Some(blocked) = &mut self.blocked
            && blocked.since <= stamp
            && blocked.settled.is_none()
        {
            blocked.settled = Some(now);
        }
    }

    /// Drops guesses whose echo never came. Returns whether the overlay changed.
    pub fn tick(&mut self, now: Instant) -> bool {
        let due = |settled: Option<Instant>| settled.is_some_and(|at| now >= at + SETTLE);
        let expired = self.cells.values().any(|cell| due(cell.settled))
            || self.cursor.as_ref().is_some_and(|guess| due(guess.settled));
        if expired {
            self.fail();
        }
        let unblocked = self.unblock(now);
        expired || unblocked
    }

    fn unblock(&mut self, now: Instant) -> bool {
        let ready = self.blocked.is_some_and(|blocked| {
            blocked.settled.map_or(
                now >= blocked.since + UNBLOCK_AFTER,
                |settled| now >= settled + SETTLE,
            )
        });
        if ready && self.cells.is_empty() {
            self.blocked = None;
            return true;
        }
        false
    }

    /// Call with the bytes about to be sent to the terminal, with the grid as last drawn.
    pub fn keys(&mut self, bytes: &[u8], grid: &RunGrid, now: Instant) {
        if self.mode == PredictMode::Never {
            return;
        }
        let keys = parse(bytes);
        let typed = keys.iter().filter(|key| matches!(key, Key::Char(_))).count();
        if typed > MAX_TYPED {
            self.control(now);
            return;
        }
        let mut rows = HashMap::new();
        for key in keys {
            let predicted = match key {
                Key::Char(ch) => self.type_char(ch, grid, &mut rows, now),
                Key::Backspace => self.backspace(grid, &mut rows, now),
                Key::Left => self.left(grid, &mut rows, now),
                Key::Right => self.right(grid, &mut rows, now),
                Key::Other => false,
            };
            if !predicted {
                self.control(now);
            } else {
                self.stats.keys += 1;
            }
        }
    }

    /// Call after the server's `frame` was applied to `grid`; `reset` is the frame's flag.
    pub fn frame(&mut self, grid: &RunGrid, reset: bool, now: Instant) {
        if reset {
            self.abandon();
            return;
        }
        self.unblock(now);
        if !self.pending() {
            return;
        }
        let mut rows: HashMap<u16, Vec<Glyph>> = HashMap::new();
        let trusted = self.confirmed_epoch;
        let (mut wrong, mut slow) = (false, false);
        let mut credit: Option<u64> = None;
        let (mut visible, mut hidden) = (0, 0);
        self.cells.retain(|&(row, col), cell| {
            let glyphs = rows.entry(row).or_insert_with(|| expand(grid, row));
            let text = glyphs.get(usize::from(col)).map_or(" ", |glyph| glyph.text.as_str());
            if text == cell.text {
                if !cell.ambiguous() {
                    credit = credit.max(Some(cell.epoch));
                    slow |= now.saturating_duration_since(cell.sent) > UNDERLINE_AFTER;
                    if cell.epoch <= trusted {
                        visible += 1;
                    } else {
                        hidden += 1;
                    }
                }
                false
            } else {
                wrong |= !cell.seen.iter().any(|seen| seen == text);
                true
            }
        });
        let mut cursor_done = false;
        if !wrong
            && grid.cursor.visible
            && let Some(guess) = &self.cursor
        {
            let at = (grid.cursor.row, grid.cursor.col);
            if at == guess.at {
                if self.cells.is_empty() {
                    cursor_done = true;
                    if credit.is_none() && guess.seen.first() != Some(&guess.at) {
                        credit = Some(guess.epoch);
                        slow |= now.saturating_duration_since(guess.sent) > UNDERLINE_AFTER;
                    }
                }
            } else {
                wrong |= !guess.seen.contains(&at);
            }
        }
        self.stats.visible_confirmed += visible;
        self.stats.hidden_confirmed += hidden;
        if wrong {
            self.fail();
            return;
        }
        if let Some(epoch) = credit {
            self.confirmed_epoch = self.confirmed_epoch.max(epoch);
        }
        if slow {
            self.slow_until = Some(now + SLOW_MEMORY);
        }
        if cursor_done {
            self.cursor = None;
        }
    }

    /// A guess was wrong or its echo never came: drop them all and distrust until one
    /// guess of a new epoch has been confirmed.
    fn fail(&mut self) {
        let trusted = self.confirmed_epoch;
        let seen = u64::try_from(
            self.cells
                .values()
                .filter(|cell| cell.epoch <= trusted)
                .count(),
        )
        .unwrap_or(u64::MAX);
        let all = u64::try_from(self.cells.len()).unwrap_or(u64::MAX);
        let cursor_only = u64::from(self.cells.is_empty() && self.cursor.is_some());
        let cursor_seen = cursor_only
            * u64::from(self.cursor.as_ref().is_some_and(|guess| guess.epoch <= trusted));
        self.stats.visible_wrong += seen + cursor_seen;
        self.stats.hidden_wrong += all - seen + cursor_only - cursor_seen;
        if self.cells.values().any(|cell| cell.shifted) {
            self.mid_line = match self.mid_line {
                MidLine::Shift => MidLine::Overwrite,
                MidLine::Overwrite => MidLine::Shift,
            };
        }
        self.abandon();
    }

    /// An unpredictable key: pending guesses stay until resolved, but nothing new is
    /// anchored until the server has caught up.
    fn control(&mut self, now: Instant) {
        self.epoch += 1;
        self.cursor = None;
        self.line_start = None;
        self.blocked = Some(Blocked {
            since: now,
            settled: None,
        });
    }

    fn origin(&mut self, grid: &RunGrid, now: Instant) -> Option<Point> {
        if let Some(guess) = &self.cursor {
            return Some(guess.at);
        }
        self.unblock(now);
        if self.blocked.is_some() || !grid.cursor.visible {
            return None;
        }
        let at = (grid.cursor.row, grid.cursor.col);
        if at.0 >= grid.size.rows || at.1 >= grid.size.cols {
            return None;
        }
        if !self.line_start.is_some_and(|start| start.0 == at.0 && start.1 <= at.1) {
            self.line_start = Some(at);
        }
        self.cursor = Some(CursorGuess {
            at,
            seen: vec![at],
            epoch: self.epoch,
            sent: now,
            settled: None,
        });
        Some(at)
    }

    fn advance(&mut self, to: Point, now: Instant) {
        if let Some(guess) = &mut self.cursor {
            guess.at = to;
            if !guess.seen.contains(&to) {
                guess.seen.push(to);
            }
            guess.epoch = self.epoch;
            guess.sent = now;
            guess.settled = None;
        }
    }

    /// What is on screen at `point`, guesses included.
    fn shown(&self, grid: &RunGrid, rows: &mut HashMap<u16, Vec<Glyph>>, point: Point) -> Glyph {
        if let Some(cell) = self.cells.get(&point) {
            return Glyph {
                text: cell.text.clone(),
                style: cell.style,
                wide: false,
            };
        }
        grid_glyph(grid, rows, point)
    }

    fn put(&mut self, point: Point, glyph: &Glyph, underlying: &str, now: Instant, shifted: bool) {
        let epoch = self.epoch;
        match self.cells.get_mut(&point) {
            Some(cell) => {
                let previous = std::mem::replace(&mut cell.text, glyph.text.clone());
                if !cell.seen.contains(&previous) {
                    cell.seen.push(previous);
                }
                cell.style = glyph.style;
                cell.sent = now;
                cell.epoch = epoch;
                cell.settled = None;
                cell.shifted |= shifted;
            }
            None => {
                self.cells.insert(
                    point,
                    Cell {
                        text: glyph.text.clone(),
                        seen: vec![underlying.to_owned()],
                        style: glyph.style,
                        sent: now,
                        epoch,
                        settled: None,
                        shifted,
                    },
                );
            }
        }
    }

    /// The glyphs from `col` to the end of `row`, as shown now; `None` if any is wide.
    fn tail(&self, grid: &RunGrid, rows: &mut HashMap<u16, Vec<Glyph>>, row: u16, col: u16) -> Option<Vec<Glyph>> {
        let tail: Vec<Glyph> = (col..grid.size.cols)
            .map(|at| self.shown(grid, rows, (row, at)))
            .collect();
        tail.iter().all(|glyph| !glyph.wide).then_some(tail)
    }

    fn type_char(
        &mut self,
        ch: char,
        grid: &RunGrid,
        rows: &mut HashMap<u16, Vec<Glyph>>,
        now: Instant,
    ) -> bool {
        let Some((row, col)) = self.origin(grid, now) else {
            return false;
        };
        if col + 1 >= grid.size.cols {
            return false;
        }
        let Some(tail) = self.tail(grid, rows, row, col) else {
            return false;
        };
        let typed = Glyph {
            text: ch.to_string(),
            style: tail.first().map(|glyph| glyph.style).unwrap_or_default(),
            wide: false,
        };
        let shift = self.mid_line == MidLine::Shift && tail.iter().any(|glyph| !glyph.is_blank());
        if shift {
            for (offset, glyph) in tail.iter().enumerate().take(tail.len() - 1) {
                let at = (row, col + 1 + u16::try_from(offset).unwrap_or(0));
                let underlying = grid_glyph(grid, rows, at).text;
                self.put(at, glyph, &underlying, now, true);
            }
        }
        let underlying = grid_glyph(grid, rows, (row, col)).text;
        self.put((row, col), &typed, &underlying, now, shift);
        self.advance((row, col + 1), now);
        true
    }

    /// Whether the cell is part of what is being typed, not a prompt or other fixed text:
    /// right of where the line began, or visibly text.
    fn deletable(&self, grid: &RunGrid, rows: &mut HashMap<u16, Vec<Glyph>>, at: Point) -> bool {
        let glyph = self.shown(grid, rows, at);
        if glyph.wide {
            return false;
        }
        let typed = self
            .line_start
            .is_some_and(|start| start.0 == at.0 && start.1 <= at.1);
        typed || !glyph.is_blank()
    }

    fn backspace(
        &mut self,
        grid: &RunGrid,
        rows: &mut HashMap<u16, Vec<Glyph>>,
        now: Instant,
    ) -> bool {
        let Some((row, col)) = self.origin(grid, now) else {
            return false;
        };
        if col == 0 {
            return false;
        }
        if !self.deletable(grid, rows, (row, col - 1)) {
            return false;
        }
        let Some(tail) = self.tail(grid, rows, row, col) else {
            return false;
        };
        let shift = self.mid_line == MidLine::Shift && tail.iter().any(|glyph| !glyph.is_blank());
        let erased = Glyph {
            text: " ".into(),
            style: Style::default(),
            wide: false,
        };
        if shift {
            for (offset, glyph) in tail.iter().enumerate() {
                let at = (row, col - 1 + u16::try_from(offset).unwrap_or(0));
                let underlying = grid_glyph(grid, rows, at).text;
                self.put(at, glyph, &underlying, now, true);
            }
            let last = (row, grid.size.cols - 1);
            let underlying = grid_glyph(grid, rows, last).text;
            self.put(last, &erased, &underlying, now, true);
        } else {
            let at = (row, col - 1);
            let underlying = grid_glyph(grid, rows, at).text;
            self.put(at, &erased, &underlying, now, false);
        }
        self.advance((row, col - 1), now);
        true
    }

    fn left(&mut self, grid: &RunGrid, rows: &mut HashMap<u16, Vec<Glyph>>, now: Instant) -> bool {
        let Some((row, col)) = self.origin(grid, now) else {
            return false;
        };
        if col == 0 {
            return false;
        }
        if !self.deletable(grid, rows, (row, col - 1)) {
            return false;
        }
        self.advance((row, col - 1), now);
        true
    }

    fn right(&mut self, grid: &RunGrid, rows: &mut HashMap<u16, Vec<Glyph>>, now: Instant) -> bool {
        let Some((row, col)) = self.origin(grid, now) else {
            return false;
        };
        if col + 1 >= grid.size.cols {
            return false;
        }
        // At the end of the text the shell may complete a suggestion instead of moving.
        let here = self.shown(grid, rows, (row, col));
        if here.wide || here.is_blank() {
            return false;
        }
        self.advance((row, col + 1), now);
        true
    }
}

/// Splits bytes into the keys that matter. Anything not understood is `Other`.
fn parse(bytes: &[u8]) -> Vec<Key> {
    let mut keys = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        match byte {
            0x1b => {
                let length = escape_len(&bytes[index..]);
                keys.push(match &bytes[index..index + length] {
                    b"\x1b[D" | b"\x1bOD" => Key::Left,
                    b"\x1b[C" | b"\x1bOC" => Key::Right,
                    _ => Key::Other,
                });
                index += length;
            }
            0x08 | 0x7f => {
                keys.push(Key::Backspace);
                index += 1;
            }
            0x20..=0x7e => {
                keys.push(Key::Char(char::from(byte)));
                index += 1;
            }
            0x00..=0x1f => {
                keys.push(Key::Other);
                index += 1;
            }
            _ => {
                let length = utf8_len(byte).min(bytes.len() - index);
                let ch = std::str::from_utf8(&bytes[index..index + length])
                    .ok()
                    .and_then(|text| text.chars().next());
                keys.push(match ch {
                    Some(ch) if narrow(ch) => Key::Char(ch),
                    _ => Key::Other,
                });
                index += length;
            }
        }
    }
    keys
}

fn utf8_len(first: u8) -> usize {
    match first {
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

/// Characters that are certainly one cell wide: Latin, Greek and Cyrillic letters.
fn narrow(ch: char) -> bool {
    matches!(ch, '\u{a1}'..='\u{2af}' | '\u{370}'..='\u{52f}')
        && !matches!(ch, '\u{ad}' | '\u{483}'..='\u{489}')
}

/// The length of the escape sequence at the start of `bytes`, which begins with ESC.
fn escape_len(bytes: &[u8]) -> usize {
    match bytes.get(1) {
        None => 1,
        Some(b'[') => {
            let mut index = 2;
            while bytes.get(index).is_some_and(|byte| (0x30..=0x3f).contains(byte)) {
                index += 1;
            }
            while bytes.get(index).is_some_and(|byte| (0x20..=0x2f).contains(byte)) {
                index += 1;
            }
            if bytes.get(index).is_some_and(|byte| (0x40..=0x7e).contains(byte)) {
                index += 1;
            }
            index
        }
        Some(b'O') => 3.min(bytes.len()),
        Some(_) => 2,
    }
}

fn grid_glyph(grid: &RunGrid, rows: &mut HashMap<u16, Vec<Glyph>>, (row, col): Point) -> Glyph {
    rows.entry(row)
        .or_insert_with(|| expand(grid, row))
        .get(usize::from(col))
        .cloned()
        .unwrap_or_else(Glyph::blank)
}

/// One glyph per column of `row`; trailing blanks the server trims come back as blanks.
fn expand(grid: &RunGrid, row: u16) -> Vec<Glyph> {
    let cols = usize::from(grid.size.cols);
    let mut glyphs = Vec::with_capacity(cols);
    for run in grid.rows.get(usize::from(row)).map_or(&[][..], Vec::as_slice) {
        if glyphs.len() >= cols {
            break;
        }
        let width = usize::from(run.width);
        if width == 0 {
            continue;
        }
        if run.text.is_ascii() && run.text.len() == width {
            glyphs.extend(run.text.chars().map(|ch| Glyph {
                text: ch.to_string(),
                style: run.style,
                wide: false,
            }));
        } else {
            let wide = width > 1;
            glyphs.push(Glyph {
                text: run.text.clone(),
                style: run.style,
                wide,
            });
            glyphs.extend((1..width).map(|_| Glyph {
                text: String::new(),
                style: run.style,
                wide,
            }));
        }
    }
    glyphs.resize(cols, Glyph::blank());
    glyphs.truncate(cols);
    glyphs
}

#[cfg(test)]
mod tests;
