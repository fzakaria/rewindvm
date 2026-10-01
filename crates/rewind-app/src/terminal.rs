//! A terminal: a command running in a host pty, the screen and
//! scrollback its output draws, and the keys sent to it as xterm input.
//!
//! The terminal emulation is alacritty_terminal's: it owns the pty, reads
//! the command's output on a thread of its own, and keeps the grid. This
//! module starts a command in it, reads the grid out as styled rows for
//! the pane to draw, and encodes keys and pastes. `ui::terminal` draws the
//! pane.

use std::borrow::Cow;
use std::collections::HashMap;
use std::io;
use std::ops::Range;
use std::sync::Arc;

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::tty::{self, Options, Shell};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Rgb};
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};

use crate::engine::CommandLine;
use crate::selection::Lines;
use crate::theme;

/// How many lines of scrollback the terminal keeps.
const SCROLLBACK: usize = 10_000;

/// The terminal type the command sees, and its color support.
const TERM: &str = "xterm-256color";
const COLORTERM: &str = "truecolor";

/// The pty's window id, which alacritty passes on as WINDOWID; the app
/// has no X window id to give.
const WINDOW_ID: u64 = 0;

/// The grid's size in cells, and a cell's size in pixels for programs that
/// ask the pty for pixel sizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridSize {
    pub columns: usize,
    pub lines: usize,
    pub cell_width: u16,
    pub cell_height: u16,
}

impl GridSize {
    /// The smallest grid the pane makes, so a pane squeezed to nothing
    /// still gives the command a terminal it can draw in.
    pub const MIN_COLUMNS: usize = 20;
    pub const MIN_LINES: usize = 4;

    fn window_size(self) -> WindowSize {
        WindowSize {
            num_lines: self.lines as u16,
            num_cols: self.columns as u16,
            cell_width: self.cell_width,
            cell_height: self.cell_height,
        }
    }
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.lines
    }

    fn screen_lines(&self) -> usize {
        self.lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

/// Passes the terminal's events to the pane through a channel; the pane
/// reads them on the UI thread.
#[derive(Clone)]
pub struct Listener(UnboundedSender<Event>);

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        let _ = self.0.unbounded_send(event);
    }
}

/// A command running in a terminal.
pub struct Session {
    term: Arc<FairMutex<Term<Listener>>>,
    sender: EventLoopSender,
    size: GridSize,
}

impl Session {
    /// Starts `command` in a new pty of `size`. The events the terminal
    /// sends, output to draw, a title, the command's exit, come out of the
    /// returned channel.
    pub fn spawn(
        command: &CommandLine,
        size: GridSize,
    ) -> io::Result<(Session, UnboundedReceiver<Event>)> {
        let (tx, rx) = unbounded();
        let listener = Listener(tx);
        let config = Config {
            scrolling_history: SCROLLBACK,
            ..Config::default()
        };
        let term = Arc::new(FairMutex::new(Term::new(config, &size, listener.clone())));

        let env = HashMap::from([
            ("TERM".to_string(), TERM.to_string()),
            ("COLORTERM".to_string(), COLORTERM.to_string()),
        ]);
        let options = Options {
            shell: Some(Shell::new(
                command.program.to_string_lossy().into_owned(),
                command
                    .args
                    .iter()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect(),
            )),
            working_directory: None,
            drain_on_exit: true,
            env,
        };
        let pty = tty::new(&options, size.window_size(), WINDOW_ID)?;
        let event_loop = EventLoop::new(term.clone(), listener, pty, options.drain_on_exit, false)?;
        let sender = event_loop.channel();
        event_loop.spawn();
        Ok((Session { term, sender, size }, rx))
    }

    pub fn size(&self) -> GridSize {
        self.size
    }

