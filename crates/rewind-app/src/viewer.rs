//! A file's contents as the viewer shows them: text as lines, anything
//! else as a hex dump, both cut to a size a panel can hold.

use crate::selection::{Lines, Mapped, Splice};

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
        let (kind, lines) = if is_text {
            let text = String::from_utf8_lossy(shown);
            (Kind::Text, text.lines().map(str::to_string).collect())
        } else {
            let lines = shown
                .chunks(HEX_WIDTH)
                .enumerate()
                .map(|(i, chunk)| hex_line(i * HEX_WIDTH, chunk))
                .collect();
            (Kind::Binary, lines)
        };
        View {
            kind,
            lines,
            size: bytes.len(),
            truncated,
        }
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
