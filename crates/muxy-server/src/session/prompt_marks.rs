//! Prompt marks for terminals whose byte stream cannot carry OSC 133.
//!
//! A herdr terminal reaches the owner as ANSI that herdr re-renders from its own
//! grid. The semantic prompt marks an agent or a shell prints (omp writes one
//! around every message it shows) are dropped on the way, so the owner's terminal
//! never learns where a message starts. The owner therefore watches what the user
//! submits and, once the screen shows it, marks the row it landed on.
//!
//! A mark is an absolute row: the rows of scrollback above the screen plus the
//! screen row. Only what was submitted through this server is marked.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::time::{Duration, Instant};

use muxy_terminal::Row;
use serde::{Deserialize, Serialize};

/// How long after the screen first changed the submitted text is looked for.
const SETTLE: Duration = Duration::from_millis(120);
/// How long after a miss the screen is searched again.
const RETRY: Duration = Duration::from_millis(150);
/// When a submission that never showed up is given up.
const GIVE_UP: Duration = Duration::from_secs(2);
/// How often the marks on the screen are checked against what it shows.
const CHECK: Duration = Duration::from_millis(100);
/// Marks kept per session; the oldest go first.
const MAX_MARKS: usize = 4096;
/// Bytes of typed text kept: the first line is all that is searched for.
const MAX_TYPED: usize = 16 * 1024;
/// Characters of the first line that are searched for, then a shorter prefix for
/// a message whose first line wraps within them.
const NEEDLE: usize = 24;
const SHORT_NEEDLE: usize = 10;
/// A shorter message would match too many rows.
const MIN_NEEDLE: usize = 3;
/// Longest escape-sequence parameter string that is still a sequence.
const MAX_PARAMS: usize = 32;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Escape {
    #[default]
    None,
    Introducer,
    Csi,
    Ss3,
}

/// The text typed into the application since its last Enter, as far as the bytes
/// sent to it tell: printable characters, Backspace, kill keys, and bracketed
/// pastes. Cursor movement is not followed.
#[derive(Debug, Default)]
struct Typed {
    text: String,
    escape: Escape,
    params: String,
    paste: bool,
}

impl Typed {
    /// The text typed before the last Enter of `bytes` that was not part of a paste.
    fn feed(&mut self, bytes: &[u8]) -> Option<String> {
        let mut submitted = None;
        for ch in String::from_utf8_lossy(bytes).chars() {
            match self.escape {
                Escape::Introducer => {
                    self.params.clear();
                    self.escape = match ch {
                        '[' => Escape::Csi,
                        'O' => Escape::Ss3,
                        _ => Escape::None,
                    };
                }
                Escape::Ss3 => self.escape = Escape::None,
                Escape::Csi if ('\u{40}'..='\u{7e}').contains(&ch) => {
                    self.escape = Escape::None;
                    let params = std::mem::take(&mut self.params);
                    match (params.as_str(), ch) {
                        ("200", '~') => self.paste = true,
                        ("201", '~') => self.paste = false,
                        ("13" | "13;1", 'u') if !self.paste => submitted = Some(self.take()),
                        _ => {}
                    }
                }
                Escape::Csi => {
                    self.params.push(ch);
                    if self.params.len() > MAX_PARAMS {
                        self.escape = Escape::None;
                    }
                }
                Escape::None => {
                    if let Some(text) = self.key(ch) {
                        submitted = Some(text);
                    }
                }
            }
        }
        submitted
    }

    fn key(&mut self, ch: char) -> Option<String> {
        match ch {
            '\u{1b}' => self.escape = Escape::Introducer,
            '\r' if !self.paste => return Some(self.take()),
            '\r' | '\n' => self.push('\n'),
            '\u{7f}' | '\u{8}' => {
                self.text.pop();
            }
            '\u{3}' | '\u{15}' => self.text.clear(),
            '\u{17}' => {
                let kept = self.text.trim_end().len();
                self.text.truncate(kept);
                while self
                    .text
                    .chars()
                    .next_back()
                    .is_some_and(|c| !c.is_whitespace())
                {
                    self.text.pop();
                }
            }
            ch if ch.is_control() => {}
            ch => self.push(ch),
        }
        None
    }

    fn push(&mut self, ch: char) {
        if self.text.len() < MAX_TYPED {
            self.text.push(ch);
        }
    }

    fn take(&mut self) -> String {
        std::mem::take(&mut self.text)
    }
}

fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn row_text(row: &Row) -> String {
    normalize(
        &row.runs
            .iter()
            .map(|run| run.text.as_str())
            .collect::<String>(),
    )
}

/// What a submitted message looks like on screen: the start of its first line.
#[derive(Debug)]
struct Needle {
    long: String,
    short: String,
}

impl Needle {
    fn new(text: &str) -> Option<Self> {
        let line = text.lines().map(normalize).find(|line| !line.is_empty())?;
        if line.chars().count() < MIN_NEEDLE {
            return None;
        }
        let prefix = |count: usize| line.chars().take(count).collect::<String>();
        Some(Self {
            long: prefix(NEEDLE),
            short: prefix(SHORT_NEEDLE),
        })
    }

    fn matches(&self, row: &str) -> bool {
        row.contains(self.long.as_str()) || row.contains(self.short.as_str())
    }

    /// The lowest row above the cursor that shows the message, else the lowest
    /// row anywhere but at the cursor, which is where an unsent draft stays.
    fn find(&self, rows: &[String], cursor: usize) -> Option<usize> {
        for needle in [&self.long, &self.short] {
            let above = (0..cursor.min(rows.len())).rev();
            let elsewhere = (0..rows.len()).rev().filter(|row| *row != cursor);
            if let Some(row) = above
                .chain(elsewhere)
                .find(|row| rows[*row].contains(needle.as_str()))
            {
                return Some(row);
            }
        }
        None
    }
}

#[derive(Debug)]
struct Pending {
    needle: Needle,
    /// The application draws a blank padding row above a message, so the mark
    /// goes there: it is where a jump should put the top of the screen.
    pad: bool,
    submitted: Instant,
    /// When the screen first changed after the submission.
    changed: Option<Instant>,
    retry: Option<Instant>,
}

impl Pending {
    fn ready(&self) -> Instant {
        let deadline = self.submitted + GIVE_UP;
        let Some(changed) = self.changed else {
            return deadline;
        };
        let ready = changed + SETTLE;
        self.retry
            .map_or(ready, |retry| ready.max(retry))
            .min(deadline)
    }
}

/// A marked row and the message that is on it, so that the mark follows the
/// message while the application redraws what is on screen (a banner that goes
/// away moves everything under it up).
#[derive(Debug)]
struct Mark {
    needle: Needle,
    /// Rows from the marked row down to the one that shows the message.
    offset: usize,
    cols: u16,
}

/// A mark as stored with its session, so that a restart of the server keeps it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct SavedMark {
    row: u64,
    long: String,
    short: String,
    #[serde(default)]
    offset: usize,
    /// The width of the terminal when the row was counted: another width reflows it.
    #[serde(default)]
    cols: u16,
}

