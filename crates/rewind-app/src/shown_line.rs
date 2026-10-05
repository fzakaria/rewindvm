//! A line of output as a terminal shows it. A program that writes to a
//! terminal, as a Nix builder does, may color its text with escape
//! sequences, go back to the start of the line with a carriage return and
//! write over it, as progress counters do, or erase the rest of the line.
//! The log shows what the terminal would: the text left on the line, in
//! the colors and weights the program drew each part in.

use std::ops::Range;

/// The final characters of the control sequences that erase the line from
/// the cursor on, `ESC [ K`, and set the pen, `ESC [ m`.
const ERASE_LINE: char = 'K';
const SET_PEN: char = 'm';

/// The control characters a log line acts on: back to the start of the
/// line, back one character, and a tab.
const CARRIAGE_RETURN: u8 = b'\r';
const BACKSPACE: u8 = 0x08;
const TAB: u8 = b'\t';

/// A color a program asked for: one of the terminal's 256, the first 16
/// of which each theme draws its own way, or red, green and blue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnsiColor {
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// How a terminal draws a run of text, as SGR sequences set it. The
/// default pen draws in the line's own color.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pen {
    pub color: Option<AnsiColor>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
}

/// A line as a terminal shows it: its text, and the runs of it drawn with
/// a pen other than the default, by byte range of the text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShownLine {
    pub text: String,
    pub pens: Vec<(Range<usize>, Pen)>,
}

/// What a terminal shows of `text`, one line of a program's output.
/// vte, the parser alacritty's terminal uses, reads the escape sequences,
/// so every kind is taken out whole; of what they do, only what a single
/// line can show is kept.
pub fn shown(text: &str) -> ShownLine {
    let mut terminal = LineTerminal::default();
    vte::Parser::new().advance(&mut terminal, text.as_bytes());
    runs(&terminal.line)
}

/// One line of a terminal: what is on it, with the pen each character was
/// drawn with, the cursor, and the pen.
#[derive(Default)]
struct LineTerminal {
    line: Vec<(char, Pen)>,
    cursor: usize,
    pen: Pen,
}

impl vte::Perform for LineTerminal {
    fn print(&mut self, c: char) {
        put(&mut self.line, &mut self.cursor, c, self.pen);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            CARRIAGE_RETURN => self.cursor = 0,
            BACKSPACE => self.cursor = self.cursor.saturating_sub(1),
            TAB => put(&mut self.line, &mut self.cursor, '\t', self.pen),
            _ => {}
        }
    }

    /// Erasing the line cuts it at the cursor and setting the pen colors
    /// what follows; every other sequence, such as a private mode or a
    /// cursor move to another line, changes nothing a line shows.
    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        ignore: bool,
        action: char,
    ) {
        if ignore || !intermediates.is_empty() {
            return;
        }
        let codes: Vec<u16> = params.iter().flatten().copied().collect();
        match action {
            ERASE_LINE => erase(&mut self.line, self.cursor, codes.first().copied()),
            SET_PEN => set_pen(&mut self.pen, &codes),
            _ => {}
        }
    }
}

/// Writes `c` with `pen` at the cursor, over what is there, and moves
/// past it.
fn put(line: &mut Vec<(char, Pen)>, cursor: &mut usize, c: char, pen: Pen) {
    match line.get_mut(*cursor) {
        Some(at) => *at = (c, pen),
        None => line.push((c, pen)),
    }
    *cursor += 1;
}

/// What `ESC [ K` erases, by its parameter.
mod erase {
    pub const TO_START: u16 = 1;
    pub const WHOLE_LINE: u16 = 2;
}

/// `ESC [ K` with its parameter: 0 or none erases from the cursor to the
/// end, 1 from the start to the cursor, 2 the whole line.
fn erase(line: &mut Vec<(char, Pen)>, cursor: usize, what: Option<u16>) {
    match what {
        Some(erase::TO_START) => {
            for c in line.iter_mut().take(cursor + 1) {
                *c = (' ', Pen::default());
            }
        }
        Some(erase::WHOLE_LINE) => line.clear(),
        _ => line.truncate(cursor),
    }
}

