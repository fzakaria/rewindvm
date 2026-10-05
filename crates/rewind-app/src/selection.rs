//! Text selection over the app's text surfaces: the build log, the file
//! viewer, the process tree, the files list, the cards, the notices, the
//! license dialog and the terminal.
//!
//! A selection lives in a surface's own coordinates, a line index and a
//! byte offset into that line's text, never in pixels. The lists draw only
//! the rows on screen, so a selection made by dragging can reach lines that
//! are no longer drawn, and copying still finds their text. The approach is
//! the one gpui-kit's text selection takes (gpui-kit.com/base/text-selection):
//! endpoints anchored in content coordinates, word and line units for
//! double and triple clicks, and three kinds of highlight on screen: the
//! rest of the first line, whole middle lines, and the start of the last.
//!
//! The UI layer (`ui::selectable`) turns pointer positions into `Pos`
//! values with the text layouts GPUI made, and paints `range_in_line`.

use std::cmp::Ordering;
use std::ops::Range;

/// A text surface: one panel, card or dialog whose lines select together.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Surface {
    Log,
    Processes,
    Files,
    Viewer,
    /// The source panel's lines.
    Source,
    /// The last event at the playhead.
    EventCard,
    /// Where the run parts from the one it is compared with.
    Divergence,
    /// The latest fork made from the playhead.
    ForkCard,
    /// A notice, by its id.
    Notice(u64),
    LicenseDialog,
    Terminal,
    /// The Runs panel's rows.
    Runs,
}

/// A place in a surface's text: a line and a byte offset into it, always
/// on a character boundary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Pos {
    pub line: usize,
    pub offset: usize,
}

impl Pos {
    pub const fn new(line: usize, offset: usize) -> Pos {
        Pos { line, offset }
    }
}

impl PartialOrd for Pos {
    fn partial_cmp(&self, other: &Pos) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Pos {
    fn cmp(&self, other: &Pos) -> Ordering {
        (self.line, self.offset).cmp(&(other.line, other.offset))
    }
}

/// What a press selects and a drag extends by: characters after one
/// click, words after two, whole lines after three or more.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    Char,
    Word,
    Line,
}

impl Unit {
    /// The unit for a press with this click count.
    pub fn for_clicks(clicks: usize) -> Unit {
        const DOUBLE: usize = 2;
        match clicks {
            0 | 1 => Unit::Char,
            DOUBLE => Unit::Word,
            _ => Unit::Line,
        }
    }
}

/// The text of a surface, as the selection reads it.
pub trait Lines {
    fn line_count(&self) -> usize;

    /// The text of line `line` as it is shown, which offsets index.
    fn shown(&self, line: usize) -> Option<String>;

    /// The text to copy for `range` of line `line`. By default the shown
    /// text; a line that shortens what it shows copies the full text.
    fn copied(&self, line: usize, range: Range<usize>) -> Option<String> {
        let text = self.shown(line)?;
        let range = clamp_range(&text, range);
        Some(text[range].to_string())
    }
}

/// Lines borrowed from elsewhere, like the terminal's session.
impl<T: Lines + ?Sized> Lines for &T {
    fn line_count(&self) -> usize {
        (**self).line_count()
    }

    fn shown(&self, line: usize) -> Option<String> {
        (**self).shown(line)
    }

    fn copied(&self, line: usize, range: Range<usize>) -> Option<String> {
        (**self).copied(line, range)
    }
}

/// Lines held in memory, for the cards and tests.
impl Lines for [String] {
    fn line_count(&self) -> usize {
        self.len()
    }

    fn shown(&self, line: usize) -> Option<String> {
        self.get(line).cloned()
    }
}

impl Lines for Vec<String> {
    fn line_count(&self) -> usize {
        self.len()
    }

    fn shown(&self, line: usize) -> Option<String> {
        self.get(line).cloned()
    }
}

/// A line shown differently from what it copies: store paths shortened,
/// tabs drawn as spaces. Offsets index the shown text; copying a range
/// gives the original text it covers, and a range that reaches into a
/// shortened part takes all of what it stands for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mapped {
    pub shown: String,
    original: String,
    splices: Vec<Splice>,
}

