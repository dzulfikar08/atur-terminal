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

use std::collections::BTreeSet;
use std::ops::Range;
use std::time::{Duration, Instant};

use muxy_terminal::Row;

/// How long after the screen first changed the submitted text is looked for.
const SETTLE: Duration = Duration::from_millis(120);
/// How long after a miss the screen is searched again.
const RETRY: Duration = Duration::from_millis(150);
/// When a submission that never showed up is given up.
const GIVE_UP: Duration = Duration::from_secs(2);
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

#[derive(Debug, Default)]
pub(super) struct PromptMarks {
    typed: Typed,
    pending: Option<Pending>,
    rows: BTreeSet<u64>,
}

impl PromptMarks {
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
        let rows: Vec<String> = screen.iter().map(row_text).collect();
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
        let row = if pending.pad && found > 0 && rows[found - 1].is_empty() {
            found - 1
        } else {
            found
        };
        let (Ok(base), Ok(row)) = (u64::try_from(base), u64::try_from(row)) else {
            return false;
        };
        self.rows.insert(base.saturating_add(row));
        if self.rows.len() > MAX_MARKS {
            self.rows.pop_first();
        }
        true
    }

    /// Forgets the awaited message: the screen could not be read.
    pub(super) fn abandon(&mut self) {
        self.pending = None;
    }

    /// Forgets every mark: the rows they point at are gone or moved.
    pub(super) fn clear(&mut self) {
        self.rows.clear();
        self.pending = None;
    }

    /// Forgets marks at or below row `total`, after the content shrank.
    pub(super) fn prune(&mut self, total: u64) {
        self.rows.retain(|row| *row < total);
    }

    /// Marks in scrollback rows `range`, as indexes into it.
    pub(super) fn history_rows(&self, range: &Range<usize>) -> Vec<u16> {
        let (Ok(start), Ok(end)) = (u64::try_from(range.start), u64::try_from(range.end)) else {
            return Vec::new();
        };
        self.rows
            .range(start..end)
            .filter_map(|row| u16::try_from(row - start).ok())
            .collect()
    }

    /// Marks on the screen under `history` rows of scrollback, as screen rows.
    pub(super) fn screen_rows(&self, history: usize, rows: u16) -> Vec<u16> {
        let Ok(start) = u64::try_from(history) else {
            return Vec::new();
        };
        self.rows
            .range(start..start + u64::from(rows))
            .filter_map(|row| u16::try_from(row - start).ok())
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
            typed.feed(b"helo\x7fllo world\r").as_deref(),
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
        assert_eq!(marks.rows.first().copied(), Some(2));
    }
}
