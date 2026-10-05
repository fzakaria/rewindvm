//! A log line's colors as the app draws them: the runs a program colored,
//! made bold, dim, italic or underlined on its terminal, as highlights of
//! the line's shown text. `crate::shown_line` reads them out of the
//! program's escape sequences.

use std::ops::Range;

use gpui::{FontStyle, FontWeight, HighlightStyle, UnderlineStyle, px, rgb};

use crate::selection::Mapped;
use crate::shown_line::{AnsiColor, Pen};
use crate::theme;

/// How far a dim run fades toward the panel.
const DIM_FADE: f32 = 0.4;

/// An underline's thickness.
const UNDERLINE_THICKNESS: f32 = 1.0;

/// The levels of red, green and blue along each side of the 256-color
/// cube, colors 16 to 231.
const CUBE_LEVELS: [u32; 6] = [0x00, 0x5f, 0x87, 0xaf, 0xd7, 0xff];
const CUBE_START: u8 = 16;
const CUBE_SIDE: u8 = 6;

/// The 24 greys after the cube, colors 232 to 255, from 8 by tens.
const GREYS_START: u8 = 232;
const GREY_FIRST: u32 = 8;
const GREY_STEP: u32 = 10;

/// `color` in this app's colors: the first 16 from the theme's palette for
/// them, the rest as every terminal draws them.
pub fn ansi_rgb(color: AnsiColor) -> u32 {
    let rgb = |r: u32, g: u32, b: u32| (r << 16) | (g << 8) | b;
    match color {
        AnsiColor::Rgb(r, g, b) => rgb(r.into(), g.into(), b.into()),
        AnsiColor::Indexed(i) if i < CUBE_START => theme::ANSI[usize::from(i)],
        AnsiColor::Indexed(i) if i < GREYS_START => {
            let n = i - CUBE_START;
            let level = |x: u8| CUBE_LEVELS[usize::from(x % CUBE_SIDE)];
            rgb(
                level(n / (CUBE_SIDE * CUBE_SIDE)),
                level(n / CUBE_SIDE),
                level(n),
            )
        }
        AnsiColor::Indexed(i) => {
            let grey = GREY_FIRST + GREY_STEP * u32::from(i - GREYS_START);
            rgb(grey, grey, grey)
        }
    }
}

/// The highlights for `line`'s runs drawn with their own pens, by byte
/// range of its original text.
pub fn penned(line: &Mapped, pens: &[(Range<usize>, Pen)]) -> Vec<(Range<usize>, HighlightStyle)> {
    pens.iter()
        .map(|(range, pen)| {
            let shown = line.shown_offset(range.start)..line.shown_offset(range.end);
            (shown, highlight(pen))
        })
        .filter(|(range, _)| !range.is_empty())
        .collect()
}

fn highlight(pen: &Pen) -> HighlightStyle {
    HighlightStyle {
        color: pen.color.map(|c| rgb(ansi_rgb(c)).into()),
        font_weight: pen.bold.then_some(FontWeight::BOLD),
        font_style: pen.italic.then_some(FontStyle::Italic),
        fade_out: pen.dim.then_some(DIM_FADE),
        underline: pen.underline.then_some(UnderlineStyle {
            thickness: px(UNDERLINE_THICKNESS),
            ..UnderlineStyle::default()
        }),
        ..HighlightStyle::default()
    }
}

#[cfg(test)]
mod tests {
    // Terminal colors in this app's colors, and a pen as a highlight.
    use super::*;

    #[test]
    fn the_16_take_the_theme_s_colors_and_the_rest_the_terminal_s() {
        // Red and green are the theme's; the cube's corners are black and
        // white, its middle is the cube's mix, the greys climb by ten, and
        // a color given by red, green and blue is itself.
        assert_eq!(ansi_rgb(AnsiColor::Indexed(1)), theme::RED);
        assert_eq!(ansi_rgb(AnsiColor::Indexed(2)), theme::GREEN_SOFT);
        assert_eq!(ansi_rgb(AnsiColor::Indexed(16)), 0x000000);
        assert_eq!(ansi_rgb(AnsiColor::Indexed(231)), 0xffffff);
        assert_eq!(ansi_rgb(AnsiColor::Indexed(208)), 0xff8700);
        assert_eq!(ansi_rgb(AnsiColor::Indexed(232)), 0x080808);
        assert_eq!(ansi_rgb(AnsiColor::Indexed(255)), 0xeeeeee);
        assert_eq!(ansi_rgb(AnsiColor::Rgb(0x12, 0x34, 0x56)), 0x123456);
    }

    #[test]
    fn a_pen_becomes_a_highlight_over_the_shown_text() {
        // A bold red run after a store path the line shortens lands on
        // the shortened text's bytes; a dim one fades.
        let original = "/nix/store/0123456789abcdefghijklmnopqrstuv-gcc/bin/gcc: error";
        let line = crate::ui::selectable::mapped(original);
        let red = Pen {
            color: Some(AnsiColor::Indexed(1)),
            bold: true,
            ..Pen::default()
        };
        let at = original.find("error").unwrap();
        let shown = penned(&line, &[(at..at + 5, red)]);
        let (range, style) = &shown[0];
        assert_eq!(&line.shown[range.clone()], "error");
        assert_eq!(style.font_weight, Some(FontWeight::BOLD));
        let dim = Pen {
            dim: true,
            ..Pen::default()
        };
        assert_eq!(highlight(&dim).fade_out, Some(DIM_FADE));
    }
}
