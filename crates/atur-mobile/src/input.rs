//! Terminal input as the app's keyboard produces it (what xterm's `onData` gives), turned into
//! typed text and key presses, so the server encodes arrows, function keys and the rest with the
//! program's own modes (application cursor keys, bracketed paste, and so on) instead of the phone
//! guessing them.

use crate::keys::{Key, Modifiers};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Segment {
    Text(String),
    Key(Key, Modifiers),
}

const NONE: Modifiers = Modifiers {
    shift: false,
    alt: false,
    control: false,
};

/// Splits `data` into text runs and key presses. Bracketed-paste markers are dropped (pasting goes
/// through `Terminal::paste`, which adds its own); unknown control bytes and escape sequences are
/// dropped rather than corrupting the line being typed.
pub(crate) fn segments(data: &str) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut text = String::new();
    let flush = |text: &mut String, out: &mut Vec<Segment>| {
        if !text.is_empty() {
            out.push(Segment::Text(std::mem::take(text)));
        }
    };
    let chars: Vec<char> = data.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        match ch {
            '\x1b' => {
                flush(&mut text, &mut out);
                let (consumed, parsed) = escape(&chars[i + 1..]);
                if let Some(parsed) = parsed {
                    out.push(parsed);
                }
                i += 1 + consumed;
                continue;
            }
            '\r' | '\n' => {
                flush(&mut text, &mut out);
                out.push(Segment::Key(Key::Enter, NONE));
            }
            '\t' => {
                flush(&mut text, &mut out);
                out.push(Segment::Key(Key::Tab, NONE));
            }
            '\x7f' => {
                flush(&mut text, &mut out);
                out.push(Segment::Key(Key::Backspace, NONE));
            }
            '\u{1}'..='\u{1a}' => {
                flush(&mut text, &mut out);
                let letter = char::from(b'a' + (ch as u8) - 1);
                out.push(Segment::Key(
                    Key::Character {
                        text: letter.to_string(),
                    },
                    Modifiers {
                        control: true,
                        ..NONE
                    },
                ));
            }
            c if c.is_control() => flush(&mut text, &mut out),
            c => text.push(c),
        }
        i += 1;
    }
    flush(&mut text, &mut out);
    out
}

/// Reads what follows an ESC: returns how many characters it used and the key, if any.
fn escape(rest: &[char]) -> (usize, Option<Segment>) {
    match rest.first() {
        Some('[') => csi(rest),
        Some('O') => match rest.get(1) {
            Some(&letter) => (2, letter_key(letter, NONE)),
            None => (1, None),
        },
        // Alt with a printable character.
        Some(&c) if !c.is_control() => (
            1,
            Some(Segment::Key(
                Key::Character {
                    text: c.to_string(),
                },
                Modifiers { alt: true, ..NONE },
            )),
        ),
        // A lone ESC, or one followed by something that is its own key.
        _ => (0, Some(Segment::Key(Key::Escape, NONE))),
    }
}

fn csi(rest: &[char]) -> (usize, Option<Segment>) {
    // rest[0] is `[`; parameters are digits, `;`, `:` and `?`, then one final character.
    let mut end = 1;
    while rest
        .get(end)
        .is_some_and(|c| c.is_ascii_digit() || matches!(c, ';' | ':' | '?'))
    {
        end += 1;
    }
    let Some(&last) = rest.get(end) else {
        return (end, None);
    };
    let consumed = end + 1;
    let params: String = rest[1..end].iter().collect();
    let mut numbers = params.split(';').map(|part| part.parse::<u32>().ok());
    let first = numbers.next().flatten();
    let modifiers = numbers.next().flatten().map_or(NONE, decode_modifiers);
    let key = match last {
        '~' => first.and_then(numbered_key),
        'Z' => Some(Key::BackTab),
        'A'..='D' | 'H' | 'F' | 'P'..='S' => return (consumed, letter_key(last, modifiers)),
        _ => None,
    };
    (consumed, key.map(|key| Segment::Key(key, modifiers)))
}

fn decode_modifiers(parameter: u32) -> Modifiers {
    let bits = parameter.saturating_sub(1);
    Modifiers {
        shift: bits & 1 != 0,
        alt: bits & 2 != 0,
        control: bits & 4 != 0,
    }
}