/// SGR codes: what `ESC [ ... m` sets.
mod sgr {
    pub const RESET: u16 = 0;
    pub const BOLD: u16 = 1;
    pub const DIM: u16 = 2;
    pub const ITALIC: u16 = 3;
    pub const UNDERLINE: u16 = 4;
    pub const NORMAL_INTENSITY: u16 = 22;
    pub const NOT_ITALIC: u16 = 23;
    pub const NOT_UNDERLINED: u16 = 24;
    pub const FOREGROUND: std::ops::RangeInclusive<u16> = 30..=37;
    pub const EXTENDED_FOREGROUND: u16 = 38;
    pub const DEFAULT_FOREGROUND: u16 = 39;
    pub const EXTENDED_BACKGROUND: u16 = 48;
    pub const BRIGHT_FOREGROUND: std::ops::RangeInclusive<u16> = 90..=97;
    /// After 38 or 48: a color of the 256, or red, green and blue.
    pub const INDEXED: u16 = 5;
    pub const RGB: u16 = 2;
    /// Where the bright colors start among the 16.
    pub const BRIGHT_OFFSET: u8 = 8;
}

/// Applies `ESC [ codes m` to `pen`. Background colors, which a log has
/// no use for, are read past.
fn set_pen(pen: &mut Pen, codes: &[u16]) {
    if codes.is_empty() {
        *pen = Pen::default();
        return;
    }
    let mut codes = codes.iter().copied();
    while let Some(code) = codes.next() {
        match code {
            sgr::RESET => *pen = Pen::default(),
            sgr::BOLD => pen.bold = true,
            sgr::DIM => pen.dim = true,
            sgr::ITALIC => pen.italic = true,
            sgr::UNDERLINE => pen.underline = true,
            sgr::NORMAL_INTENSITY => (pen.bold, pen.dim) = (false, false),
            sgr::NOT_ITALIC => pen.italic = false,
            sgr::NOT_UNDERLINED => pen.underline = false,
            sgr::DEFAULT_FOREGROUND => pen.color = None,
            c if sgr::FOREGROUND.contains(&c) => {
                pen.color = Some(AnsiColor::Indexed((c - 30) as u8));
            }
            c if sgr::BRIGHT_FOREGROUND.contains(&c) => {
                pen.color = Some(AnsiColor::Indexed((c - 90) as u8 + sgr::BRIGHT_OFFSET));
            }
            sgr::EXTENDED_FOREGROUND => pen.color = extended(&mut codes),
            sgr::EXTENDED_BACKGROUND => {
                extended(&mut codes);
            }
            _ => {}
        }
    }
}

/// The color after 38 or 48: `5;n` or `2;r;g;b`.
fn extended(codes: &mut impl Iterator<Item = u16>) -> Option<AnsiColor> {
    let byte = |c: Option<u16>| c.and_then(|c| u8::try_from(c).ok());
    match codes.next()? {
        sgr::INDEXED => Some(AnsiColor::Indexed(byte(codes.next())?)),
        sgr::RGB => {
            let (r, g, b) = (codes.next(), codes.next(), codes.next());
            Some(AnsiColor::Rgb(byte(r)?, byte(g)?, byte(b)?))
        }
        _ => None,
    }
}

/// The line's text, and its runs of one pen other than the default.
fn runs(line: &[(char, Pen)]) -> ShownLine {
    let mut shown = ShownLine::default();
    for &(c, pen) in line {
        let start = shown.text.len();
        shown.text.push(c);
        let end = shown.text.len();
        if pen == Pen::default() {
            continue;
        }
        match shown.pens.last_mut() {
            Some((range, last)) if *last == pen && range.end == start => range.end = end,
            _ => shown.pens.push((start..end, pen)),
        }
    }
    shown
}

