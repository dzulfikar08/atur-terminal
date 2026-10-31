//! Draws a terminal screen as a PNG, for agents that can read an image but not
//! the server's cell grid. Text uses a built-in 8x16 bitmap font, so it needs
//! no fonts installed on the computer that runs it; characters outside the
//! font appear as hollow boxes.

use image::{ExtendedColorType, ImageEncoder, ImageError, codecs::png::PngEncoder};
use muxy_protocol::{Color, Run, Underline};

#[rustfmt::skip]
mod font;

const CELL_WIDTH: usize = font::WIDTH;
const CELL_HEIGHT: usize = font::HEIGHT;
const MAX_PIXELS: usize = 100_000_000;

const DEFAULT_FG: [u8; 3] = [0xd4, 0xd4, 0xd4];
const DEFAULT_BG: [u8; 3] = [0x1e, 0x1e, 0x1e];
const ANSI: [[u8; 3]; 16] = [
    [0x00, 0x00, 0x00],
    [0xcd, 0x31, 0x31],
    [0x0d, 0xbc, 0x79],
    [0xe5, 0xe5, 0x10],
    [0x24, 0x72, 0xc8],
    [0xbc, 0x3f, 0xbc],
    [0x11, 0xa8, 0xcd],
    [0xe5, 0xe5, 0xe5],
    [0x66, 0x66, 0x66],
    [0xf1, 0x4c, 0x4c],
    [0x23, 0xd1, 0x8b],
    [0xf5, 0xf5, 0x43],
    [0x3b, 0x8e, 0xea],
    [0xd6, 0x70, 0xd6],
    [0x29, 0xb8, 0xdb],
    [0xff, 0xff, 0xff],
];

pub(super) struct Shot {
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) png: Vec<u8>,
}

#[derive(Debug)]
pub(super) enum ShotError {
    TooLarge,
    Encode(ImageError),
}

impl std::fmt::Display for ShotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge => f.write_str("the screen is too large to draw"),
            Self::Encode(error) => write!(f, "could not encode the PNG: {error}"),
        }
    }
}

impl std::error::Error for ShotError {}

/// The screen's `rows` of runs as a PNG `cols` cells wide. `cursor` is the
/// visible cursor's (row, column), outlined in the foreground color.
pub(super) fn render(
    rows: &[Vec<Run>],
    cols: u16,
    cursor: Option<(u16, u16)>,
) -> Result<Shot, ShotError> {
    let cols = usize::from(cols).max(1);
    let width = cols * CELL_WIDTH;
    let height = rows.len().max(1) * CELL_HEIGHT;
    if width.saturating_mul(height) > MAX_PIXELS {
        return Err(ShotError::TooLarge);
    }
    let mut canvas = Canvas {
        width,
        pixels: DEFAULT_BG.repeat(width * height),
    };
    for (row, runs) in rows.iter().enumerate() {
        let mut column = 0;
        for run in runs {
            let start = column;
            column = start + usize::from(run.width);
            draw_run(&mut canvas, row, start, cols, run);
        }
    }
    if let Some((row, column)) = cursor
        && usize::from(row) < rows.len()
        && usize::from(column) < cols
    {
        canvas.outline(
            usize::from(column) * CELL_WIDTH,
            usize::from(row) * CELL_HEIGHT,
            CELL_WIDTH,
            CELL_HEIGHT,
            DEFAULT_FG,
        );
    }
    let (w, h) = (
        u32::try_from(width).map_err(|_| ShotError::TooLarge)?,
        u32::try_from(height).map_err(|_| ShotError::TooLarge)?,
    );
    let mut png = Vec::new();
    PngEncoder::new(&mut png)
        .write_image(&canvas.pixels, w, h, ExtendedColorType::Rgb8)
        .map_err(ShotError::Encode)?;
    Ok(Shot {
        width: w,
        height: h,
        png,
    })
}

struct Canvas {
    width: usize,
    pixels: Vec<u8>,
}

impl Canvas {
    fn set(&mut self, x: usize, y: usize, color: [u8; 3]) {
        let offset = (y * self.width + x) * 3;
        if x < self.width
            && let Some(pixel) = self.pixels.get_mut(offset..offset + 3)
        {
            pixel.copy_from_slice(&color);
        }
    }

    fn fill(&mut self, x: usize, y: usize, width: usize, height: usize, color: [u8; 3]) {
        for row in y..y + height {
            for column in x..(x + width).min(self.width) {
                self.set(column, row, color);
            }
        }
    }

    fn outline(&mut self, x: usize, y: usize, width: usize, height: usize, color: [u8; 3]) {
        self.fill(x, y, width, 1, color);
        self.fill(x, y + height - 1, width, 1, color);
        self.fill(x, y, 1, height, color);
        self.fill(x + width - 1, y, 1, height, color);
    }
}