fn letter_key(letter: char, modifiers: Modifiers) -> Option<Segment> {
    let key = match letter {
        'A' => Key::Up,
        'B' => Key::Down,
        'C' => Key::Right,
        'D' => Key::Left,
        'H' => Key::Home,
        'F' => Key::End,
        'P' => Key::Function { number: 1 },
        'Q' => Key::Function { number: 2 },
        'R' => Key::Function { number: 3 },
        'S' => Key::Function { number: 4 },
        _ => return None,
    };
    Some(Segment::Key(key, modifiers))
}

fn numbered_key(number: u32) -> Option<Key> {
    Some(match number {
        1 | 7 => Key::Home,
        2 => Key::Insert,
        3 => Key::Delete,
        4 | 8 => Key::End,
        5 => Key::PageUp,
        6 => Key::PageDown,
        11..=15 => Key::Function {
            number: u8::try_from(number - 10).ok()?,
        },
        17..=21 => Key::Function {
            number: u8::try_from(number - 11).ok()?,
        },
        23 | 24 => Key::Function {
            number: u8::try_from(number - 12).ok()?,
        },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(key: Key) -> Segment {
        Segment::Key(key, NONE)
    }

    fn text(text: &str) -> Segment {
        Segment::Text(text.into())
    }

    #[test]
    fn typing_is_text_and_enter_is_a_key() {
        assert_eq!(segments("ls -la\r"), vec![text("ls -la"), key(Key::Enter)]);
        assert_eq!(segments("héllo 日本"), vec![text("héllo 日本")]);
    }

    #[test]
    fn arrows_and_navigation_become_keys() {
        assert_eq!(
            segments("\x1b[A\x1b[B\x1b[C\x1b[D\x1b[H\x1b[F\x1b[5~\x1b[6~\x1b[3~"),
            vec![
                key(Key::Up),
                key(Key::Down),
                key(Key::Right),
                key(Key::Left),
                key(Key::Home),
                key(Key::End),
                key(Key::PageUp),
                key(Key::PageDown),
                key(Key::Delete),
            ]
        );
        assert_eq!(segments("\x1bOA"), vec![key(Key::Up)]);
    }

    #[test]
    fn modifiers_come_from_the_parameter() {
        assert_eq!(
            segments("\x1b[1;5C"),
            vec![Segment::Key(
                Key::Right,
                Modifiers {
                    control: true,
                    ..NONE
                }
            )]
        );
        assert_eq!(
            segments("\x1b[1;2A"),
            vec![Segment::Key(
                Key::Up,
                Modifiers {
                    shift: true,
                    ..NONE
                }
            )]
        );
    }

    #[test]
    fn function_keys_map_both_forms() {
        assert_eq!(
            segments("\x1bOP\x1b[15~\x1b[17~\x1b[24~"),
            vec![
                key(Key::Function { number: 1 }),
                key(Key::Function { number: 5 }),
                key(Key::Function { number: 6 }),
                key(Key::Function { number: 12 }),
            ]
        );
    }

    #[test]
    fn control_bytes_are_ctrl_letters() {
        assert_eq!(
            segments("\x03"),
            vec![Segment::Key(
                Key::Character { text: "c".into() },
                Modifiers {
                    control: true,
                    ..NONE
                }
            )]
        );
    }

    #[test]
    fn escape_alone_and_alt_letters() {
        assert_eq!(segments("\x1b"), vec![key(Key::Escape)]);
        assert_eq!(
            segments("\x1bb"),
            vec![Segment::Key(
                Key::Character { text: "b".into() },
                Modifiers { alt: true, ..NONE }
            )]
        );
        assert_eq!(
            segments("\x7f\t\x1b[Z"),
            vec![key(Key::Backspace), key(Key::Tab), key(Key::BackTab)]
        );
    }

    #[test]
    fn paste_markers_and_unknown_sequences_are_dropped() {
        assert_eq!(segments("\x1b[200~hi\x1b[201~"), vec![text("hi")]);
        assert_eq!(segments("a\x1b[99xb"), vec![text("a"), text("b")]);
        assert_eq!(segments("a\x00b"), vec![text("a"), text("b")]);
    }
}
