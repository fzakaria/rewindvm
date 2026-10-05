//! Sideways scrolling of a text area whose lines do not wrap, the source
//! panel's and the file viewer's: the lines move left together past a
//! line-number gutter that stays put, as far as the widest line needs to
//! show its end.

use crate::theme::size;
use crate::viewer::TAB_WIDTH;

/// How far a text area's lines are scrolled left, in pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Sideways {
    offset: f32,
}

impl Sideways {
    /// The offset to draw the lines at: the one scrolled to, kept within
    /// what lines `widest` pixels wide in a column `visible` pixels wide
    /// allow, as after the text or the window changed.
    pub fn offset(self, widest: f32, visible: f32) -> f32 {
        self.offset.clamp(0.0, furthest(widest, visible))
    }

    /// Scrolls `pixels` toward the lines' ends, or back toward their
    /// starts when negative, kept between the start and where the widest
    /// line's end meets the column's right edge.
    pub fn scroll_by(&mut self, pixels: f32, widest: f32, visible: f32) {
        let from = self.offset(widest, visible);
        self.offset = (from + pixels).clamp(0.0, furthest(widest, visible));
    }
}

/// The furthest lines `widest` pixels wide scroll in a column `visible`
/// pixels wide.
fn furthest(widest: f32, visible: f32) -> f32 {
    (widest - visible).max(0.0)
}

/// The characters in the widest of `lines`, with each tab drawn as
/// TAB_WIDTH spaces.
pub fn widest_line(lines: &[String]) -> usize {
    let chars = |line: &String| {
        let tabs = line.matches('\t').count();
        line.chars().count() + tabs * (TAB_WIDTH - 1)
    };
    lines.iter().map(chars).max().unwrap_or(0)
}

/// The width of a line of `chars` characters in the monospace font.
pub fn line_width(chars: usize) -> f32 {
    chars as f32 * size::MONO_CHAR_WIDTH
}

/// The width of the text column of a row `row_width` pixels wide whose
/// gutter holds line numbers of `digits` digits; a row of no digits has no
/// gutter.
pub fn text_column(row_width: f32, digits: usize) -> f32 {
    let padding = 2.0 * size::PANEL_PAD_X;
    let gutter = match digits {
        0 => 0.0,
        _ => digits as f32 * size::MONO_CHAR_WIDTH + size::LOG_COLUMN_GAP,
    };
    (row_width - padding - gutter).max(0.0)
}

#[cfg(test)]
mod tests {
    // Offsets kept in range as the area scrolls and as its text and width
    // change, and the widths of lines and of the text column they scroll
    // in, from numbers and short lines given by hand.
    use super::*;

    /// Scrolling right stops where the widest line's end meets the
    /// column's edge, scrolling left stops at the start, and lines that
    /// fit do not scroll. A 1,000-pixel line in a 300-pixel column.
    #[test]
    fn scrolling_stops_at_the_start_and_at_the_widest_line_s_end() {
        let mut sideways = Sideways::default();
        sideways.scroll_by(200.0, 1000.0, 300.0);
        assert_eq!(sideways.offset(1000.0, 300.0), 200.0);
        sideways.scroll_by(900.0, 1000.0, 300.0);
        assert_eq!(sideways.offset(1000.0, 300.0), 700.0);
        sideways.scroll_by(-1000.0, 1000.0, 300.0);
        assert_eq!(sideways.offset(1000.0, 300.0), 0.0);

        let mut fits = Sideways::default();
        fits.scroll_by(50.0, 200.0, 300.0);
        assert_eq!(fits.offset(200.0, 300.0), 0.0);
    }

    /// An offset scrolled to stays drawn within range when the text gets
    /// narrower or the column wider, and comes back when they return, as
    /// the viewer's file does when the playhead moves.
    #[test]
    fn the_offset_drawn_follows_the_text_and_the_column() {
        let mut sideways = Sideways::default();
        sideways.scroll_by(700.0, 1000.0, 300.0);
        assert_eq!(sideways.offset(500.0, 300.0), 200.0);
        assert_eq!(sideways.offset(1000.0, 600.0), 400.0);
        assert_eq!(sideways.offset(500.0, 600.0), 0.0);
        assert_eq!(sideways.offset(1000.0, 300.0), 700.0);

        // A scroll from an offset out of range starts from the one drawn.
        sideways.scroll_by(-10.0, 500.0, 300.0);
        assert_eq!(sideways.offset(500.0, 300.0), 190.0);
    }

    /// The widest line counts characters, not bytes, with each tab as
    /// the spaces it is drawn as.
    #[test]
    fn the_widest_line_counts_tabs_as_drawn() {
        let lines: Vec<String> = ["ab", "\tx", "\u{65e5}\u{672c}"]
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(widest_line(&lines), TAB_WIDTH + 1);
        assert_eq!(widest_line(&[]), 0);
        assert_eq!(line_width(10), 10.0 * size::MONO_CHAR_WIDTH);
    }

    /// The text column is the row less its padding, its gutter of line
    /// numbers and the gap after the gutter; a row without numbers, as a
    /// hex dump's, has no gutter.
    #[test]
    fn the_text_column_is_the_row_less_the_gutter() {
        let row = 400.0;
        let expected =
            row - 2.0 * size::PANEL_PAD_X - 3.0 * size::MONO_CHAR_WIDTH - size::LOG_COLUMN_GAP;
        assert_eq!(text_column(row, 3), expected);
        assert_eq!(text_column(10.0, 3), 0.0);
        assert_eq!(text_column(row, 0), row - 2.0 * size::PANEL_PAD_X);
    }
}
