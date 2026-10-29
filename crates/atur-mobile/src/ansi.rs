//! Screen rows as ANSI text, the form the Atur app already draws.
//!
//! The server splits a row into many runs of one style (box drawing comes one cell per run), so a
//! style sequence is written only when the style changes. A row is self-contained: it starts from
//! a reset and ends with one when styled.

use std::fmt::Write as _;

use muxy_protocol::{Color, Run, Style, Underline};

/// Which colour a parameter list is for; the base numbers are the SGR codes of the 8-colour form.
#[derive(Clone, Copy)]
enum Ground {
    Foreground,
    Background,
    Underline,
}

impl Ground {
    const fn base(self) -> u8 {
        match self {
            Self::Foreground => 30,
            Self::Background => 40,
            Self::Underline => 58,
        }
    }
}

fn color(out: &mut String, color: Color, ground: Ground) {
    let base = ground.base();
    let extended = if matches!(ground, Ground::Underline) {
        base
    } else {
        base + 8
    };
    // Writing to a String cannot fail.
    match color {
        Color::Default => {}
        Color::Indexed(index) => match ground {
            Ground::Underline => {
                let _ = write!(out, ";{extended};5;{index}");
            }
            _ if index < 8 => {
                let _ = write!(out, ";{}", u16::from(base) + u16::from(index));
            }
            _ if index < 16 => {
                let _ = write!(out, ";{}", u16::from(base) + 60 + u16::from(index) - 8);
            }
            _ => {
                let _ = write!(out, ";{extended};5;{index}");
            }
        },
        Color::Rgb(red, green, blue) => {
            let _ = write!(out, ";{extended};2;{red};{green};{blue}");
        }
    }
}

/// SGR parameters of a style, each preceded by `;`; empty for the default style.
fn parameters(style: &Style) -> String {
    let mut out = String::new();
    if style.bold {
        out.push_str(";1");
    }
    if style.faint {
        out.push_str(";2");
    }
    if style.italic {
        out.push_str(";3");
    }
    let underline = match style.underline {
        Underline::None => None,
        Underline::Single => Some("4"),
        Underline::Double => Some("4:2"),
        Underline::Curly => Some("4:3"),
        Underline::Dotted => Some("4:4"),
        Underline::Dashed => Some("4:5"),
    };
    if let Some(underline) = underline {
        out.push(';');
        out.push_str(underline);
    }
    if style.inverse {
        out.push_str(";7");
    }
    if style.invisible {
        out.push_str(";8");
    }
    if style.strikethrough {
        out.push_str(";9");
    }
    if style.overline {
        out.push_str(";53");
    }
    color(&mut out, style.fg, Ground::Foreground);
    color(&mut out, style.bg, Ground::Background);
    if underline.is_some() {
        color(&mut out, style.underline_color, Ground::Underline);
    }
    out
}

/// One screen row as ANSI.
pub(crate) fn row(runs: &[Run]) -> String {
    let mut out = String::new();
    let mut current = String::new();
    for run in runs {
        let sgr = parameters(&run.style);
        if sgr != current {
            if sgr.is_empty() {
                out.push_str("\x1b[0m");
            } else {
                out.push_str("\x1b[0");
                out.push_str(&sgr);
                out.push('m');
            }
            current = sgr;
        }
        out.push_str(&run.text);
    }
    if !current.is_empty() {
        out.push_str("\x1b[0m");
    }
    out
}

#[cfg(test)]
mod tests {
    use muxy_protocol::Underline;

    use super::*;

    fn run(text: &str, style: Style) -> Run {
        Run {
            text: text.into(),
            width: u16::try_from(text.chars().count()).unwrap_or(0),
            style,
        }
    }

    #[test]
    fn plain_text_has_no_sequences() {
        assert_eq!(row(&[run("ls -la", Style::default())]), "ls -la");
    }

    #[test]
    fn style_changes_write_one_sequence_each() {
        let red = Style {
            fg: Color::Indexed(1),
            bold: true,
            ..Style::default()
        };
        let line = row(&[
            run("a", red),
            run("b", red),
            run("c", Style::default()),
            run(
                "d",
                Style {
                    bg: Color::Rgb(1, 2, 3),
                    ..Style::default()
                },
            ),
        ]);
        assert_eq!(line, "\x1b[0;1;31mab\x1b[0mc\x1b[0;48;2;1;2;3md\x1b[0m");
    }

    #[test]
    fn bright_and_palette_colors_use_their_own_forms() {
        let bright = Style {
            fg: Color::Indexed(9),
            bg: Color::Indexed(200),
            ..Style::default()
        };
        assert_eq!(row(&[run("x", bright)]), "\x1b[0;91;48;5;200mx\x1b[0m");
    }

    #[test]
    fn underline_carries_its_color() {
        let curly = Style {
            underline: Underline::Curly,
            underline_color: Color::Rgb(9, 8, 7),
            ..Style::default()
        };
        assert_eq!(row(&[run("x", curly)]), "\x1b[0;4:3;58;2;9;8;7mx\x1b[0m");
    }
}
