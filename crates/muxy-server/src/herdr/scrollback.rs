//! Scrollback read from herdr with `pane.read`, which returns only the last N
//! rows as ANSI text with the screen's trailing blank rows trimmed.

use std::ops::Range;

use muxy_protocol::Size;
use muxy_terminal::{Row, Terminal};
use serde_json::json;

use super::api::Api;
use super::panes::PaneState;
use crate::session::backing::Fault;

/// Scratch terminals hold the rows being converted; they never keep history.
const SCRATCH_HISTORY_BYTES: usize = 1 << 16;

/// The history rows of `lines`, the tail of a `recent` read, given how many of
/// its last lines are the visible screen. Returns the number of history rows
/// before the first returned line, and the history lines.
fn history_lines<'a>(
    lines: &'a [&'a str],
    visible: usize,
    history_rows: usize,
) -> (usize, &'a [&'a str]) {
    let history = &lines[..lines.len().saturating_sub(visible)];
    (history_rows.saturating_sub(history.len()), history)
}

fn split(text: &str) -> Vec<&str> {
    if text.is_empty() {
        Vec::new()
    } else {
        text.split("\r\n").collect()
    }
}

/// Renders ANSI lines to rows by replaying them in a terminal tall enough
/// never to scroll.
fn rows(lines: &[&str], cols: u16) -> Result<Vec<Row>, Fault> {
    if lines.is_empty() {
        return Ok(Vec::new());
    }
    let height = u16::try_from(lines.len() + 1).map_err(|_| "history page too tall")?;
    let mut terminal = Terminal::new(Size { cols, rows: height }, SCRATCH_HISTORY_BYTES)?;
    terminal.feed(lines.join("\r\n").as_bytes());
    let mut screen = terminal.screen()?;
    screen.truncate(lines.len());
    Ok(screen)
}

/// Rows `range` of the scrollback, oldest first, at the pane's width.
pub(super) fn read(
    api: &Api,
    pane: &PaneState,
    cols: u16,
    range: Range<usize>,
) -> Result<Vec<Row>, Fault> {
    let end = range.end.min(pane.history_rows);
    if range.start >= end {
        return Ok(Vec::new());
    }
    let read = |source: &str, lines: usize| -> Result<String, Fault> {
        let reply = api.call(
            "pane.read",
            &json!({
                "pane_id": pane.pane,
                "source": source,
                "format": "ansi",
                "strip_ansi": false,
                "lines": lines,
            }),
        )?;
        Ok(reply["read"]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned())
    };
    let wanted = pane.history_rows - range.start + pane.viewport_rows + 1;
    let recent = read("recent", wanted)?;
    let visible = read("visible", pane.viewport_rows)?;
    let lines = split(&recent);
    let (first, history) = history_lines(&lines, split(&visible).len(), pane.history_rows);
    let from = range.start.max(first);
    let selected = if end > from {
        history
            .get(from - first..(end - first).min(history.len()))
            .unwrap_or_default()
    } else {
        &[]
    };
    let mut out = rows(selected, cols)?;
    // Rows older than what herdr kept come back blank so positions stay aligned.
    let missing = first.min(end).saturating_sub(range.start);
    if missing > 0 {
        let mut blank = (0..missing)
            .map(|_| Row {
                index: 0,
                runs: Vec::new(),
            })
            .collect::<Vec<_>>();
        blank.append(&mut out);
        out = blank;
    }
    for (index, row) in out.iter_mut().enumerate() {
        row.index = u16::try_from(index).unwrap_or(u16::MAX);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_visible_lines_are_not_history() {
        let lines = ["h1", "h2", "h3", "screen a", "screen b"];
        let (first, history) = history_lines(&lines, 2, 3);
        assert_eq!((first, history), (0, &["h1", "h2", "h3"][..]));
        // Only the newest two history rows were read out of three.
        let (first, history) = history_lines(&lines[1..], 2, 3);
        assert_eq!((first, history), (1, &["h2", "h3"][..]));
    }

    #[test]
    fn an_empty_read_has_no_lines() {
        assert!(split("").is_empty());
        assert_eq!(split("a\r\n\r\nb"), ["a", "", "b"]);
    }

    #[test]
    fn styled_lines_become_rows_with_runs() {
        let lines = ["\u{1b}[0m\u{1b}[38;5;2mgreen\u{1b}[0m", "plain"];
        let rows = rows(&lines, 20).expect("rows");
        assert_eq!(rows.len(), 2);
        let text = |row: &Row| {
            row.runs
                .iter()
                .map(|run| run.text.as_str())
                .collect::<String>()
        };
        assert_eq!(text(&rows[0]).trim_end(), "green");
        assert_eq!(text(&rows[1]).trim_end(), "plain");
        assert!(rows[0].runs.iter().any(|run| !run.style.is_default()));
    }
}