#[cfg(test)]
mod tests {
    // Lines with the sequences programs write to a terminal, and what a
    // terminal leaves on screen of each.
    use super::*;

    fn text(line: &str) -> String {
        shown(line).text
    }

    #[test]
    fn colors_leave_the_text_and_mark_its_runs() {
        // meson's bold green YES, and gcc's bold location and red error:
        // the text without the sequences, and a run for each pen.
        let yes = shown("found: \u{1b}[1;32mYES\u{1b}[0m 1.17.0");
        assert_eq!(yes.text, "found: YES 1.17.0");
        let green = Pen {
            color: Some(AnsiColor::Indexed(2)),
            bold: true,
            ..Pen::default()
        };
        assert_eq!(yes.pens, vec![(7..10, green)]);

        let error =
            shown("\u{1b}[01m\u{1b}[Kpool.c:77:\u{1b}[m\u{1b}[K \u{1b}[01;31merror:\u{1b}[m bad");
        assert_eq!(error.text, "pool.c:77: error: bad");
        let bold = Pen {
            bold: true,
            ..Pen::default()
        };
        let red = Pen {
            color: Some(AnsiColor::Indexed(1)),
            ..bold
        };
        assert_eq!(error.pens, vec![(0..10, bold), (11..17, red)]);
    }

    #[test]
    fn bright_256_and_rgb_colors_are_told_apart() {
        // A bright color is one of the 16, 38;5;n one of the 256, and
        // 38;2;r;g;b a color of its own; a background color changes
        // nothing, and 39 goes back to the line's own color.
        let pen = |line: &str| shown(line).pens.first().map(|(_, p)| p.color);
        assert_eq!(pen("\u{1b}[91mx"), Some(Some(AnsiColor::Indexed(9))));
        assert_eq!(
            pen("\u{1b}[38;5;208mx"),
            Some(Some(AnsiColor::Indexed(208)))
        );
        assert_eq!(
            pen("\u{1b}[38;2;10;20;30mx"),
            Some(Some(AnsiColor::Rgb(10, 20, 30)))
        );
        assert_eq!(pen("\u{1b}[48;5;1mx"), None);
        assert_eq!(pen("\u{1b}[31m\u{1b}[39mx"), None);
    }

    #[test]
    fn a_carriage_return_writes_over_the_line() {
        // A counter rewritten in place leaves its last value; a shorter
        // write over a longer one leaves the longer one's tail, unless the
        // line is erased after it.
        assert_eq!(text("[1/3] a\r[2/3] b\r[3/3] c"), "[3/3] c");
        assert_eq!(text("downloading 100%\rdone"), "doneloading 100%");
        assert_eq!(text("downloading 100%\rdone\u{1b}[K"), "done");
        assert_eq!(text("abc\u{8}\u{8}X"), "aXc");
    }

    #[test]
    fn every_kind_of_escape_leaves_nothing_behind() {
        // tput sgr0's character set choice ESC ( B before its reset, a
        // device control string, a private mode that hides the cursor and
        // a cursor move all leave only the text around them.
        assert_eq!(text("\u{1b}[1mok\u{1b}(B\u{1b}[m done"), "ok done");
        assert_eq!(text("a\u{1b}P1$r0m\u{1b}\\b"), "ab");
        assert_eq!(text("\u{1b}[?25lbusy\u{1b}[?25h"), "busy");
        assert_eq!(text("x\u{1b}[2Cy"), "xy");
    }

    #[test]
    fn titles_and_other_controls_are_dropped() {
        // An operating system command that sets the window title, ended by
        // a bell or by ESC \, and stray control characters leave nothing;
        // a tab stays.
        assert_eq!(text("\u{1b}]0;make\u{7}building"), "building");
        assert_eq!(text("\u{1b}]2;title\u{1b}\\ok"), "ok");
        assert_eq!(text("a\u{1}b\tc"), "ab\tc");
        assert_eq!(shown("plain text").pens, vec![]);
    }
}
