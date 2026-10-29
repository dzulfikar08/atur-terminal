use std::collections::{BTreeSet, VecDeque};

use muxy_protocol::{Cursor, CursorShape, Modes, Run, Size};

use super::*;

const COLS: u16 = 20;

/// A fake terminal the tests play the server with.
struct Screen {
    grid: RunGrid,
}

impl Screen {
    /// A screen showing `line` on row 0 with the cursor at its end.
    fn new(line: &str) -> Self {
        let mut screen = Self {
            grid: RunGrid {
                graphics: muxy_protocol::Graphics::default(),
                prompts: BTreeSet::new(),
                prompt_state: crate::ScreenPrompts::default(),
                links: crate::ScreenLinks::default(),
                size: Size {
                    cols: COLS,
                    rows: 4,
                },
                rows: vec![Vec::new(); 4],
                cursor: Cursor {
                    shape: CursorShape::Block,
                    row: 0,
                    col: 0,
                    visible: true,
                },
                modes: Modes::default(),
                history: VecDeque::new(),
                history_cursor: None,
                history_total: 0,
                history_fresh: false,
            },
        };
        screen.set(0, line, line.len());
        screen
    }

    fn set(&mut self, row: usize, text: &str, cursor: usize) {
        self.grid.rows[row] = if text.is_empty() {
            Vec::new()
        } else {
            vec![Run {
                text: text.into(),
                width: u16::try_from(text.len()).unwrap_or(0),
                style: Style::default(),
            }]
        };
        self.grid.cursor.row = u16::try_from(row).unwrap_or(0);
        self.grid.cursor.col = u16::try_from(cursor).unwrap_or(0);
    }

    fn line(&self) -> String {
        self.grid.row_text(0)
    }
}

struct Session {
    predictor: Predictor,
    screen: Screen,
    now: Instant,
}

impl Session {
    fn new(mode: PredictMode, line: &str) -> Self {
        Self {
            predictor: Predictor::new(mode),
            screen: Screen::new(line),
            now: Instant::now(),
        }
    }

    fn wait(&mut self, millis: u64) {
        self.now += Duration::from_millis(millis);
    }

    fn type_bytes(&mut self, bytes: &[u8]) {
        self.predictor.keys(bytes, &self.screen.grid, self.now);
    }

    /// The server answers with `text` on row 0 and the cursor at `cursor`.
    fn serve(&mut self, text: &str, cursor: usize) {
        self.screen.set(0, text, cursor);
        self.predictor
            .frame(&self.screen.grid, false, self.now);
    }

    /// What the user sees on row 0: the grid with predicted cells drawn over it.
    fn shown(&self) -> String {
        let mut line: Vec<char> = format!("{:<width$}", self.screen.line(), width = usize::from(COLS))
            .chars()
            .collect();
        for cell in self.predictor.cells(self.now) {
            if cell.row == 0 {
                line[usize::from(cell.col)] = cell.text.chars().next().unwrap_or('?');
            }
        }
        line.into_iter().collect::<String>().trim_end().to_owned()
    }

    fn cursor(&self) -> Option<Point> {
        self.predictor.cursor(self.now)
    }

    /// Types `a` and has the server echo it, so the epoch is trusted.
    fn trust(&mut self) {
        let before = self.screen.line();
        let col = before.len();
        self.type_bytes(b"a");
        self.wait(100);
        self.serve(&format!("{before}a"), col + 1);
    }
}

#[test]
fn first_key_is_hidden_until_the_server_echoes_it() {
    let mut session = Session::new(PredictMode::Always, "$ ");
    session.type_bytes(b"l");
    assert_eq!(session.shown(), "$", "unproven epoch shows nothing");
    assert_eq!(session.cursor(), None);
    session.wait(100);
    session.serve("$ l", 3);
    session.type_bytes(b"s");
    assert_eq!(session.shown(), "$ ls");
    assert_eq!(session.cursor(), Some((0, 4)));
}

#[test]
fn keys_after_a_trusted_one_show_at_once_and_confirm_quietly() {
    let mut session = Session::new(PredictMode::Always, "$ ");
    session.trust();
    session.type_bytes(b"bc");
    assert_eq!(session.shown(), "$ abc");
    assert_eq!(session.cursor(), Some((0, 5)));
    session.wait(200);
    session.serve("$ abc", 5);
    assert!(!session.predictor.pending());
    assert_eq!(session.cursor(), None);
    assert_eq!(session.shown(), "$ abc");
    let stats = session.predictor.stats();
    assert_eq!(stats.visible_wrong + stats.hidden_wrong, 0);
    assert_eq!(stats.visible_confirmed, 2);
}

