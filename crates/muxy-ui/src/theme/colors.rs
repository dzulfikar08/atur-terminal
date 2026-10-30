use super::palette::{ColorScheme, luminance, with_alpha};
use gpui::{Hsla, Rgba, rgb};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appearance {
    Light,
    Dark,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Theme {
    pub bg: Hsla,
    pub fg: Hsla,
    pub fg_muted: Hsla,
    pub fg_dim: Hsla,
    pub surface: Hsla,
    pub border: Hsla,
    pub hover: Hsla,
    pub accent: Hsla,
    pub accent_soft: Hsla,
    pub accent_foreground: Hsla,
    pub warning: Hsla,
    pub danger: Hsla,
    pub diff_add: Hsla,
    pub diff_remove: Hsla,
    pub diff_hunk: Hsla,
}

const FALLBACK_BACKGROUND: u32 = 0x0c_0c_0c;
const FALLBACK_FOREGROUND: u32 = 0xeb_eb_eb;
const FALLBACK_ACCENT: u32 = 0xe4_e4_e4;

impl Theme {
    pub fn from_scheme(theme: &ColorScheme) -> Self {
        let bg = theme.background.unwrap_or(rgb(FALLBACK_BACKGROUND));
        let fg = theme.foreground.unwrap_or(rgb(FALLBACK_FOREGROUND));
        let accent = theme
            .accent
            .or_else(|| theme.palette_color(4))
            .unwrap_or(rgb(FALLBACK_ACCENT));
        let warning = theme.palette_color(3).unwrap_or(rgb(0xd4_b0_62));
        let danger = theme.palette_color(1).unwrap_or(rgb(0xe0_70_70));
        let diff_add = theme.palette_color(2).unwrap_or(rgb(0x6f_bf_8a));
        let diff_hunk = theme.palette_color(6).unwrap_or(accent);

        Self {
            bg: bg.into(),
            fg: fg.into(),
            fg_muted: with_alpha(fg, 0.65).into(),
            fg_dim: with_alpha(fg, 0.4).into(),
            surface: with_alpha(fg, 0.08).into(),
            border: blend(with_alpha(fg, 0.12).into(), bg.into()),
            hover: with_alpha(fg, 0.06).into(),
            accent: accent.into(),
            accent_soft: with_alpha(accent, 0.1).into(),
            accent_foreground: contrasting_foreground(accent).into(),
            warning: warning.into(),
            danger: danger.into(),
            diff_add: diff_add.into(),
            diff_remove: danger.into(),
            diff_hunk: diff_hunk.into(),
        }
    }

    pub fn raised(&self) -> Hsla {
        blend(self.surface, self.bg)
    }

    pub fn border_solid(&self) -> Hsla {
        self.border
    }

    pub fn fg_alpha(&self, alpha: f32) -> Hsla {
        let mut color = self.fg;
        color.a = alpha;
        color
    }
}

fn blend(top: Hsla, bottom: Hsla) -> Hsla {
    let top: Rgba = top.into();
    let bottom: Rgba = bottom.into();
    let mix = |top_channel: f32, bottom_channel: f32| {
        top_channel * top.a + bottom_channel * (1.0 - top.a)
    };
    Rgba {
        r: mix(top.r, bottom.r),
        g: mix(top.g, bottom.g),
        b: mix(top.b, bottom.b),
        a: 1.0,
    }
    .into()
}

pub fn contrasting_foreground(color: Rgba) -> Rgba {
    if luminance(color) > 0.6 {
        rgb(0x00_00_00)
    } else {
        rgb(0xff_ff_ff)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::Assets;
    use std::error::Error;

    const NAMES: [&str; 2] = ["Atur", "Atur Light"];

    fn chroma(color: Rgba) -> f32 {
        color.r.max(color.g).max(color.b) - color.r.min(color.g).min(color.b)
    }

    fn bundled(name: &str) -> Result<ColorScheme, Box<dyn Error>> {
        let (_, source) = Assets::themes()
            .find(|(candidate, _)| *candidate == name)
            .ok_or("theme is bundled")?;
        Ok(ColorScheme::parse(source))
    }

    fn fixed(color: Option<CellColor>) -> Result<Rgba, Box<dyn Error>> {
        match color {
            Some(CellColor::Rgb(color)) => Ok(color),
            _ => Err("a fixed color".into()),
        }
    }

    #[test]
    fn atur_themes_are_neutral() -> Result<(), Box<dyn Error>> {
        for name in NAMES {
            let scheme = bundled(name)?;
            let theme = Theme::from_scheme(&scheme);
            let accent: Rgba = theme.accent.into();
            for color in [
                scheme.background.ok_or("background")?,
                scheme.foreground.ok_or("foreground")?,
                scheme.accent.ok_or("accent")?,
                fixed(scheme.selection_background)?,
                fixed(scheme.selection_foreground)?,
                fixed(scheme.cursor_color)?,
                accent,
            ] {
                assert!(chroma(color) < 0.02, "{name} has a tint: {color:?}");
            }
            let ansi: std::collections::HashSet<u32> = (0..16)
                .filter_map(|index| scheme.palette_color(index).map(u32::from))
                .collect();
            assert_eq!(ansi.len(), 16, "{name} ANSI colors stay distinct");
        }
        Ok(())
    }

    #[test]
    fn atur_ansi_text_colors_are_readable() -> Result<(), Box<dyn Error>> {
        let linear = |channel: f32| {
            if channel <= 0.04045 {
                channel / 12.92
            } else {
                ((channel + 0.055) / 1.055).powf(2.4)
            }
        };
        let relative = |color: Rgba| {
            0.2126 * linear(color.r) + 0.7152 * linear(color.g) + 0.0722 * linear(color.b)
        };
        let ratio = |a: Rgba, b: Rgba| {
            let (high, low) = (relative(a).max(relative(b)), relative(a).min(relative(b)));
            (high + 0.05) / (low + 0.05)
        };
        for name in NAMES {
            let scheme = bundled(name)?;
            let background = scheme.background.ok_or("background")?;
            for index in (1..=6).chain(9..=14) {
                let color = scheme.palette_color(index).ok_or("ANSI color")?;
                assert!(ratio(color, background) >= 3.0, "{name} color {index}");
            }
            let foreground = scheme.foreground.ok_or("foreground")?;
            assert!(ratio(foreground, background) >= 7.0, "{name}");
        }
        Ok(())
    }
}