    /// Sends bytes to the command, as typed.
    pub fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        let _ = self.sender.send(Msg::Input(bytes.into()));
    }

    /// Pastes `text`: bracketed when the command asked for it, with line
    /// ends as the Enter key sends them otherwise.
    pub fn paste(&self, text: &str) {
        let bracketed = self.mode().contains(TermMode::BRACKETED_PASTE);
        self.write(encode_paste(text, bracketed));
    }

    /// Resizes the grid and the pty (TIOCSWINSZ), which sends the command
    /// SIGWINCH.
    pub fn resize(&mut self, size: GridSize) {
        if size == self.size {
            return;
        }
        self.size = size;
        self.term.lock().resize(size);
        let _ = self.sender.send(Msg::Resize(size.window_size()));
    }

    /// Scrolls the view into the scrollback by `lines`, up for positive.
    pub fn scroll(&self, lines: i32) {
        self.term.lock().scroll_display(Scroll::Delta(lines));
    }

    /// Puts the view back at the bottom, where output lands.
    pub fn scroll_to_bottom(&self) {
        self.term.lock().scroll_display(Scroll::Bottom);
    }

    pub fn mode(&self) -> TermMode {
        *self.term.lock().mode()
    }

    /// Asks the terminal's thread to stop; dropping the pty hangs up on
    /// the command.
    pub fn shutdown(&self) {
        let _ = self.sender.send(Msg::Shutdown);
    }

    /// The rows on screen, with their styles, the cursor and the scroll
    /// position.
    pub fn screen(&self) -> Screen {
        let term = self.term.lock();
        let grid = term.grid();
        let history = grid.total_lines() - grid.screen_lines();
        let offset = grid.display_offset();
        let palette = Palette::new(term.colors());
        let rows = (0..grid.screen_lines())
            .map(|i| {
                let line = Line(i as i32 - offset as i32);
                let mut row = styled_row(&grid[line], grid.columns(), &palette);
                row.line = (line.0 + history as i32) as usize;
                row
            })
            .collect::<Vec<_>>();

        // The cursor, as a cell drawn in reverse, while the view is at
        // the bottom and the command shows it.
        let content = term.renderable_content();
        let cursor = (content.cursor.shape != CursorShape::Hidden && offset == 0)
            .then(|| {
                let point = content.cursor.point;
                let row = rows.get(point.line.0 as usize)?;
                let column = point.column.0;
                let start = row.column_offsets.get(column).copied()?;
                let end = row
                    .column_offsets
                    .get(column + 1)
                    .copied()
                    .unwrap_or(row.text.len());
                Some((point.line.0 as usize, start..end.max(start)))
            })
            .flatten();
        Screen {
            rows,
            cursor,
            scrolled_back: offset,
            history,
        }
    }

    /// The text of the line `line` lines below the top of the scrollback,
    /// for selections.
    pub fn line_text(&self, line: usize) -> Option<String> {
        let term = self.term.lock();
        let grid = term.grid();
        let history = grid.total_lines() - grid.screen_lines();
        let at = line as i32 - history as i32;
        if at >= grid.screen_lines() as i32 || line >= grid.total_lines() {
            return None;
        }
        let row = styled_row(&grid[Line(at)], grid.columns(), &Palette::default());
        Some(row.text)
    }

    /// How many lines the scrollback and the screen hold together.
    pub fn total_lines(&self) -> usize {
        self.term.lock().grid().total_lines()
    }

    /// The color the command asked about with OSC 4, 10 or 11.
    pub fn color(&self, index: usize) -> Rgb {
        let term = self.term.lock();
        Palette::new(term.colors()).indexed(index)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The terminal's lines as a selection reads them. A line copies without
/// the blanks that fill it out to the grid's width.
impl Lines for Session {
    fn line_count(&self) -> usize {
        self.total_lines()
    }

    fn shown(&self, line: usize) -> Option<String> {
        self.line_text(line)
    }

    fn copied(&self, line: usize, range: Range<usize>) -> Option<String> {
        let text = self.line_text(line)?;
        let end = range.end.min(text.len());
        let start = range.start.min(end);
        let part = &text[start..end];
        Some(if end == text.len() {
            part.trim_end().to_string()
        } else {
            part.to_string()
        })
    }
}

/// What the pane draws.
#[derive(Clone, Debug, Default)]
pub struct Screen {
    pub rows: Vec<Row>,
    /// The cursor's row on screen and its byte range in that row's text.
    pub cursor: Option<(usize, Range<usize>)>,
    /// How many lines the view is scrolled back from the bottom.
    pub scrolled_back: usize,
    /// How many lines of scrollback there are above the screen.
    pub history: usize,
}

/// One line of the grid as text with styled spans.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Row {
    /// The line's index from the top of the scrollback.
    pub line: usize,
    pub text: String,
    pub spans: Vec<Span>,
    /// The byte offset in `text` where each column starts.
    pub column_offsets: Vec<usize>,
}

/// A run of cells drawn alike.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Style {
    pub fg: Rgb,
    /// The cell's background, when it is not the terminal's.
    pub bg: Option<Rgb>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub range: Range<usize>,
    pub style: Style,
}