fn draw_run(canvas: &mut Canvas, row: usize, start: usize, cols: usize, run: &Run) {
    let style = run.style;
    let mut fg = color(style.fg, DEFAULT_FG, style.bold);
    let mut bg = color(style.bg, DEFAULT_BG, false);
    if style.inverse {
        std::mem::swap(&mut fg, &mut bg);
    }
    if style.faint {
        fg = mix(fg, bg);
    }
    let y = row * CELL_HEIGHT;
    let end = (start + usize::from(run.width)).min(cols);
    if start >= end {
        return;
    }
    canvas.fill(
        start * CELL_WIDTH,
        y,
        (end - start) * CELL_WIDTH,
        CELL_HEIGHT,
        bg,
    );
    let mut column = start;
    for character in run.text.chars() {
        let cells = cell_width(character);
        if cells == 0 {
            continue;
        }
        if column >= end {
            break;
        }
        if !style.invisible && character != ' ' {
            glyph(
                canvas,
                column * CELL_WIDTH,
                y,
                character,
                cells,
                fg,
                style.bold,
            );
        }
        column += cells;
    }
    let x = start * CELL_WIDTH;
    let span = (end - start) * CELL_WIDTH;
    let line = match style.underline_color {
        Color::Default => fg,
        other => color(other, fg, false),
    };
    match style.underline {
        Underline::None => (),
        Underline::Double => {
            canvas.fill(x, y + CELL_HEIGHT - 4, span, 1, line);
            canvas.fill(x, y + CELL_HEIGHT - 2, span, 1, line);
        }
        _ => canvas.fill(x, y + CELL_HEIGHT - 3, span, 1, line),
    }
    if style.strikethrough {
        canvas.fill(x, y + CELL_HEIGHT / 2, span, 1, fg);
    }
    if style.overline {
        canvas.fill(x, y, span, 1, fg);
    }
}

fn glyph(
    canvas: &mut Canvas,
    x: usize,
    y: usize,
    character: char,
    cells: usize,
    fg: [u8; 3],
    bold: bool,
) {
    if let Some(shade) = shade(character) {
        for row in 0..CELL_HEIGHT {
            for column in 0..CELL_WIDTH {
                if shade(column, row) {
                    canvas.set(x + column, y + row, fg);
                }
            }
        }
        return;
    }
    let bits = (cells == 1)
        .then(|| {
            font::GLYPHS
                .binary_search_by_key(&u32::from(character), |(code, _)| *code)
                .ok()
                .map(|index| &font::GLYPHS[index].1)
        })
        .flatten();
    let Some(bits) = bits else {
        canvas.outline(x + 1, y + 2, cells * CELL_WIDTH - 2, CELL_HEIGHT - 4, fg);
        return;
    };
    for (row, line) in bits.iter().enumerate() {
        for column in 0..CELL_WIDTH {
            if line & (0x80 >> column) != 0 {
                canvas.set(x + column, y + row, fg);
                if bold && column + 1 < CELL_WIDTH {
                    canvas.set(x + column + 1, y + row, fg);
                }
            }
        }
    }
}

/// The dither of the three shade blocks, which no font rasterizes evenly.
fn shade(character: char) -> Option<fn(usize, usize) -> bool> {
    match character {
        '\u{2591}' => Some(|x, y| x % 4 == 0 && y % 2 == 0),
        '\u{2592}' => Some(|x, y| (x + y) % 2 == 0),
        '\u{2593}' => Some(|x, y| !(x % 4 == 0 && y % 2 == 0)),
        _ => None,
    }
}

/// How many cells `character` covers: 0 for combining marks, 2 for East Asian
/// wide characters and emoji, otherwise 1.
fn cell_width(character: char) -> usize {
    match u32::from(character) {
        0x0300..=0x036f | 0x200b..=0x200f | 0xfe00..=0xfe0f => 0,
        0x1100..=0x115f
        | 0x2e80..=0xa4cf
        | 0xac00..=0xd7a3
        | 0xf900..=0xfaff
        | 0xfe30..=0xfe6f
        | 0xff00..=0xff60
        | 0xffe0..=0xffe6
        | 0x1f300..=0x1f64f
        | 0x1f900..=0x1f9ff
        | 0x20000..=0x3fffd => 2,
        _ => 1,
    }
}

fn color(color: Color, default: [u8; 3], bold: bool) -> [u8; 3] {
    match color {
        Color::Default => default,
        Color::Indexed(index) => palette(if bold && index < 8 { index + 8 } else { index }),
        Color::Rgb(r, g, b) => [r, g, b],
    }
}

