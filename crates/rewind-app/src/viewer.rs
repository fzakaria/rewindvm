//! A file's contents as the viewer shows them: text as lines, anything
//! else as a hex dump, both cut to a size a panel can hold.

use crate::selection::{Lines, Mapped, Splice};
use crate::theme;

/// The most bytes the viewer shows; the rest is summed up in a note.
pub const MAX_SHOWN: usize = 1 << 20;

/// A file counts as binary when its first bytes hold a NUL or are not
/// UTF-8. Checking this many is enough to tell.
const SNIFF: usize = 8 << 10;

/// Bytes per line of a hex dump, and per group within a line; eight
/// bytes fit the viewer next to the other panels.
const HEX_WIDTH: usize = 8;
const HEX_GROUP: usize = 4;

/// The first byte of a file that `ascii` shows as itself, and the last.
const PRINTABLE: std::ops::RangeInclusive<u8> = 0x20..=0x7e;

/// How the bytes read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Text,
    Binary,
}

/// A file ready to show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    pub kind: Kind,
    /// Text lines, or hex dump lines.
    pub lines: Vec<String>,
    /// The bytes a hex dump shows, for coloring them; empty for text.
    pub bytes: Vec<u8>,
    /// The file's whole size in bytes.
    pub size: usize,
    /// Whether only the first `MAX_SHOWN` bytes are shown. `fetched_all`
    /// false also counts: the engine's answer itself was cut short.
    pub truncated: bool,
}

impl View {
    /// Lays out `bytes`, the whole file unless `fetched_all` is false.
    pub fn new(bytes: &[u8], fetched_all: FetchedAll) -> View {
        let shown = &bytes[..bytes.len().min(MAX_SHOWN)];
        let truncated = shown.len() < bytes.len() || fetched_all == FetchedAll::No;
        let sniff = &shown[..shown.len().min(SNIFF)];
        let is_text = !sniff.contains(&0) && utf8_prefix_ok(sniff);
        let (kind, lines, dumped) = if is_text {
            let text = String::from_utf8_lossy(shown);
            let lines = text.lines().map(str::to_string).collect();
            (Kind::Text, lines, Vec::new())
        } else {
            let lines = shown
                .chunks(HEX_WIDTH)
                .enumerate()
                .map(|(i, chunk)| hex_line(i * HEX_WIDTH, chunk))
                .collect();
            (Kind::Binary, lines, shown.to_vec())
        };
        View {
            kind,
            lines,
            bytes: dumped,
            size: bytes.len(),
            truncated,
        }
    }

    /// The bytes hex dump line `row` shows.
    pub fn chunk(&self, row: usize) -> &[u8] {
        let start = (row * HEX_WIDTH).min(self.bytes.len());
        let end = (start + HEX_WIDTH).min(self.bytes.len());
        &self.bytes[start..end]
    }
}

/// Whether the caller got the whole file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FetchedAll {
    Yes,
    No,
}

/// Whether bytes are UTF-8, allowing a character cut in two at the end.
fn utf8_prefix_ok(bytes: &[u8]) -> bool {
    const MAX_CHAR_LEN: usize = 4;
    match std::str::from_utf8(bytes) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none() && bytes.len() - e.valid_up_to() < MAX_CHAR_LEN,
    }
}

/// One hex dump line: offset, the bytes in two groups, and the printable
/// ones as characters.
pub fn hex_line(offset: usize, chunk: &[u8]) -> String {
    let mut hex = String::new();
    for i in 0..HEX_WIDTH {
        if i == HEX_GROUP {
            hex.push(' ');
        }
        match chunk.get(i) {
            Some(b) => hex.push_str(&format!("{b:02x} ")),
            None => hex.push_str("   "),
        }
    }
    let ascii: String = chunk
        .iter()
        .map(|b| {
            if PRINTABLE.contains(b) {
                *b as char
            } else {
                '.'
            }
        })
        .collect();
    format!("{offset:08x}  {hex} {ascii}")
}

/// What a byte is, as the hex dump colors it after hexyl.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ByteClass {
    Null,
    /// ASCII letters, digits and punctuation.
    Printable,
    /// Space, tab, the line breaks, vertical tab and form feed.
    Whitespace,
    /// ASCII's other control bytes, DEL among them.
    Control,
    /// 0xff, often padding or -1.
    AllOnes,
    /// Bytes above ASCII.
    NonAscii,
}