/// Reads a grid row into text and spans. Spacer cells after wide
/// characters add nothing; combining characters join their cell.
fn styled_row(
    row: &alacritty_terminal::grid::Row<alacritty_terminal::term::cell::Cell>,
    columns: usize,
    palette: &Palette,
) -> Row {
    let mut out = Row::default();
    for column in 0..columns {
        let cell = &row[Column(column)];
        out.column_offsets.push(out.text.len());
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            continue;
        }
        let start = out.text.len();
        let hidden = cell.flags.contains(Flags::HIDDEN);
        out.text.push(if hidden { ' ' } else { cell.c });
        if let Some(extra) = cell.zerowidth() {
            out.text.extend(extra.iter());
        }
        let style = palette.style(cell.fg, cell.bg, cell.flags);
        match out.spans.last_mut() {
            Some(last) if last.style == style && last.range.end == start => {
                last.range.end = out.text.len()
            }
            _ => out.spans.push(Span {
                range: start..out.text.len(),
                style,
            }),
        }
    }
    out
}

/// The terminal's colors: the app's theme for the default foreground and
/// background and the sixteen named colors, the xterm cube and greys for
/// the rest, and whatever the command set with OSC 4.
#[derive(Clone, Debug, Default)]
struct Palette {
    overrides: Vec<Option<Rgb>>,
}

/// The sixteen named colors, normal then bright, tuned to the theme.
const ANSI: [u32; 16] = [
    0x1c1f24, 0xff7a6b, 0x9fd8a4, 0xf2c56b, 0x7fb2ff, 0xd3a0f2, 0x7fd6d2, 0xc9c5bd, 0x5f6571,
    0xff9a8e, 0xb8ecbc, 0xffd98f, 0xa9cbff, 0xe2bdf7, 0xa4e8e4, 0xebe7df,
];

/// Where the 6x6x6 color cube and the grey ramp start in the 256-color
/// palette, and how they step.
const CUBE_START: usize = 16;
const GREY_START: usize = 232;
const CUBE_STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];
const GREY_BASE: u8 = 8;
const GREY_STEP: u8 = 10;

impl Palette {
    fn new(colors: &alacritty_terminal::term::color::Colors) -> Palette {
        const COUNT: usize = 269;
        Palette {
            overrides: (0..COUNT).map(|i| colors[i]).collect(),
        }
    }

    fn rgb(word: u32) -> Rgb {
        Rgb {
            r: (word >> 16) as u8,
            g: (word >> 8) as u8,
            b: word as u8,
        }
    }

    /// The color at a palette index: 0 to 255, or a named color's index.
    fn indexed(&self, index: usize) -> Rgb {
        if let Some(Some(color)) = self.overrides.get(index) {
            return *color;
        }
        match index {
            i if i < CUBE_START => Palette::rgb(ANSI[i]),
            i if i < GREY_START => {
                let i = i - CUBE_START;
                Rgb {
                    r: CUBE_STEPS[i / 36],
                    g: CUBE_STEPS[(i / 6) % 6],
                    b: CUBE_STEPS[i % 6],
                }
            }
            i if i < 256 => {
                let level = GREY_BASE + GREY_STEP * (i - GREY_START) as u8;
                Rgb {
                    r: level,
                    g: level,
                    b: level,
                }
            }
            i if i == NamedColor::Background as usize => Palette::rgb(theme::PANEL),
            i if i == NamedColor::DimForeground as usize => Palette::rgb(theme::MUTED),
            _ => Palette::rgb(theme::TEXT),
        }
    }

    fn color(&self, color: Color) -> Rgb {
        match color {
            Color::Spec(rgb) => rgb,
            Color::Indexed(i) => self.indexed(i as usize),
            Color::Named(named) => self.indexed(named as usize),
        }
    }

    fn style(&self, fg: Color, bg: Color, flags: Flags) -> Style {
        // Bold text in a named color is drawn in its bright variant, as
        // xterm does.
        const BRIGHT: usize = 8;
        let fg = match fg {
            Color::Named(named) if flags.contains(Flags::BOLD) && (named as usize) < BRIGHT => {
                Color::Indexed(named as u8 + BRIGHT as u8)
            }
            other => other,
        };
        let default_bg = matches!(bg, Color::Named(NamedColor::Background));
        let (mut fg_rgb, mut bg_rgb) = (self.color(fg), self.color(bg));
        let mut bg_set = !default_bg;
        if flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg_rgb, &mut bg_rgb);
            bg_set = true;
        }
        if flags.contains(Flags::DIM) {
            fg_rgb = dim(fg_rgb);
        }
        Style {
            fg: fg_rgb,
            bg: bg_set.then_some(bg_rgb),
            bold: flags.contains(Flags::BOLD),
            italic: flags.contains(Flags::ITALIC),
            underline: flags.intersects(Flags::ALL_UNDERLINES),
        }
    }
}