fn palette(index: u8) -> [u8; 3] {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    match index {
        0..=15 => ANSI[usize::from(index)],
        16..=231 => {
            let cube = usize::from(index - 16);
            [LEVELS[cube / 36], LEVELS[cube / 6 % 6], LEVELS[cube % 6]]
        }
        _ => {
            let gray = 8 + 10 * (index - 232);
            [gray; 3]
        }
    }
}

fn mix(foreground: [u8; 3], background: [u8; 3]) -> [u8; 3] {
    [0, 1, 2].map(|i| u8::midpoint(foreground[i], background[i]))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use muxy_protocol::Style;

    fn run(text: &str, width: u16, style: Style) -> Run {
        Run {
            text: text.into(),
            width,
            style,
        }
    }

    fn decode(shot: &Shot) -> image::RgbImage {
        image::load_from_memory(&shot.png).unwrap().to_rgb8()
    }

    fn lit(image: &image::RgbImage, column: u32, row: u32, color: [u8; 3]) -> usize {
        let (w, h) = (
            u32::try_from(CELL_WIDTH).unwrap(),
            u32::try_from(CELL_HEIGHT).unwrap(),
        );
        (0..h)
            .flat_map(|y| (0..w).map(move |x| (x, y)))
            .filter(|(x, y)| image.get_pixel(column * w + x, row * h + y).0 == color)
            .count()
    }

    #[test]
    fn the_image_is_one_cell_per_column_and_row_with_each_run_in_its_own_colors() {
        let red = Style {
            bg: Color::Rgb(200, 0, 0),
            fg: Color::Rgb(0, 255, 0),
            ..Style::default()
        };
        let rows = vec![
            vec![run("A", 1, red), run("b", 1, Style::default())],
            vec![run("", 2, Style::default())],
        ];
        let shot = render(&rows, 3, None).unwrap();
        assert_eq!((shot.width, shot.height), (24, 32));
        let image = decode(&shot);
        assert_eq!(image.dimensions(), (24, 32));
        assert!(
            lit(&image, 0, 0, [0, 255, 0]) > 10,
            "A draws in its foreground"
        );
        assert_eq!(
            image.get_pixel(0, 0).0,
            [200, 0, 0],
            "A's cell has its background"
        );
        assert!(
            lit(&image, 1, 0, DEFAULT_FG) > 10,
            "b uses the default colors"
        );
        assert_eq!(
            lit(&image, 2, 0, DEFAULT_FG),
            0,
            "past the last run is blank"
        );
        assert_eq!(lit(&image, 0, 1, DEFAULT_FG), 0);
    }

    #[test]
    fn inverse_swaps_colors_and_invisible_text_leaves_only_the_background() {
        let inverse = Style {
            inverse: true,
            ..Style::default()
        };
        let hidden = Style {
            invisible: true,
            ..Style::default()
        };
        let rows = vec![vec![run("M", 1, inverse), run("M", 1, hidden)]];
        let image = decode(&render(&rows, 2, None).unwrap());
        assert_eq!(
            image.get_pixel(0, 0).0,
            DEFAULT_FG,
            "inverse fills with the foreground"
        );
        assert!(
            lit(&image, 0, 0, DEFAULT_BG) > 10,
            "inverse text is drawn in the background"
        );
        assert_eq!(lit(&image, 1, 0, DEFAULT_FG), 0);
    }

    #[test]
    fn wide_and_unknown_characters_get_a_box_and_the_cursor_is_outlined() {
        let rows = vec![vec![run("世", 2, Style::default())]];
        let image = decode(&render(&rows, 2, Some((0, 1))).unwrap());
        assert!(
            lit(&image, 0, 0, DEFAULT_FG) > 20,
            "a wide character is drawn as a box"
        );
        assert_eq!(
            image.get_pixel(8, 0).0,
            DEFAULT_FG,
            "the cursor cell has an outline"
        );
        assert_eq!(image.get_pixel(8 + 3, 8).0, DEFAULT_BG);
    }

    #[test]
    fn a_screen_beyond_the_pixel_budget_is_refused() {
        let rows = vec![Vec::new(); 3000];
        assert!(matches!(
            render(&rows, 5000, None),
            Err(ShotError::TooLarge)
        ));
    }

    #[test]
    fn indexed_colors_follow_the_xterm_cube_and_gray_ramp() {
        assert_eq!(palette(1), ANSI[1]);
        assert_eq!(palette(16), [0, 0, 0]);
        assert_eq!(palette(196), [255, 0, 0]);
        assert_eq!(palette(231), [255, 255, 255]);
        assert_eq!(palette(232), [8, 8, 8]);
        assert_eq!(
            color(Color::Indexed(1), DEFAULT_FG, true),
            ANSI[9],
            "bold brightens"
        );
    }
}