/// A part of the shown text that stands for a different part of the
/// original.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Splice {
    pub shown: Range<usize>,
    pub original: Range<usize>,
}

/// Which end of a copied range an offset is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Edge {
    Start,
    End,
}

impl Mapped {
    /// Text shown as it is.
    pub fn plain(text: impl Into<String>) -> Mapped {
        let text = text.into();
        Mapped {
            shown: text.clone(),
            original: text,
            splices: Vec::new(),
        }
    }

    /// Shown text made from `original` by the replacements in `splices`,
    /// in order.
    pub fn new(shown: String, original: String, splices: Vec<Splice>) -> Mapped {
        Mapped {
            shown,
            original,
            splices,
        }
    }

    /// `prefix`, shown and copied as it is, then this.
    pub fn prefixed(self, prefix: &str) -> Mapped {
        let (s, o) = (prefix.len(), prefix.len());
        Mapped {
            shown: format!("{prefix}{}", self.shown),
            original: format!("{prefix}{}", self.original),
            splices: self
                .splices
                .into_iter()
                .map(|sp| Splice {
                    shown: sp.shown.start + s..sp.shown.end + s,
                    original: sp.original.start + o..sp.original.end + o,
                })
                .collect(),
        }
    }

    /// The original text a range of the shown text covers.
    pub fn copy(&self, range: Range<usize>) -> String {
        let start = self.to_original(range.start, Edge::Start);
        let end = self.to_original(range.end, Edge::End).max(start);
        self.original
            .get(start..end)
            .map(str::to_string)
            .unwrap_or_default()
    }

    /// The offset into the shown text of `original`, an offset into the
    /// original text: an offset inside a part shown differently maps to
    /// where that part's shown text starts.
    pub fn shown_offset(&self, original: usize) -> usize {
        let mut delta: isize = 0;
        for splice in &self.splices {
            if original <= splice.original.start {
                break;
            }
            if original < splice.original.end {
                return splice.shown.start;
            }
            delta = splice.shown.end as isize - splice.original.end as isize;
        }
        (original as isize + delta).clamp(0, self.shown.len() as isize) as usize
    }

    fn to_original(&self, offset: usize, edge: Edge) -> usize {
        let mut delta: isize = 0;
        for splice in &self.splices {
            if offset <= splice.shown.start {
                break;
            }
            if offset < splice.shown.end {
                return match edge {
                    Edge::Start => splice.original.start,
                    Edge::End => splice.original.end,
                };
            }
            delta = splice.original.end as isize - splice.shown.end as isize;
        }
        (offset as isize + delta).clamp(0, self.original.len() as isize) as usize
    }
}

/// Lines of mapped text, for the cards, notices and short lists.
impl Lines for [Mapped] {
    fn line_count(&self) -> usize {
        self.len()
    }

    fn shown(&self, line: usize) -> Option<String> {
        self.get(line).map(|m| m.shown.clone())
    }

    fn copied(&self, line: usize, range: Range<usize>) -> Option<String> {
        self.get(line).map(|m| m.copy(range))
    }
}

impl Lines for Vec<Mapped> {
    fn line_count(&self) -> usize {
        self.len()
    }

    fn shown(&self, line: usize) -> Option<String> {
        self.as_slice().shown(line)
    }

    fn copied(&self, line: usize, range: Range<usize>) -> Option<String> {
        self.as_slice().copied(line, range)
    }
}

/// A selection in one surface. The anchor is the unit the first press
/// selected, and stays selected while the head moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub surface: Surface,
    pub unit: Unit,
    anchor_start: Pos,
    anchor_end: Pos,
    head: Pos,
}

impl Selection {
    /// A press at `at` with `unit`: nothing selected for a single click,
    /// the word or the line under the pointer for more.
    pub fn press(surface: Surface, at: Pos, unit: Unit, lines: &dyn Lines) -> Selection {
        let unit_range = unit_at(at, unit, lines);
        Selection {
            surface,
            unit,
            anchor_start: unit_range.start,
            anchor_end: unit_range.end,
            head: at,
        }
    }