#[test]
fn partial_echo_keeps_the_rest_pending() {
    let mut session = Session::new(PredictMode::Always, "$ ");
    session.trust();
    session.type_bytes(b"bcd");
    session.wait(100);
    session.serve("$ ab", 4);
    assert_eq!(session.shown(), "$ abcd", "c and d are still guessed");
    session.serve("$ abcd", 6);
    assert!(!session.predictor.pending());
}

#[test]
fn wrong_echo_rolls_back_and_distrusts_the_next_guess() {
    let mut session = Session::new(PredictMode::Always, "$ ");
    session.trust();
    session.type_bytes(b"x");
    assert_eq!(session.shown(), "$ ax");
    session.wait(100);
    session.serve("$ ay", 4);
    assert_eq!(session.shown(), "$ ay", "the server wins");
    assert!(!session.predictor.pending());
    assert_eq!(session.predictor.stats().visible_wrong, 1);
    session.type_bytes(b"z");
    assert_eq!(session.shown(), "$ ay", "guesses are hidden again until one is proven");
    session.wait(100);
    session.serve("$ ayz", 5);
    session.type_bytes(b"w");
    assert_eq!(session.shown(), "$ ayzw");
}

#[test]
fn password_prompt_never_shows_typed_text() {
    // A program prints its own prompt, so no Enter starts a fresh epoch before the typing.
    let mut session = Session::new(PredictMode::Always, "Password: ");
    let sent = session.now;
    session.type_bytes(b"hunter2");
    assert!(session.predictor.pending(), "guessed silently");
    assert_eq!(session.shown(), "Password:", "nothing echoed, nothing shown");
    assert_eq!(session.cursor(), None);
    session.wait(300);
    session.predictor.barrier(sent, session.now);
    session.wait(300);
    assert!(session.predictor.tick(session.now));
    assert!(!session.predictor.pending());
    let stats = session.predictor.stats();
    assert_eq!(stats.visible_wrong, 0, "no password character was ever drawn");
    assert!(stats.hidden_wrong > 0);
    session.type_bytes(b"x");
    assert_eq!(session.shown(), "Password:", "and the next try is still hidden");
}

#[test]
fn password_after_enter_is_not_even_guessed() {
    let mut session = Session::new(PredictMode::Always, "$ ");
    session.trust();
    session.type_bytes(b"\r");
    session.wait(100);
    session.serve("Password: ", 10);
    session.type_bytes(b"hunter2");
    assert!(!session.predictor.pending());
    assert_eq!(session.shown(), "Password:");
    assert_eq!(session.predictor.stats().visible_wrong, 0);
}

#[test]
fn password_typed_in_a_trusted_epoch_cannot_happen_because_enter_starts_a_new_one() {
    let mut session = Session::new(PredictMode::Always, "$ sudo ");
    session.trust();
    session.type_bytes(b"\r");
    session.wait(50);
    session.serve("[sudo] password: ", 17);
    session.type_bytes(b"s");
    assert_eq!(session.shown(), "[sudo] password:");
}

#[test]
fn nothing_is_anchored_until_the_server_caught_up_with_enter() {
    let mut session = Session::new(PredictMode::Always, "$ ls");
    session.trust();
    let enter = session.now;
    session.type_bytes(b"\r");
    session.type_bytes(b"q");
    assert!(!session.predictor.pending() || session.predictor.cells(session.now).is_empty());
    session.wait(100);
    session.predictor.barrier(enter, session.now);
    session.wait(300);
    session.predictor.tick(session.now);
    session.screen.set(1, "$ ", 2);
    session.screen.grid.cursor.row = 1;
    session.type_bytes(b"k");
    assert!(session.predictor.pending(), "anchored on the new line after resync");
}

#[test]
fn backspace_erases_the_character_before_the_cursor() {
    let mut session = Session::new(PredictMode::Always, "$ ");
    session.trust();
    session.type_bytes(b"bc");
    session.wait(100);
    session.serve("$ abc", 5);
    session.type_bytes(b"\x7f");
    assert_eq!(session.shown(), "$ ab");
    assert_eq!(session.cursor(), Some((0, 4)));
    session.wait(100);
    session.serve("$ ab", 4);
    assert!(!session.predictor.pending());
}

#[test]
fn backspace_goes_back_over_typed_spaces_but_not_into_the_prompt() {
    let mut session = Session::new(PredictMode::Always, "$ ");
    session.trust();
    session.type_bytes(b" b");
    session.wait(100);
    session.serve("$ a b", 5);
    session.type_bytes(b"\x7f\x7f\x7f");
    assert_eq!(session.shown(), "$", "a, the space and b are all erased");
    assert_eq!(session.cursor(), Some((0, 2)));
    session.type_bytes(b"\x7f");
    assert_eq!(session.cursor(), None, "no guess about what the shell does at the prompt");
    assert_eq!(session.shown(), "$", "and the prompt itself is untouched");
}