/// A dimmed color: two thirds of its brightness.
fn dim(c: Rgb) -> Rgb {
    const KEEP: u16 = 2;
    const OF: u16 = 3;
    let f = |v: u8| (v as u16 * KEEP / OF) as u8;
    Rgb {
        r: f(c.r),
        g: f(c.g),
        b: f(c.b),
    }
}

/// A key the terminal encodes, independent of the UI toolkit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Key {
    /// A character, as typed with any shift applied.
    Char(char),
    Enter,
    Tab,
    Backspace,
    Escape,
    Up,
    Down,
    Right,
    Left,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    /// F1 to F12.
    F(u8),
}

/// The modifiers held with a key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

impl Mods {
    /// xterm's modifier parameter: 1 plus shift 1, alt 2 and control 4.
    fn parameter(self) -> u8 {
        1 + self.shift as u8 + 2 * self.alt as u8 + 4 * self.ctrl as u8
    }

    fn any(self) -> bool {
        self.shift || self.alt || self.ctrl
    }
}

/// Whether the command switched the cursor keys to application mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorKeys {
    Normal,
    Application,
}

const ESC: u8 = 0x1b;
const DEL: u8 = 0x7f;
const BACKSPACE: u8 = 0x08;

/// The bytes xterm sends for `key` with `mods`, or None for a key the
/// terminal does not send.
pub fn encode_key(key: &Key, mods: Mods, cursor_keys: CursorKeys) -> Option<Vec<u8>> {
    let alt_prefix = |mut bytes: Vec<u8>| {
        if mods.alt {
            bytes.insert(0, ESC);
        }
        bytes
    };
    let bytes = match key {
        Key::Char(c) if mods.ctrl => {
            let byte = control_byte(*c)?;
            alt_prefix(vec![byte])
        }
        Key::Char(c) => {
            let mut buf = [0u8; 4];
            alt_prefix(c.encode_utf8(&mut buf).as_bytes().to_vec())
        }
        Key::Enter => alt_prefix(vec![b'\r']),
        Key::Tab if mods.shift => b"\x1b[Z".to_vec(),
        Key::Tab => alt_prefix(vec![b'\t']),
        Key::Backspace if mods.ctrl => alt_prefix(vec![BACKSPACE]),
        Key::Backspace => alt_prefix(vec![DEL]),
        Key::Escape => alt_prefix(vec![ESC]),
        Key::Up => cursor(b'A', mods, cursor_keys),
        Key::Down => cursor(b'B', mods, cursor_keys),
        Key::Right => cursor(b'C', mods, cursor_keys),
        Key::Left => cursor(b'D', mods, cursor_keys),
        Key::Home => cursor(b'H', mods, cursor_keys),
        Key::End => cursor(b'F', mods, cursor_keys),
        Key::Insert => tilde(2, mods),
        Key::Delete => tilde(3, mods),
        Key::PageUp => tilde(5, mods),
        Key::PageDown => tilde(6, mods),
        Key::F(n @ 1..=4) => {
            let last = b'P' + (n - 1);
            if mods.any() {
                format!("\x1b[1;{}{}", mods.parameter(), last as char).into_bytes()
            } else {
                vec![ESC, b'O', last]
            }
        }
        Key::F(n @ 5..=12) => {
            // xterm skips 16 and 22.
            const CODES: [u8; 8] = [15, 17, 18, 19, 20, 21, 23, 24];
            tilde(CODES[(*n - 5) as usize], mods)
        }
        Key::F(_) => return None,
    };
    Some(bytes)
}

/// The control character for Ctrl with `c`: letters and @[\]^_ map to
/// 0 to 31, space and 2 to NUL, / to 31 and ? to DEL.
fn control_byte(c: char) -> Option<u8> {
    const CONTROL_MASK: u8 = 0x1f;
    match c {
        'a'..='z' | 'A'..='Z' | '@' | '[' | '\\' | ']' | '^' | '_' => {
            Some(c.to_ascii_uppercase() as u8 & CONTROL_MASK)
        }
        ' ' | '2' => Some(0),
        '/' => Some(CONTROL_MASK),
        '?' => Some(DEL),
        _ => None,
    }
}