    /// Everything in the surface.
    pub fn all(surface: Surface, lines: &dyn Lines) -> Selection {
        let count = lines.line_count();
        let end = match count.checked_sub(1) {
            Some(last) => Pos::new(last, lines.shown(last).map_or(0, |t| t.len())),
            None => Pos::default(),
        };
        Selection {
            surface,
            unit: Unit::Char,
            anchor_start: Pos::default(),
            anchor_end: Pos::default(),
            head: end,
        }
    }

    /// Moves the head to `at`, as a drag or a shift-click does. The unit
    /// the selection was made with decides how far it reaches: a word
    /// selection grows by whole words, a line selection by whole lines.
    pub fn extend_to(&mut self, at: Pos) {
        self.head = at;
    }

    /// The selected range, start before end, with the head's unit
    /// widened when the selection was made by words or lines.
    pub fn range(&self, lines: &dyn Lines) -> Range<Pos> {
        let head_unit = unit_at(self.head, self.unit, lines);
        if self.head < self.anchor_start {
            head_unit.start..self.anchor_end
        } else {
            self.anchor_start..head_unit.end.max(self.anchor_end)
        }
    }

    /// Whether nothing is selected.
    pub fn is_empty(&self, lines: &dyn Lines) -> bool {
        let range = self.range(lines);
        range.start == range.end
    }

    /// The selected part of line `line`, if any, as a byte range of its
    /// shown text. A line inside the selection is selected to its end,
    /// and an empty line inside it selects its zero width.
    pub fn range_in_line(
        &self,
        line: usize,
        len: usize,
        lines: &dyn Lines,
    ) -> Option<Range<usize>> {
        part_of_line(&self.range(lines), line, len)
    }

    /// The selected text, lines joined by newlines.
    pub fn text(&self, lines: &dyn Lines) -> String {
        let range = self.range(lines);
        let last = lines.line_count().saturating_sub(1);
        let mut out = String::new();
        for line in range.start.line..=range.end.line.min(last) {
            let Some(shown) = lines.shown(line) else {
                continue;
            };
            let start = if line == range.start.line {
                range.start.offset
            } else {
                0
            };
            let end = if line == range.end.line {
                range.end.offset
            } else {
                shown.len()
            };
            if line > range.start.line {
                out.push('\n');
            }
            if let Some(text) = lines.copied(line, start..end) {
                out.push_str(&text);
            }
        }
        out
    }
}

/// The part of line `line`, `len` bytes long, that `range` selects. The
/// UI works out the range once per frame and asks this for every row.
pub fn part_of_line(range: &Range<Pos>, line: usize, len: usize) -> Option<Range<usize>> {
    if range.start == range.end || line < range.start.line || line > range.end.line {
        return None;
    }
    let start = if line == range.start.line {
        range.start.offset.min(len)
    } else {
        0
    };
    let end = if line == range.end.line {
        range.end.offset.min(len)
    } else {
        len
    };
    (start <= end).then_some(start..end)
}

/// The range of the unit at `at`: the position itself, the word around
/// it, or its whole line.
fn unit_at(at: Pos, unit: Unit, lines: &dyn Lines) -> Range<Pos> {
    match unit {
        Unit::Char => at..at,
        Unit::Word => {
            let text = lines.shown(at.line).unwrap_or_default();
            let word = word_at(&text, at.offset);
            Pos::new(at.line, word.start)..Pos::new(at.line, word.end)
        }
        Unit::Line => {
            let len = lines.shown(at.line).map_or(0, |t| t.len());
            Pos::new(at.line, 0)..Pos::new(at.line, len)
        }
    }
}

/// What a character is, for finding words: a run of the same kind is one
/// word, as editors treat double clicks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CharKind {
    Space,
    Word,
    Punctuation,
}