#[test]
fn backspace_at_the_start_of_the_input_is_not_predicted() {
    let mut session = Session::new(PredictMode::Always, "$ ");
    session.trust();
    session.wait(100);
    session.serve("$ ", 2);
    session.type_bytes(b"\x7f");
    assert_eq!(session.cursor(), None);
    assert!(session.predictor.cells(session.now).is_empty());
}

#[test]
fn backspace_mid_line_shifts_the_rest_of_the_line_left() {
    let mut session = Session::new(PredictMode::Always, "$ ");
    session.trust();
    session.type_bytes(b"bc");
    session.wait(100);
    session.serve("$ abc", 5);
    session.type_bytes(b"\x1b[D\x1b[D");
    session.wait(100);
    session.serve("$ abc", 3);
    session.type_bytes(b"\x7f");
    assert_eq!(session.shown(), "$ bc");
    session.wait(100);
    session.serve("$ bc", 2);
    assert!(!session.predictor.pending());
}

#[test]
fn typing_mid_line_shifts_the_rest_right_and_learns_overwrite_after_a_miss() {
    let mut session = Session::new(PredictMode::Always, "$ ");
    session.trust();
    session.type_bytes(b"b");
    session.wait(100);
    session.serve("$ ab", 4);
    session.type_bytes(b"\x1b[D");
    session.wait(100);
    session.serve("$ ab", 3);
    session.type_bytes(b"x");
    assert_eq!(session.shown(), "$ axb");
    // The program overwrote instead of inserting.
    session.wait(100);
    session.serve("$ ax", 4);
    let sent = session.now - Duration::from_millis(100);
    session.predictor.barrier(sent, session.now);
    session.wait(300);
    assert!(session.predictor.tick(session.now));
    assert_eq!(session.predictor.mid_line, MidLine::Overwrite);
    session.trust();
    session.type_bytes(b"\x1b[D");
    session.wait(100);
    session.serve("$ axa", 4);
    session.type_bytes(b"y");
    assert_eq!(session.shown(), "$ axy", "overwrite guesses only the typed cell");
}

#[test]
fn arrow_keys_move_the_predicted_cursor_only() {
    let mut session = Session::new(PredictMode::Always, "$ ");
    session.trust();
    session.type_bytes(b"\x1b[D");
    assert_eq!(session.cursor(), Some((0, 2)));
    session.type_bytes(b"\x1bOC");
    assert_eq!(session.cursor(), Some((0, 3)));
    assert!(session.predictor.cells(session.now).is_empty());
}

#[test]
fn right_arrow_at_the_end_of_the_text_is_not_predicted() {
    let mut session = Session::new(PredictMode::Always, "$ ab");
    session.trust();
    session.wait(100);
    session.serve("$ aba", 5);
    session.type_bytes(b"\x1b[C");
    assert_eq!(session.cursor(), None);
}

#[test]
fn escape_sequences_and_control_keys_start_a_new_epoch() {
    let mut session = Session::new(PredictMode::Always, "$ ");
    session.trust();
    session.type_bytes(b"\x1b[1;5C");
    session.type_bytes(b"x");
    assert!(
        session.predictor.cells(session.now).is_empty(),
        "no stray [ 1 ; 5 C characters and no guess after it"
    );
    session.type_bytes(b"\x03");
    assert!(!session.predictor.pending());
}

#[test]
fn a_paste_is_not_predicted() {
    let mut session = Session::new(PredictMode::Always, "$ ");
    session.trust();
    session.type_bytes(b"0123456789");
    assert!(!session.predictor.pending());
    assert_eq!(session.shown(), "$ a");
}

#[test]
fn typing_into_the_last_column_is_not_predicted() {
    let mut session = Session::new(PredictMode::Always, &"x".repeat(usize::from(COLS) - 2));
    session.trust();
    session.type_bytes(b"y");
    assert!(!session.predictor.pending());
}

#[test]
fn never_mode_predicts_nothing() {
    let mut session = Session::new(PredictMode::Never, "$ ");
    session.type_bytes(b"abc");
    assert!(!session.predictor.pending());
    assert_eq!(session.predictor.stats().keys, 0);
}

#[test]
fn auto_mode_waits_for_a_slow_link_and_has_hysteresis() {
    let mut session = Session::new(PredictMode::Auto, "$ ");
    session.trust();
    session.type_bytes(b"b");
    assert_eq!(session.shown(), "$ a", "fast link: shadow guess only");
    session.wait(10);
    session.serve("$ ab", 4);
    session.predictor.set_rtt(Some(Duration::from_millis(80)));
    session.type_bytes(b"c");
    assert_eq!(session.shown(), "$ abc");
    session.predictor.set_rtt(Some(Duration::from_millis(25)));
    assert_eq!(session.shown(), "$ abc", "between the thresholds the state is kept");
    session.predictor.set_rtt(Some(Duration::from_millis(5)));
    assert_eq!(session.shown(), "$ ab");
}