/// A cursor key: SS3 in application mode without modifiers, CSI with
/// the modifier parameter when any is held.
fn cursor(last: u8, mods: Mods, cursor_keys: CursorKeys) -> Vec<u8> {
    if mods.any() {
        return format!("\x1b[1;{}{}", mods.parameter(), last as char).into_bytes();
    }
    match cursor_keys {
        CursorKeys::Application => vec![ESC, b'O', last],
        CursorKeys::Normal => vec![ESC, b'[', last],
    }
}

/// An editing key: CSI number ~, with the modifier parameter when held.
fn tilde(number: u8, mods: Mods) -> Vec<u8> {
    if mods.any() {
        format!("\x1b[{number};{}~", mods.parameter()).into_bytes()
    } else {
        format!("\x1b[{number}~").into_bytes()
    }
}

/// A paste as the command should get it: between the bracketed paste
/// markers when it asked for them, with every line end as a carriage
/// return otherwise, as typing Enter would send.
pub fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        // A paste must not end the bracket early.
        let clean = text.replace("\x1b[201~", "");
        return format!("\x1b[200~{clean}\x1b[201~").into_bytes();
    }
    text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
}

#[cfg(test)]
mod tests {
    // Key encoding against xterm's sequences, and a real command in a
    // real pty: its output on the grid, a resize, and its exit.
    use super::*;
    use std::time::{Duration, Instant};

    const NONE: Mods = Mods {
        shift: false,
        alt: false,
        ctrl: false,
    };

    fn key(key: Key, mods: Mods) -> Vec<u8> {
        encode_key(&key, mods, CursorKeys::Normal).unwrap()
    }

    #[test]
    fn printable_keys_and_control_characters() {
        // Characters go as UTF-8, Ctrl with a letter as its control code,
        // Alt as an escape before the key.
        assert_eq!(key(Key::Char('a'), NONE), b"a");
        assert_eq!(key(Key::Char('\u{e9}'), NONE), "\u{e9}".as_bytes());
        let ctrl = Mods { ctrl: true, ..NONE };
        assert_eq!(key(Key::Char('c'), ctrl), [0x03]);
        assert_eq!(key(Key::Char('D'), ctrl), [0x04]);
        assert_eq!(key(Key::Char(' '), ctrl), [0x00]);
        assert_eq!(key(Key::Char('['), ctrl), [0x1b]);
        let alt = Mods { alt: true, ..NONE };
        assert_eq!(key(Key::Char('b'), alt), b"\x1bb");
        assert_eq!(encode_key(&Key::Char('1'), ctrl, CursorKeys::Normal), None);
    }

    #[test]
    fn named_keys_follow_xterm() {
        assert_eq!(key(Key::Enter, NONE), b"\r");
        assert_eq!(key(Key::Backspace, NONE), [0x7f]);
        assert_eq!(
            key(
                Key::Tab,
                Mods {
                    shift: true,
                    ..NONE
                }
            ),
            b"\x1b[Z"
        );
        assert_eq!(key(Key::Delete, NONE), b"\x1b[3~");
        assert_eq!(key(Key::PageUp, NONE), b"\x1b[5~");
        assert_eq!(key(Key::F(1), NONE), b"\x1bOP");
        assert_eq!(key(Key::F(5), NONE), b"\x1b[15~");
        assert_eq!(key(Key::F(12), NONE), b"\x1b[24~");
        assert_eq!(
            key(
                Key::F(2),
                Mods {
                    shift: true,
                    ..NONE
                }
            ),
            b"\x1b[1;2Q"
        );
    }

    #[test]
    fn cursor_keys_follow_the_mode_and_the_modifiers() {
        // Application mode sends SS3; any modifier sends CSI 1;m.
        assert_eq!(key(Key::Up, NONE), b"\x1b[A");
        assert_eq!(
            encode_key(&Key::Up, NONE, CursorKeys::Application).unwrap(),
            b"\x1bOA"
        );
        let ctrl = Mods { ctrl: true, ..NONE };
        assert_eq!(key(Key::Left, ctrl), b"\x1b[1;5D");
        assert_eq!(key(Key::Home, NONE), b"\x1b[H");
        assert_eq!(
            key(
                Key::End,
                Mods {
                    shift: true,
                    ..NONE
                }
            ),
            b"\x1b[1;2F"
        );
    }