fn kind(c: char) -> CharKind {
    if c.is_whitespace() {
        CharKind::Space
    } else if c.is_alphanumeric() || c == '_' {
        CharKind::Word
    } else {
        CharKind::Punctuation
    }
}

/// The word around byte `offset` of `text`: the run of characters of the
/// same kind as the one at the offset, or as the one before it at the end
/// of the line.
pub fn word_at(text: &str, offset: usize) -> Range<usize> {
    let offset = floor_boundary(text, offset.min(text.len()));
    let Some(here) = text[offset..]
        .chars()
        .next()
        .or_else(|| text[..offset].chars().next_back())
    else {
        return offset..offset;
    };
    let wanted = kind(here);
    let probe = if offset == text.len() {
        offset - here.len_utf8()
    } else {
        offset
    };

    // Walk back, then forward, while the characters are of the same kind.
    let start = text[..probe]
        .char_indices()
        .rev()
        .take_while(|&(_, c)| kind(c) == wanted)
        .last()
        .map_or(probe, |(i, _)| i);
    let end = text[probe..]
        .char_indices()
        .take_while(|&(_, c)| kind(c) == wanted)
        .last()
        .map_or(probe, |(i, c)| probe + i + c.len_utf8());
    start..end
}

/// The nearest character boundary at or before `offset`.
pub fn floor_boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

/// `range` cut to `text` and moved onto character boundaries.
fn clamp_range(text: &str, range: Range<usize>) -> Range<usize> {
    let start = floor_boundary(text, range.start);
    let end = floor_boundary(text, range.end).max(start);
    start..end
}

/// Which end a line was cut at to fit its column, with an ellipsis put in
/// for what was left out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cut {
    /// Drawn as written.
    None,
    /// The end was cut: "start of the line…".
    End { kept: usize },
    /// The start was cut: "…end of the line".
    Start { kept: usize },
}

/// How the text drawn on screen relates to the line's text, which
/// selection offsets index. GPUI draws a line that does not fit its
/// column with an ellipsis in place of the end or the start, and its
/// layout then indexes the drawn text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DrawnText {
    cut: Cut,
    /// The byte length of the line's text.
    len: usize,
    /// The byte length of the ellipsis drawn in place of the cut part.
    ellipsis: usize,
}

impl DrawnText {
    /// Works out how `drawn` was made from `text`.
    pub fn new(text: &str, drawn: &str, ellipsis: &str) -> DrawnText {
        let len = text.len();
        let plain = DrawnText {
            cut: Cut::None,
            len,
            ellipsis: ellipsis.len(),
        };
        if drawn == text || ellipsis.is_empty() {
            return plain;
        }
        if let Some(kept) = drawn.strip_suffix(ellipsis)
            && text.starts_with(kept)
        {
            return DrawnText {
                cut: Cut::End { kept: kept.len() },
                ..plain
            };
        }
        if let Some(kept) = drawn.strip_prefix(ellipsis)
            && text.ends_with(kept)
        {
            return DrawnText {
                cut: Cut::Start { kept: kept.len() },
                ..plain
            };
        }
        plain
    }

    /// The offset into the line's text for an offset into the drawn text.
    /// A point on the ellipsis stands for the part it replaced, up to its
    /// far edge.
    pub fn text_offset(&self, drawn: usize) -> usize {
        match self.cut {
            Cut::None => drawn.min(self.len),
            Cut::End { kept } if drawn <= kept => drawn,
            Cut::End { .. } => self.len,
            Cut::Start { kept } => {
                let skipped = self.len - kept;
                match drawn.checked_sub(self.ellipsis) {
                    Some(into_kept) => skipped + into_kept.min(kept),
                    None if drawn == 0 => 0,
                    None => skipped,
                }
            }
        }
    }

    /// The offset into the drawn text for an offset into the line's text,
    /// for painting a selection: the cut part maps onto the ellipsis.
    pub fn drawn_offset(&self, text: usize) -> usize {
        match self.cut {
            Cut::None => text,
            Cut::End { kept } if text <= kept => text,
            Cut::End { kept } => kept + self.ellipsis,
            Cut::Start { kept } => {
                let skipped = self.len - kept;
                match text.checked_sub(skipped) {
                    Some(into_kept) => self.ellipsis + into_kept,
                    None if text == 0 => 0,
                    None => self.ellipsis,
                }
            }
        }
    }
}