impl SavedMark {
    #[cfg(test)]
    pub(crate) fn new(row: u64, long: &str, short: &str, offset: usize) -> Self {
        Self {
            row,
            long: long.to_owned(),
            short: short.to_owned(),
            offset,
            cols: 0,
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct PromptMarks {
    typed: Typed,
    pending: Option<Pending>,
    rows: BTreeMap<u64, Mark>,
    checked: Option<Instant>,
    /// The width of the terminal now, which new marks are counted at.
    cols: u16,
    /// Whether the marks changed since they were last taken for storing.
    dirty: bool,
}

impl PromptMarks {
    /// Takes the marks as they are to be stored, when they changed since last taken.
    pub(super) fn take_changed(&mut self) -> Option<Vec<SavedMark>> {
        if !std::mem::take(&mut self.dirty) {
            return None;
        }
        Some(
            self.rows
                .iter()
                .map(|(row, mark)| SavedMark {
                    row: *row,
                    long: mark.needle.long.clone(),
                    short: mark.needle.short.clone(),
                    offset: mark.offset,
                    cols: mark.cols,
                })
                .collect(),
        )
    }

    /// Takes up marks stored by an earlier run, keeping those inside the `total`
    /// rows the content has now. They are checked against the screen as it shows.
    pub(super) fn restore(&mut self, saved: Vec<SavedMark>, total: u64) {
        for mark in saved.into_iter().filter(|mark| mark.row < total) {
            self.rows.insert(
                mark.row,
                Mark {
                    needle: Needle {
                        long: mark.long,
                        short: mark.short,
                    },
                    offset: mark.offset,
                    cols: mark.cols,
                },
            );
        }
        if self.rows.len() > MAX_MARKS {
            self.rows.pop_first();
        }
    }

    /// The terminal is `cols` wide now. Marks counted at another width are gone:
    /// the content reflowed under them.
    pub(super) fn resized(&mut self, cols: u16) {
        let before = self.rows.len();
        self.rows.retain(|_, mark| mark.cols == cols);
        self.dirty |= self.rows.len() != before;
        self.cols = cols;
    }

    /// Sets the width new marks are counted at, without touching the others.
    pub(super) fn set_width(&mut self, cols: u16) {
        self.cols = cols;
    }

    pub(super) fn len(&self) -> usize {
        self.rows.len()
    }

    /// The text of each screen row, for matching.
    pub(super) fn texts(screen: &[Row]) -> Vec<String> {
        screen.iter().map(row_text).collect()
    }

    /// Follows bytes sent to the application; the text typed before an Enter.
    pub(super) fn type_in(&mut self, bytes: &[u8]) -> Option<String> {
        self.typed.feed(bytes)
    }

    /// Starts looking for a submitted message. False when it is too short to find.
    pub(super) fn expect(&mut self, text: &str, pad: bool, now: Instant) -> bool {
        let Some(needle) = Needle::new(text) else {
            return false;
        };
        self.pending = Some(Pending {
            needle,
            pad,
            submitted: now,
            changed: None,
            retry: None,
        });
        true
    }

    /// Output reached the terminal.
    pub(super) fn screen_changed(&mut self, now: Instant) {
        if let Some(pending) = &mut self.pending
            && pending.changed.is_none()
        {
            pending.changed = Some(now);
        }
    }

    /// When the screen is to be searched, if a message is awaited.
    pub(super) fn next_check(&self) -> Option<Instant> {
        self.pending.as_ref().map(Pending::ready)
    }

    pub(super) fn due(&self, now: Instant) -> bool {
        self.next_check().is_some_and(|at| at <= now)
    }

    /// Searches the screen for the awaited message. `base` is the number of rows
    /// of scrollback above it. True when a mark was added.
    pub(super) fn resolve(
        &mut self,
        now: Instant,
        screen: &[Row],
        cursor: u16,
        base: usize,
    ) -> bool {
        let Some(pending) = self.pending.take() else {
            return false;
        };
        let rows = Self::texts(screen);
        // Content that is no longer there takes its marks with it.
        self.prune(u64::try_from(base.saturating_add(rows.len())).unwrap_or(u64::MAX));
        let Some(found) = pending.needle.find(&rows, usize::from(cursor)) else {
            if now < pending.submitted + GIVE_UP {
                self.pending = Some(Pending {
                    retry: Some(now + RETRY),
                    ..pending
                });
            }
            return false;
        };
        let offset = usize::from(pending.pad && found > 0 && rows[found - 1].is_empty());
        let (Ok(base), Ok(row)) = (u64::try_from(base), u64::try_from(found - offset)) else {
            return false;
        };
        self.rows.insert(
            base.saturating_add(row),
            Mark {
                needle: pending.needle,
                offset,
                cols: self.cols,
            },
        );
        if self.rows.len() > MAX_MARKS {
            self.rows.pop_first();
        }
        self.dirty = true;
        true
    }

    /// Forgets the awaited message: the screen could not be read.
    pub(super) fn abandon(&mut self) {
        self.pending = None;
    }

    /// Forgets every mark: the rows they point at are gone or moved.
    pub(super) fn clear(&mut self) {
        self.dirty |= !self.rows.is_empty();
        self.rows.clear();
        self.pending = None;
    }

    /// Forgets marks at or below row `total`, after the content shrank.
    pub(super) fn prune(&mut self, total: u64) {
        let before = self.rows.len();
        self.rows.retain(|row, _| *row < total);
        self.dirty |= self.rows.len() != before;
    }

    /// Whether the marks on a screen of `rows` rows under `history` rows of
    /// scrollback are to be checked against what the screen shows: some are
    /// there, and the last check was a while ago.
    pub(super) fn check_due(&mut self, history: usize, rows: u16, now: Instant) -> bool {
        if self.checked.is_some_and(|at| now < at + CHECK)
            || self.on_screen(history, usize::from(rows)).next().is_none()
        {
            return false;
        }
        self.checked = Some(now);
        true
    }

    fn on_screen(&self, history: usize, rows: usize) -> impl Iterator<Item = (&u64, &Mark)> {
        let start = u64::try_from(history).unwrap_or(u64::MAX);
        let end = start.saturating_add(u64::try_from(rows).unwrap_or(u64::MAX));
        self.rows.range(start..end)
    }

    /// Whether a mark on the screen no longer has its message where it points.
    pub(super) fn stale(&self, history: usize, texts: &[String]) -> bool {
        let start = u64::try_from(history).unwrap_or(u64::MAX);
        self.on_screen(history, texts.len()).any(|(row, mark)| {
            usize::try_from(row - start)
                .ok()
                .and_then(|row| texts.get(row + mark.offset))
                .is_none_or(|text| !mark.needle.matches(text))
        })
    }

    /// Moves each mark on the screen to where its message is now: the closest row
    /// that shows it and no other mark does. A mark whose message is gone is
    /// dropped.
    pub(super) fn reanchor(&mut self, history: usize, texts: &[String]) {
        let start = u64::try_from(history).unwrap_or(u64::MAX);
        let keys: Vec<u64> = self
            .on_screen(history, texts.len())
            .map(|(row, _)| *row)
            .collect();
        let at = |key: u64, offset: usize| {
            usize::try_from(key - start)
                .ok()
                .map(|row| row.saturating_add(offset))
        };
        self.dirty = true;
        let mut claimed = BTreeSet::new();
        let mut lost = Vec::new();
        for key in keys {
            let Some(mark) = self.rows.get(&key) else {
                continue;
            };
            match at(key, mark.offset).filter(|row| {
                texts
                    .get(*row)
                    .is_some_and(|text| mark.needle.matches(text))
            }) {
                Some(row) => {
                    claimed.insert(row);
                }
                None => lost.push(key),
            }
        }
        for key in lost {
            let Some(mark) = self.rows.remove(&key) else {
                continue;
            };
            let expected = at(key, mark.offset).unwrap_or(0);
            let moved = (0..texts.len())
                .filter(|row| !claimed.contains(row) && mark.needle.matches(&texts[*row]))
                .min_by_key(|row| row.abs_diff(expected))
                .filter(|row| *row >= mark.offset)
                .and_then(|row| Some((row, u64::try_from(row - mark.offset).ok()?)));
            if let Some((row, from_top)) = moved {
                claimed.insert(row);
                self.rows.insert(start.saturating_add(from_top), mark);
            }
        }
    }

    /// Marks in scrollback rows `range`, as indexes into it.
    pub(super) fn history_rows(&self, range: &Range<usize>) -> Vec<u16> {
        let (Ok(start), Ok(end)) = (u64::try_from(range.start), u64::try_from(range.end)) else {
            return Vec::new();
        };
        self.rows
            .range(start..end)
            .filter_map(|(row, _)| u16::try_from(row - start).ok())
            .collect()
    }

    /// Marks on the screen under `history` rows of scrollback, as screen rows.
    pub(super) fn screen_rows(&self, history: usize, rows: u16) -> Vec<u16> {
        let start = u64::try_from(history).unwrap_or(u64::MAX);
        self.on_screen(history, usize::from(rows))
            .filter_map(|(row, _)| u16::try_from(row - start).ok())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use muxy_protocol::{Run, Style};

    use super::*;

    fn screen(lines: &[&str]) -> Vec<Row> {
        lines
            .iter()
            .enumerate()
            .map(|(index, text)| Row {
                index: u16::try_from(index).unwrap_or(u16::MAX),
                runs: vec![Run {
                    text: (*text).to_owned(),
                    width: u16::try_from(text.chars().count()).unwrap_or(u16::MAX),
                    style: Style::default(),
                }],
            })
            .collect()
    }

    fn submit(marks: &mut PromptMarks, text: &str, pad: bool, now: Instant) {
        assert!(marks.expect(text, pad, now));
        marks.screen_changed(now);
    }

    #[test]
    fn typed_text_follows_edits_pastes_and_kill_keys() {
        let mut typed = Typed::default();
        assert_eq!(
            typed.feed(b"hellp\x7fo world\r").as_deref(),
            Some("hello world")
        );
        assert_eq!(
            typed.feed(b"junk\x15fix\x1b[D\x1bOA\r").as_deref(),
            Some("fix")
        );
        assert_eq!(
            typed.feed(b"one two\x17three\r").as_deref(),
            Some("one three")
        );
        assert_eq!(
            typed
                .feed(b"\x1b[200~first\r\nsecond\x1b[201~\r")
                .as_deref(),
            Some("first\n\nsecond")
        );
        assert_eq!(typed.feed(b"kitty\x1b[13u").as_deref(), Some("kitty"));
        assert_eq!(typed.feed(b"\r").as_deref(), Some(""));
        // Typing continues across writes.
        assert_eq!(typed.feed(b"par"), None);
        assert_eq!(typed.feed(b"ts\r").as_deref(), Some("parts"));
        // Alt+Enter is a newline, not a submission.
        assert_eq!(typed.feed(b"a\x1b\rb\r").as_deref(), Some("ab"));
    }

    #[test]
    fn a_message_is_found_by_the_start_of_its_first_line() {
        let needle =
            Needle::new("\n  Fix the   login redirect\nand then the tests").expect("needle");
        let rows = [
            "Fix the login redirect".to_owned(),
            String::new(),
            "Sure, I will fix the login redirect".to_owned(),
            "> ".to_owned(),
        ];
        assert_eq!(needle.find(&rows, 3), Some(0));
        // The cursor row, where an unsent draft stays, is not a match.
        let draft = ["Fix the login redirect".to_owned()];
        assert_eq!(needle.find(&draft, 0), None);
        assert!(Needle::new("ok").is_none());
        assert!(Needle::new("   \n ").is_none());
    }

    #[test]
    fn a_long_first_line_that_wraps_is_found_by_its_head() {
        let needle = Needle::new("please refactor the session owner module").expect("needle");
        let rows = [
            "please refactor the".to_owned(),
            "session owner module".to_owned(),
        ];
        assert_eq!(needle.find(&rows, 5), Some(0));
    }

    #[test]
    fn the_mark_is_an_absolute_row_on_the_padding_above_the_message() {
        let mut marks = PromptMarks::default();
        let now = Instant::now();
        submit(&mut marks, "Fix the login redirect", true, now);
        let rows = screen(&[
            "",
            "older output",
            "",
            "Fix the login redirect",
            "",
            "answer",
            "> ",
        ]);
        assert!(!marks.due(now));
        assert!(marks.due(now + SETTLE));
        assert!(marks.resolve(now + SETTLE, &rows, 6, 100));
        assert_eq!(marks.next_check(), None);
        assert_eq!(marks.screen_rows(100, 7), [2]);
        assert_eq!(marks.history_rows(&(90..110)), [12]);
        assert!(marks.history_rows(&(0..100)).is_empty());
        marks.prune(102);
        assert!(marks.screen_rows(100, 7).is_empty());
    }

    #[test]
    fn a_shell_command_is_marked_on_its_own_row() {
        let mut marks = PromptMarks::default();
        let now = Instant::now();
        submit(&mut marks, "cargo test", false, now);
        let rows = screen(&["", "$ cargo test", "ok", "$ "]);
        assert!(marks.resolve(now + SETTLE, &rows, 3, 0));
        assert_eq!(marks.screen_rows(0, 4), [1]);
    }

    #[test]
    fn a_message_that_has_not_appeared_is_searched_for_again_then_given_up() {
        let mut marks = PromptMarks::default();
        let now = Instant::now();
        submit(&mut marks, "wait for me", true, now);
        let blank = screen(&["", "", "> "]);
        let later = now + SETTLE;
        assert!(!marks.resolve(later, &blank, 2, 0));
        assert!(!marks.due(later));
        assert!(marks.due(later + RETRY));
        let shown = screen(&["wait for me", "", "> "]);
        assert!(marks.resolve(later + RETRY, &shown, 2, 7));
        assert_eq!(marks.history_rows(&(0..20)), [7]);

        submit(&mut marks, "never shown", true, now);
        assert!(!marks.resolve(now + GIVE_UP, &blank, 2, 0));
        assert_eq!(marks.next_check(), None);
    }

    #[test]
    fn nothing_is_searched_before_the_screen_changes() {
        let mut marks = PromptMarks::default();
        let now = Instant::now();
        assert!(marks.expect("hello there", true, now));
        assert!(!marks.due(now + SETTLE));
        assert!(marks.due(now + GIVE_UP));
        marks.screen_changed(now + Duration::from_millis(500));
        assert!(!marks.due(now + Duration::from_millis(500)));
        assert!(marks.due(now + Duration::from_millis(500) + SETTLE));
    }

    #[test]
    fn old_marks_go_first_when_there_are_too_many() {
        let mut marks = PromptMarks::default();
        let now = Instant::now();
        for index in 0..=MAX_MARKS {
            submit(&mut marks, "hello there", false, now);
            let rows = screen(&["hello there", "> "]);
            assert!(marks.resolve(now + SETTLE, &rows, 1, index * 2));
        }
        assert_eq!(marks.history_rows(&(0..MAX_MARKS * 2 + 2)).len(), MAX_MARKS);
        assert_eq!(marks.rows.len(), MAX_MARKS);
        assert_eq!(marks.rows.keys().next().copied(), Some(2));
    }

    fn marked(lines: &[&str], pad: bool, base: usize, cursor: u16) -> PromptMarks {
        let mut marks = PromptMarks::default();
        let now = Instant::now();
        submit(&mut marks, "fix the login redirect", pad, now);
        assert!(marks.resolve(now + SETTLE, &screen(lines), cursor, base));
        marks
    }

    #[test]
    fn marks_taken_for_storing_come_back_after_a_restart() {
        let mut marks = marked(&["", "fix the login redirect", "", "> "], true, 40, 3);
        let saved = marks.take_changed().expect("changed");
        assert_eq!(saved.len(), 1);
        assert!(marks.take_changed().is_none());
        let json = serde_json::to_string(&saved).expect("serialize");
        let saved: Vec<SavedMark> = serde_json::from_str(&json).expect("deserialize");

        let mut fresh = PromptMarks::default();
        fresh.resized(0);
        // Row 99 is beyond the 44 rows of content left; row 40 is inside them.
        let mut outside = saved.clone();
        outside.push(SavedMark::new(99, "gone", "gone", 0));
        fresh.restore(outside, 44);
        assert_eq!(fresh.history_rows(&(0..60)), [40]);
        assert_eq!(fresh.screen_rows(40, 4), [0]);
        let texts = PromptMarks::texts(&screen(&["", "fix the login redirect", "", "> "]));
        assert!(!fresh.stale(40, &texts));
        assert!(fresh.take_changed().is_none());
        // The same width keeps them, another one drops them.
        fresh.resized(0);
        assert_eq!(fresh.len(), 1);
        assert!(fresh.take_changed().is_none());
        fresh.resized(100);
        assert_eq!(fresh.len(), 0);
        assert!(fresh.take_changed().is_some());
    }

    #[test]
    fn a_mark_follows_its_message_when_the_screen_shifts() {
        let mut marks = marked(&["", "", "fix the login redirect", "", "> "], true, 100, 4);
        // Rows 0 and 1 of the screen hold the mark's padding row and message now.
        assert_eq!(marks.screen_rows(100, 5), [1]);
        let moved = PromptMarks::texts(&screen(&["", "fix the login redirect", "", "> ", ""]));
        assert!(marks.stale(100, &moved));
        marks.reanchor(100, &moved);
        assert!(!marks.stale(100, &moved));
        assert_eq!(marks.screen_rows(100, 5), [0]);
        // Where it is is also where it stays.
        marks.reanchor(100, &moved);
        assert_eq!(marks.screen_rows(100, 5), [0]);
    }

    #[test]
    fn a_mark_whose_message_was_overwritten_is_dropped() {
        // A shell command's row that a full-screen program then paints over.
        let mut marks = marked(&["fix the login redirect", "ok", "$ "], false, 0, 2);
        let gone = PromptMarks::texts(&screen(&["welcome banner", "", "> "]));
        assert!(marks.stale(0, &gone));
        marks.reanchor(0, &gone);
        assert!(marks.screen_rows(0, 3).is_empty());
        assert!(marks.history_rows(&(0..10)).is_empty());
    }

    #[test]
    fn two_marks_with_the_same_text_keep_their_own_rows() {
        let mut marks = marked(&["fix the login redirect", "", "> "], false, 0, 2);
        let now = Instant::now();
        submit(&mut marks, "fix the login redirect", false, now);
        let both = screen(&[
            "fix the login redirect",
            "x",
            "fix the login redirect",
            "> ",
        ]);
        assert!(marks.resolve(now + SETTLE, &both, 3, 0));
        let shifted = PromptMarks::texts(&screen(&[
            "x",
            "fix the login redirect",
            "y",
            "fix the login redirect",
        ]));
        marks.reanchor(0, &shifted);
        assert_eq!(marks.screen_rows(0, 4), [1, 3]);
    }

    #[test]
    fn marks_in_scrollback_are_not_checked() {
        let mut marks = marked(&["fix the login redirect", "", "> "], false, 0, 2);
        assert!(marks.check_due(0, 3, Instant::now()));
        // The screen moved on by ten rows: the mark is history, untouched.
        assert!(!marks.check_due(10, 3, Instant::now() + CHECK));
        assert_eq!(marks.history_rows(&(0..10)), [0]);
    }
}