    #[test]
    fn pastes_are_bracketed_when_asked() {
        assert_eq!(encode_paste("a\nb", false), b"a\rb");
        assert_eq!(encode_paste("a\r\nb", false), b"a\rb");
        assert_eq!(encode_paste("ls\n", true), b"\x1b[200~ls\n\x1b[201~");
        assert_eq!(encode_paste("x\x1b[201~y", true), b"\x1b[200~xy\x1b[201~");
    }

    #[test]
    fn the_cube_and_the_greys_follow_xterm() {
        let palette = Palette::default();
        assert_eq!(palette.indexed(16), Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(palette.indexed(196), Rgb { r: 255, g: 0, b: 0 });
        assert_eq!(palette.indexed(232), Rgb { r: 8, g: 8, b: 8 });
        assert_eq!(
            palette.indexed(255),
            Rgb {
                r: 238,
                g: 238,
                b: 238
            }
        );
    }

    /// Waits for `done` on the session, up to a few seconds, reading the
    /// terminal's events as they come.
    fn wait_for(
        session: &Session,
        events: &mut UnboundedReceiver<Event>,
        done: impl Fn(&Session, &[Event]) -> bool,
    ) -> Vec<Event> {
        const LIMIT: Duration = Duration::from_secs(10);
        const PAUSE: Duration = Duration::from_millis(20);
        let start = Instant::now();
        let mut seen = Vec::new();
        while start.elapsed() < LIMIT {
            while let Ok(event) = events.try_recv() {
                seen.push(event);
            }
            if done(session, &seen) {
                return seen;
            }
            std::thread::sleep(PAUSE);
        }
        panic!("timed out; screen: {:?}", session.screen().rows);
    }

    fn size(columns: usize, lines: usize) -> GridSize {
        GridSize {
            columns,
            lines,
            cell_width: 8,
            cell_height: 16,
        }
    }

    fn sh(script: &str) -> CommandLine {
        CommandLine {
            program: "sh".into(),
            args: vec!["-c".into(), script.into()],
        }
    }

    #[test]
    fn a_command_draws_in_colors_and_its_exit_is_reported() {
        // printf writes two lines, the second in red; the grid holds them
        // and the exit status comes out as an event.
        let (session, mut events) = Session::spawn(
            &sh("printf 'hello\\r\\n\\033[31mred\\033[0m\\r\\n'; exit 3"),
            size(40, 5),
        )
        .unwrap();
        let seen = wait_for(&session, &mut events, |_, seen| {
            seen.iter().any(|e| matches!(e, Event::ChildExit(_)))
        });
        let status = seen
            .iter()
            .find_map(|e| match e {
                Event::ChildExit(status) => Some(*status),
                _ => None,
            })
            .unwrap();
        assert_eq!(status.code(), Some(3));
        let screen = session.screen();
        assert!(screen.rows[0].text.starts_with("hello"));
        assert!(screen.rows[1].text.starts_with("red"));
        let red = &screen.rows[1].spans[0];
        assert_eq!(red.range, 0..3);
        assert_eq!(red.style.fg, Palette::rgb(ANSI[1]));
        assert_eq!(screen.rows[0].text.chars().count(), 40);
        assert_eq!(session.copied(screen.rows[0].line, 0..40).unwrap(), "hello");
    }

    #[test]
    fn typed_input_reaches_the_command_and_a_resize_reaches_its_tty() {
        // The command reads a line and prints it back with the tty's size
        // as stty sees it after a resize.
        let (mut session, mut events) = Session::spawn(
            &sh("read line; echo \"got $line\"; stty size; read _"),
            size(40, 5),
        )
        .unwrap();
        session.resize(size(60, 7));
        session.write(b"abc\r".to_vec());
        wait_for(&session, &mut events, |s, _| {
            s.screen().rows.iter().any(|r| r.text.starts_with("7 60"))
        });
        let screen = session.screen();
        assert!(screen.rows.iter().any(|r| r.text.starts_with("got abc")));
        assert_eq!(screen.rows[0].text.chars().count(), 60);
        session.write(b"\r".to_vec());
        wait_for(&session, &mut events, |_, seen| {
            seen.iter().any(|e| matches!(e, Event::ChildExit(_)))
        });
    }
}