/// The line of a surface a pointer at height `y` is on, from the lines
/// drawn on screen as (line, top, bottom). A pointer between two lines or
/// past the drawn ones takes the nearest, so a drag that leaves a list
/// keeps selecting to its edge.
pub fn nearest_line(drawn: &[(usize, f32, f32)], y: f32) -> Option<usize> {
    drawn
        .iter()
        .min_by(|a, b| {
            distance(y, a.1, a.2)
                .total_cmp(&distance(y, b.1, b.2))
                .then(a.0.cmp(&b.0))
        })
        .map(|&(line, _, _)| line)
}

fn distance(y: f32, top: f32, bottom: f32) -> f32 {
    if y < top {
        top - y
    } else if y > bottom {
        y - bottom
    } else {
        0.0
    }
}

/// The column of a fixed-width grid a pointer at `x` is nearest to, from
/// the grid's left edge and its cell width: the boundary between two cells
/// is where a selection starts or ends.
pub fn nearest_column(x: f32, left: f32, cell_width: f32, columns: usize) -> usize {
    if cell_width <= 0.0 {
        return 0;
    }
    let cells = ((x - left) / cell_width).round();
    (cells.max(0.0) as usize).min(columns)
}

/// The byte offset of character column `column` in `text`, or its end.
pub fn offset_of_column(text: &str, column: usize) -> usize {
    text.char_indices()
        .nth(column)
        .map_or(text.len(), |(i, _)| i)
}

#[cfg(test)]
mod tests {
    // The selection model on lines held in memory: presses and drags in
    // characters, words and lines, the selected range on each line, the
    // copied string, and the mapping from pointer positions to offsets.
    use super::*;

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_drag_selects_from_the_press_to_the_pointer_across_lines() {
        // A press on line 0 and a drag to line 2 select the rest of line 0,
        // all of line 1 and the start of line 2; copying joins them with
        // newlines.
        let text = lines(&["job 3 done", "worker picked job 4", "job 4 done"]);
        let mut sel = Selection::press(Surface::Log, Pos::new(0, 4), Unit::Char, &text);
        assert!(sel.is_empty(&text));
        sel.extend_to(Pos::new(2, 5));
        assert_eq!(sel.range_in_line(0, 10, &text), Some(4..10));
        assert_eq!(sel.range_in_line(1, 19, &text), Some(0..19));
        assert_eq!(sel.range_in_line(2, 10, &text), Some(0..5));
        assert_eq!(sel.range_in_line(3, 3, &text), None);
        assert_eq!(sel.text(&text), "3 done\nworker picked job 4\njob 4");
    }

    #[test]
    fn a_drag_upward_selects_the_same_text_backward() {
        // The head above the anchor orders the range the other way.
        let text = lines(&["alpha beta", "gamma"]);
        let mut sel = Selection::press(Surface::Viewer, Pos::new(1, 3), Unit::Char, &text);
        sel.extend_to(Pos::new(0, 6));
        assert_eq!(sel.text(&text), "beta\ngam");
    }

    #[test]
    fn a_double_click_selects_a_word_and_a_drag_grows_by_words() {
        // Letters, digits and underscores make a word; slashes and dots
        // are words of their own kind.
        let text = lines(&["/build/mylib/tests/test_pool_shutdown done"]);
        let mut sel = Selection::press(Surface::Files, Pos::new(0, 22), Unit::Word, &text);
        assert_eq!(sel.text(&text), "test_pool_shutdown");
        sel.extend_to(Pos::new(0, 40));
        assert_eq!(sel.text(&text), "test_pool_shutdown done");
        sel.extend_to(Pos::new(0, 8));
        assert_eq!(sel.text(&text), "mylib/tests/test_pool_shutdown");
    }