impl ByteClass {
    pub fn of(byte: u8) -> ByteClass {
        const NUL: u8 = 0x00;
        const ALL_ONES: u8 = 0xff;
        const VERTICAL_TAB: u8 = 0x0b;
        match byte {
            NUL => ByteClass::Null,
            ALL_ONES => ByteClass::AllOnes,
            b' ' | b'\t' | b'\n' | VERTICAL_TAB | b'\x0c' | b'\r' => ByteClass::Whitespace,
            b if b.is_ascii_graphic() => ByteClass::Printable,
            b if b.is_ascii() => ByteClass::Control,
            _ => ByteClass::NonAscii,
        }
    }

    /// The class's color.
    pub fn color(self) -> u32 {
        match self {
            ByteClass::Null => theme::bytes::NULL,
            ByteClass::Printable => theme::bytes::PRINTABLE,
            ByteClass::Whitespace => theme::bytes::WHITESPACE,
            ByteClass::Control => theme::bytes::CONTROL,
            ByteClass::AllOnes => theme::bytes::ALL_ONES,
            ByteClass::NonAscii => theme::bytes::NON_ASCII,
        }
    }
}

/// Where a hex dump line's parts start, as `hex_line` lays it out: the
/// offset's digits, the gap after them, the width of a byte's pair with
/// its space, and the gap between the hex and the characters.
const OFFSET_DIGITS: usize = 8;
const OFFSET_GAP: usize = 2;
const PAIR_WIDTH: usize = 3;
const PAIR_DIGITS: usize = 2;
const CHARACTERS_GAP: usize = 1;

/// The colored parts of the hex dump line `hex_line` draws for `chunk`:
/// the offset, then each byte's hex pair and character, by byte range in
/// the line and color.
pub fn hex_spans(chunk: &[u8]) -> Vec<(std::ops::Range<usize>, u32)> {
    let hex_start = OFFSET_DIGITS + OFFSET_GAP;
    let group_gap = HEX_WIDTH / HEX_GROUP - 1;
    let characters = hex_start + HEX_WIDTH * PAIR_WIDTH + group_gap + CHARACTERS_GAP;

    // The offset, then each byte in the hex and in the characters.
    let mut spans = vec![(0..OFFSET_DIGITS, theme::bytes::OFFSET)];
    for (i, &byte) in chunk.iter().enumerate().take(HEX_WIDTH) {
        let color = ByteClass::of(byte).color();
        let pair = hex_start + i * PAIR_WIDTH + i / HEX_GROUP;
        spans.push((pair..pair + PAIR_DIGITS, color));
        spans.push((characters + i..characters + i + 1, color));
    }
    spans
}

/// The spaces a tab is drawn as.
pub const TAB_WIDTH: usize = 4;

/// A line of text with its tabs drawn as spaces, copying back as tabs.
pub fn expand_tabs(line: &str) -> Mapped {
    if !line.contains('\t') {
        return Mapped::plain(line);
    }
    let mut shown = String::with_capacity(line.len() + TAB_WIDTH);
    let mut splices = Vec::new();
    for (i, c) in line.char_indices() {
        if c != '\t' {
            shown.push(c);
            continue;
        }
        let start = shown.len();
        shown.push_str(&" ".repeat(TAB_WIDTH));
        splices.push(Splice {
            shown: start..shown.len(),
            original: i..i + 1,
        });
    }
    Mapped::new(shown, line.to_string(), splices)
}

/// Lines of text with their tabs drawn as spaces, read on demand: a file
/// can be long, and a selection reads only the lines it covers.
pub struct TabbedLines<'a>(pub &'a [String]);

impl Lines for TabbedLines<'_> {
    fn line_count(&self) -> usize {
        self.0.len()
    }

    fn shown(&self, line: usize) -> Option<String> {
        self.0.get(line).map(|l| expand_tabs(l).shown)
    }

    fn copied(&self, line: usize, range: std::ops::Range<usize>) -> Option<String> {
        self.0.get(line).map(|l| expand_tabs(l).copy(range))
    }
}

#[cfg(test)]
mod tests {
    // Layouts of small byte strings: each test builds bytes, lays them
    // out, and checks the kind, the lines and the notes.
    use super::*;

    #[test]
    fn tabs_are_drawn_as_spaces_and_copied_as_tabs() {
        let line = expand_tabs("a\tb");
        assert_eq!(line.shown, "a    b");
        assert_eq!(line.copy(0..line.shown.len()), "a\tb");
        assert_eq!(line.copy(2..6), "\tb");
        assert_eq!(expand_tabs("plain").shown, "plain");
    }