#[test]
fn a_slow_echo_turns_auto_on_without_an_rtt_estimate() {
    let mut session = Session::new(PredictMode::Auto, "$ ");
    session.trust();
    session.type_bytes(b"b");
    session.wait(400);
    session.serve("$ ab", 4);
    session.type_bytes(b"c");
    assert_eq!(session.shown(), "$ abc");
}

#[test]
fn predictions_unconfirmed_for_250_ms_are_underlined() {
    let mut session = Session::new(PredictMode::Always, "$ ");
    session.trust();
    session.type_bytes(b"b");
    assert!(!session.predictor.cells(session.now)[0].underline);
    session.wait(250);
    assert!(session.predictor.cells(session.now)[0].underline);
    assert_eq!(
        session.predictor.next_deadline(session.now),
        None,
        "nothing left to wait for once underlined"
    );
}

#[test]
fn deadline_is_when_the_underline_starts() {
    let mut session = Session::new(PredictMode::Always, "$ ");
    session.trust();
    let sent = session.now;
    session.type_bytes(b"b");
    assert_eq!(
        session.predictor.next_deadline(session.now),
        Some(sent + UNDERLINE_AFTER)
    );
}

#[test]
fn an_outage_keeps_predictions_on_screen_until_the_server_answers() {
    let mut session = Session::new(PredictMode::Always, "$ ");
    session.trust();
    session.type_bytes(b"bc");
    session.wait(10_000);
    assert_eq!(session.shown(), "$ abc");
    assert!(session.predictor.cells(session.now).iter().all(|cell| cell.underline));
    assert!(!session.predictor.tick(session.now), "no ping answered, so nothing expires");
    session.serve("$ abc", 5);
    assert!(!session.predictor.pending());
}

#[test]
fn a_ping_answer_expires_guesses_nobody_echoed() {
    let mut session = Session::new(PredictMode::Always, "$ ");
    session.trust();
    let sent = session.now;
    session.type_bytes(b"b");
    session.wait(500);
    session.predictor.barrier(sent, session.now);
    assert!(!session.predictor.tick(session.now), "still within the grace");
    session.wait(300);
    assert!(session.predictor.tick(session.now));
    assert!(!session.predictor.pending());
    assert_eq!(session.predictor.stats().visible_wrong, 1);
}

#[test]
fn input_sent_after_the_ping_is_not_settled_by_it() {
    let mut session = Session::new(PredictMode::Always, "$ ");
    session.trust();
    let stamp = session.now;
    session.wait(10);
    session.type_bytes(b"b");
    session.wait(500);
    session.predictor.barrier(stamp, session.now);
    session.wait(500);
    assert!(!session.predictor.tick(session.now));
    assert!(session.predictor.pending());
}

#[test]
fn toggling_the_alternate_screen_drops_guesses_and_trust() {
    let mut session = Session::new(PredictMode::Always, "$ ");
    session.trust();
    session.type_bytes(b"b");
    session.predictor.set_alternate_screen(true);
    assert!(!session.predictor.pending());
    session.type_bytes(b"j");
    assert_eq!(session.shown(), "$ a", "vim normal mode: a key is checked before it shows");
    session.wait(100);
    session.serve("$ a", 3);
    assert_eq!(session.predictor.stats().visible_wrong, 0);
}

#[test]
fn a_reset_frame_drops_everything() {
    let mut session = Session::new(PredictMode::Always, "$ ");
    session.trust();
    session.type_bytes(b"b");
    session.predictor.frame(&session.screen.grid, true, session.now);
    assert!(!session.predictor.pending());
}

#[test]
fn a_hidden_cursor_is_never_a_prediction_origin() {
    let mut session = Session::new(PredictMode::Always, "$ ");
    session.screen.grid.cursor.visible = false;
    session.type_bytes(b"a");
    assert!(!session.predictor.pending());
}

#[test]
fn mode_names_parse() {
    assert_eq!("auto".parse(), Ok(PredictMode::Auto));
    assert_eq!("Always".parse(), Ok(PredictMode::Always));
    assert_eq!("off".parse(), Ok(PredictMode::Never));
    assert!("sometimes".parse::<PredictMode>().is_err());
    for mode in [PredictMode::Auto, PredictMode::Always, PredictMode::Never] {
        assert_eq!(mode.as_str().parse(), Ok(mode));
    }
}