    #[test]
    fn a_triple_click_selects_the_line_and_a_drag_grows_by_lines() {
        let text = lines(&["one", "two", "three"]);
        let mut sel = Selection::press(Surface::Log, Pos::new(1, 1), Unit::Line, &text);
        assert_eq!(sel.text(&text), "two");
        sel.extend_to(Pos::new(2, 0));
        assert_eq!(sel.text(&text), "two\nthree");
        sel.extend_to(Pos::new(0, 2));
        assert_eq!(sel.text(&text), "one\ntwo");
    }

    #[test]
    fn select_all_covers_every_line() {
        let text = lines(&["first", "", "last"]);
        let sel = Selection::all(Surface::Log, &text);
        assert_eq!(sel.text(&text), "first\n\nlast");
        assert_eq!(sel.range_in_line(1, 0, &text), Some(0..0));
        assert!(Selection::all(Surface::Log, &lines(&[])).is_empty(&lines(&[])));
    }

    #[test]
    fn click_counts_pick_the_unit() {
        assert_eq!(Unit::for_clicks(1), Unit::Char);
        assert_eq!(Unit::for_clicks(2), Unit::Word);
        assert_eq!(Unit::for_clicks(3), Unit::Line);
        assert_eq!(Unit::for_clicks(5), Unit::Line);
    }

    #[test]
    fn words_stop_at_a_change_of_kind_and_respect_multibyte_text() {
        // The end of a line takes the word before it; an offset inside a
        // multibyte character moves to its start.
        assert_eq!(word_at("write(1, \"job\")", 2), 0..5);
        assert_eq!(word_at("write(1, \"job\")", 5), 5..6);
        assert_eq!(word_at("a  b", 2), 1..3);
        assert_eq!(word_at("caf\u{e9} au", 4), 0..5);
        assert_eq!(word_at("end", 3), 0..3);
        assert_eq!(word_at("", 0), 0..0);
    }

    /// Lines that show store paths shortened and copy them in full, as
    /// the log does.
    struct Shortened(Vec<(String, String)>);

    impl Lines for Shortened {
        fn line_count(&self) -> usize {
            self.0.len()
        }
        fn shown(&self, line: usize) -> Option<String> {
            self.0.get(line).map(|(shown, _)| shown.clone())
        }
        fn copied(&self, line: usize, range: Range<usize>) -> Option<String> {
            let (shown, full) = self.0.get(line)?;
            if range == (0..shown.len()) {
                return Some(full.clone());
            }
            Some(shown[range].to_string())
        }
    }

    #[test]
    fn copying_asks_each_line_for_its_own_text() {
        // A line can copy more than it shows: here a whole line copies its
        // full store path.
        let text = Shortened(vec![
            (
                "/nix/store/ab\u{2026}-x".into(),
                "/nix/store/abcdef-x".into(),
            ),
            ("next".into(), "next".into()),
        ]);
        let mut sel = Selection::press(Surface::Log, Pos::new(0, 0), Unit::Char, &text);
        sel.extend_to(Pos::new(1, 2));
        assert_eq!(sel.text(&text), "/nix/store/abcdef-x\nne");
    }

    #[test]
    fn mapped_text_copies_the_original_it_stands_for() {
        // "ab…-x" shows "abcdef-x": a range inside or touching the
        // shortened part copies all of it, a range outside copies itself,
        // and a prefix shifts both.
        let shown = "ab\u{2026}-x".to_string();
        let ell = '\u{2026}'.len_utf8();
        let mapped = Mapped::new(
            shown.clone(),
            "abcdef-x".into(),
            vec![Splice {
                shown: 2..2 + ell,
                original: 2..6,
            }],
        );
        assert_eq!(mapped.copy(0..shown.len()), "abcdef-x");
        assert_eq!(mapped.copy(0..2), "ab");
        assert_eq!(mapped.copy(3..shown.len()), "cdef-x");
        assert_eq!(mapped.copy(2 + ell..shown.len()), "-x");
        let prefixed = mapped.prefixed("165   ");
        assert_eq!(prefixed.shown, format!("165   {shown}"));
        assert_eq!(prefixed.copy(0..prefixed.shown.len()), "165   abcdef-x");
        assert_eq!(prefixed.copy(0..3), "165");
        assert_eq!(Mapped::plain("same").copy(1..3), "am");
    }