    #[test]
    fn text_is_shown_as_lines() {
        let v = View::new(b"CFLAGS = -O1\nall: pool\n", FetchedAll::Yes);
        assert_eq!(v.kind, Kind::Text);
        assert_eq!(v.lines, vec!["CFLAGS = -O1", "all: pool"]);
        assert!(!v.truncated);
    }

    #[test]
    fn binary_is_shown_as_a_hex_dump() {
        // An ELF header: a NUL makes it binary.
        let v = View::new(b"\x7fELF\x02\x01\x01\x00abcdefghijklmnop", FetchedAll::Yes);
        assert_eq!(v.kind, Kind::Binary);
        assert_eq!(v.lines.len(), 3);
        assert_eq!(v.lines[0], "00000000  7f 45 4c 46  02 01 01 00  .ELF....");
        assert_eq!(v.lines[2], "00000010  69 6a 6b 6c  6d 6e 6f 70  ijklmnop");
        assert_eq!(v.size, 24);
    }

    /// Bytes by what they are, as hexyl tells them apart: NUL, printable
    /// ASCII, ASCII whitespace, other ASCII control bytes, 0xff, and the
    /// rest of the bytes above ASCII.
    #[test]
    fn bytes_are_told_apart_as_hexyl_does() {
        let cases = [
            (0x00, ByteClass::Null),
            (b'A', ByteClass::Printable),
            (b'~', ByteClass::Printable),
            (b'!', ByteClass::Printable),
            (b' ', ByteClass::Whitespace),
            (b'\t', ByteClass::Whitespace),
            (b'\n', ByteClass::Whitespace),
            (0x0b, ByteClass::Whitespace),
            (0x0c, ByteClass::Whitespace),
            (b'\r', ByteClass::Whitespace),
            (0x01, ByteClass::Control),
            (0x1b, ByteClass::Control),
            (0x7f, ByteClass::Control),
            (0xff, ByteClass::AllOnes),
            (0x80, ByteClass::NonAscii),
            (0xfe, ByteClass::NonAscii),
        ];
        for (byte, class) in cases {
            assert_eq!(ByteClass::of(byte), class, "{byte:#04x}");
        }
    }

    /// An ELF header's first line colors its offset in the gutter's color
    /// and each byte's hex pair and character alike: 0x7f and the small
    /// numbers as control bytes, "ELF" as printable, the NUL dim. Checks
    /// each span against the text it covers on the line hex_line draws.
    #[test]
    fn a_hex_dump_line_colors_each_byte_in_both_columns() {
        use crate::theme::bytes;

        let chunk = b"\x7fELF\x02\x01\x01\x00";
        let line = hex_line(0, chunk);
        let spans = hex_spans(chunk);
        let covered: Vec<(&str, u32)> = spans
            .iter()
            .map(|(range, color)| (&line[range.clone()], *color))
            .collect();
        assert_eq!(
            covered,
            vec![
                ("00000000", bytes::OFFSET),
                ("7f", bytes::CONTROL),
                ("\u{2e}", bytes::CONTROL),
                ("45", bytes::PRINTABLE),
                ("E", bytes::PRINTABLE),
                ("4c", bytes::PRINTABLE),
                ("L", bytes::PRINTABLE),
                ("46", bytes::PRINTABLE),
                ("F", bytes::PRINTABLE),
                ("02", bytes::CONTROL),
                (".", bytes::CONTROL),
                ("01", bytes::CONTROL),
                (".", bytes::CONTROL),
                ("01", bytes::CONTROL),
                (".", bytes::CONTROL),
                ("00", bytes::NULL),
                (".", bytes::NULL),
            ]
        );

        // The last line of a file can be short.
        let short = hex_spans(b"\xff");
        let line = hex_line(16, b"\xff");
        assert_eq!(&line[short[1].0.clone()], "ff");
        assert_eq!(&line[short[2].0.clone()], ".");
        assert_eq!(short[1].1, bytes::ALL_ONES);
    }

    #[test]
    fn large_files_are_cut_and_say_so() {
        // One byte over the limit, and an answer the engine cut short.
        let big = vec![b'a'; MAX_SHOWN + 1];
        let v = View::new(&big, FetchedAll::Yes);
        assert!(v.truncated);
        assert_eq!(v.size, MAX_SHOWN + 1);
        assert!(View::new(b"short", FetchedAll::No).truncated);
    }

    #[test]
    fn a_character_cut_at_the_sniff_boundary_is_still_text() {
        // "é" is two bytes; cutting after the first is not a sign of
        // binary data.
        assert!(utf8_prefix_ok(&"caf\u{e9}".as_bytes()[..4]));
        assert!(!utf8_prefix_ok(b"\xff\xfe text"));
    }
}
