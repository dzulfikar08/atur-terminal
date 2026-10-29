use super::*;
use std::time::{Duration, Instant};

fn pane(window: &mut Window, cx: &mut Context<TerminalPane>) -> TerminalPane {
    let mut terminal = muxy_app_core::settings::TerminalSettings::default();
    terminal.options.local_echo = muxy_app_core::settings::LocalEcho::Always;
    let mut pane = TerminalPane::new(Palette::new(true), terminal, cx);
    prepare_mouse(&mut pane);
    pane.focus.focus(window);
    pane
}

fn echo(pane: &mut TerminalPane, text: &str, cursor: u16, cx: &mut Context<TerminalPane>) {
    let frame = ScreenFrame {
        size: pane.grid.as_ref().unwrap().size,
        graphics: None,
        seq: 2,
        reset: false,
        rows: vec![row(2, text)],
        cursor: Cursor {
            shape: muxy_protocol::CursorShape::default(),
            row: 2,
            col: cursor,
            visible: true,
        },
        modes: Modes::default(),
    };
    pane.apply(&frame, cx);
}

fn pause() {
    std::thread::sleep(Duration::from_millis(60));
}

#[gpui::test]
fn typing_is_guessed_after_the_server_proved_it_echoes_and_rolled_back_when_wrong(
    cx: &mut TestAppContext,
) {
    let (pane, cx) = cx.add_window_view(pane);
    pane.update(cx, |pane, cx| {
        // The first key of a fresh epoch is checked silently.
        pane.send_paste(b"a", cx);
        assert!(pane.predict.cells(Instant::now()).is_empty());
        pause();
        echo(pane, "prompta", 7, cx);
        assert!(!pane.predict.pending(), "confirmed by the frame");
        pause();
        // Now typing shows at once, at the cursor.
        pane.send_paste(b"b", cx);
        let cells = pane.predict.cells(Instant::now());
        assert_eq!((cells.len(), cells[0].row, cells[0].col), (1, 2, 7));
        assert_eq!(cells[0].text, "b");
        assert_eq!(pane.predict.cursor(Instant::now()), Some((2, 8)));
        // The program wrote something else there.
        pause();
        echo(pane, "promptaZ", 8, cx);
        pause();
        pane.tick_echo(cx);
        assert!(pane.predict.cells(Instant::now()).is_empty());
        assert_eq!(pane.predict.stats().visible_wrong, 1);
    });
}

#[gpui::test]
fn pasted_text_and_the_never_setting_are_not_guessed(cx: &mut TestAppContext) {
    let (pane, cx) = cx.add_window_view(pane);
    pane.update(cx, |pane, cx| {
        pane.send_paste(b"a", cx);
        pause();
        echo(pane, "prompta", 7, cx);
        pause();
        pane.send_paste(b"0123456789", cx);
        assert!(pane.predict.cells(Instant::now()).is_empty());
        pane.terminal.options.local_echo = muxy_app_core::settings::LocalEcho::Never;
        pane.tick_echo(cx);
        assert_eq!(pane.predict.mode(), PredictMode::Never);
    });
}