    /// Offsets into a line's original text map onto its shown text, for
    /// coloring parts of it: "a\tb\tc" shown with each tab as four
    /// spaces moves each offset past a tab by three, and an offset at a
    /// tab maps to where its spaces start.
    #[test]
    fn original_offsets_map_onto_the_shown_text() {
        let mapped = Mapped::new(
            "a    b    c".into(),
            "a\tb\tc".into(),
            vec![
                Splice {
                    shown: 1..5,
                    original: 1..2,
                },
                Splice {
                    shown: 6..10,
                    original: 3..4,
                },
            ],
        );
        let shown: Vec<usize> = (0..=5).map(|o| mapped.shown_offset(o)).collect();
        assert_eq!(shown, vec![0, 1, 5, 6, 10, 11]);
        assert_eq!(Mapped::plain("same").shown_offset(2), 2);
    }

    #[test]
    fn drawn_text_cut_at_the_end_maps_back_to_the_line() {
        // "build/out…" drawn for "build/output.log": offsets before the
        // ellipsis are the same, the ellipsis stands for the rest.
        let drawn = DrawnText::new("build/output.log", "build/out\u{2026}", "\u{2026}");
        assert_eq!(drawn.text_offset(3), 3);
        assert_eq!(drawn.text_offset(10), 16);
        assert_eq!(drawn.drawn_offset(12), 12);
        assert_eq!(drawn.drawn_offset(5), 5);
    }

    #[test]
    fn drawn_text_cut_at_the_start_maps_back_to_the_line() {
        // "…tests/t" drawn for "/build/mylib/tests/t": the kept end is
        // offset by what was cut, the ellipsis by nothing.
        let text = "/build/mylib/tests/t";
        let drawn = DrawnText::new(text, "\u{2026}tests/t", "\u{2026}");
        let ellipsis = '\u{2026}'.len_utf8();
        assert_eq!(drawn.text_offset(0), 0);
        assert_eq!(drawn.text_offset(ellipsis), 13);
        assert_eq!(drawn.text_offset(ellipsis + 5), 18);
        assert_eq!(drawn.drawn_offset(18), ellipsis + 5);
        assert_eq!(drawn.drawn_offset(4), ellipsis);
        assert_eq!(DrawnText::new("same", "same", "\u{2026}").text_offset(2), 2);
    }

    #[test]
    fn a_pointer_takes_the_nearest_drawn_line() {
        // Rows 22 pixels tall from y 100; above the first and below the
        // last row the edge rows are taken.
        let drawn = [(7, 100.0, 122.0), (8, 122.0, 144.0), (9, 144.0, 166.0)];
        assert_eq!(nearest_line(&drawn, 130.0), Some(8));
        assert_eq!(nearest_line(&drawn, 20.0), Some(7));
        assert_eq!(nearest_line(&drawn, 900.0), Some(9));
        assert_eq!(nearest_line(&[], 1.0), None);
    }

    #[test]
    fn a_pointer_in_a_grid_takes_the_nearest_cell_boundary() {
        // Cells 8 pixels wide from x 10: a point past the middle of a cell
        // rounds to its right edge, and the grid's edges bound the column.
        assert_eq!(nearest_column(10.0, 10.0, 8.0, 80), 0);
        assert_eq!(nearest_column(23.0, 10.0, 8.0, 80), 2);
        assert_eq!(nearest_column(19.0, 10.0, 8.0, 80), 1);
        assert_eq!(nearest_column(-5.0, 10.0, 8.0, 80), 0);
        assert_eq!(nearest_column(5000.0, 10.0, 8.0, 80), 80);
        assert_eq!(offset_of_column("a\u{e9}b", 2), 3);
        assert_eq!(offset_of_column("ab", 9), 2);
    }
}
