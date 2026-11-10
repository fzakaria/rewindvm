//! How one file differs between two runs: line by line for text, byte by
//! byte for anything else. The desktop app's two-run file view and
//! `rewind diff --file` both draw on it.
//!
//! Lines are matched with Myers' diff ("An O(ND) Difference Algorithm and
//! Its Variations", 1986), which finds the fewest lines to take out of one
//! copy and put into it to make the other. Bytes are compared by their
//! offset, as `cmp` does: a build that differs in a timestamp or a hash
//! keeps its size, and each byte that differs is where it was.

use std::ops::Range;

/// The most bytes of each copy that are compared, and what the app reads
/// of a file; past this, a caller says only the first part was compared.
pub const MAX_COMPARED: usize = 1 << 20;

/// A copy counts as binary when its first bytes hold a NUL or are not
/// UTF-8, as the app's file viewer tells them apart. Checking this many is
/// enough to tell.
const SNIFF: usize = 8 << 10;

/// The most lines Myers' diff may take out and put in before it gives up
/// on the shortest match: its time and memory grow with the square of
/// this. Copies further apart are matched roughly instead.
pub const MAX_EDITS: usize = 1000;

/// What a summary says last when the lines were matched roughly.
pub const ROUGH_NOTE: &str = "Too many lines differ to match them up one by one. Everything between the first and the last difference is shown as changed.";

/// How a copy's bytes read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Text,
    Binary,
}

/// Whether `bytes` read as text or as binary.
pub fn kind_of(bytes: &[u8]) -> Kind {
    let sniff = &bytes[..bytes.len().min(SNIFF)];
    if sniff.contains(&0) || !utf8_prefix_ok(sniff) {
        return Kind::Binary;
    }
    Kind::Text
}

/// Whether bytes are UTF-8, allowing a character cut in two at the end.
fn utf8_prefix_ok(bytes: &[u8]) -> bool {
    const MAX_CHAR_LEN: usize = 4;
    match std::str::from_utf8(bytes) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none() && bytes.len() - e.valid_up_to() < MAX_CHAR_LEN,
    }
}

/// One line of a line diff: a line both copies have, by its index in
/// each, or a line only one copy has, by its index there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Line {
    Both { a: usize, b: usize },
    OnlyA(usize),
    OnlyB(usize),
}

/// How well a line diff matched the copies' lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Alignment {
    /// The fewest lines only one copy has.
    Shortest,
    /// More than `MAX_EDITS` lines differ: the lines both copies start
    /// and end with are matched, and everything between is only in one.
    Rough,
}

/// Two copies' lines matched up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineDiff {
    /// Every line of both copies in order. Where the copies differ, the
    /// lines only in `a` come before the lines only in `b`.
    pub lines: Vec<Line>,
    pub alignment: Alignment,
}

/// The lines of copies `a` and `b` matched up.
pub fn line_diff<T: PartialEq>(a: &[T], b: &[T]) -> LineDiff {
    // The lines both copies start and end with match as they are, and
    // only the middle is left to Myers' diff.
    let start = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let end = a[start..]
        .iter()
        .rev()
        .zip(b[start..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (middle_a, middle_b) = (&a[start..a.len() - end], &b[start..b.len() - end]);

    // The middle, matched as closely as MAX_EDITS allows, with its
    // indices moved back to the whole copies'.
    let (middle, alignment) = match myers(middle_a, middle_b) {
        Some(lines) => (lines, Alignment::Shortest),
        None => {
            let only_a = (0..middle_a.len()).map(Line::OnlyA);
            let only_b = (0..middle_b.len()).map(Line::OnlyB);
            (only_a.chain(only_b).collect(), Alignment::Rough)
        }
    };
    let shifted = middle.into_iter().map(|line| match line {
        Line::Both { a, b } => Line::Both {
            a: a + start,
            b: b + start,
        },
        Line::OnlyA(a) => Line::OnlyA(a + start),
        Line::OnlyB(b) => Line::OnlyB(b + start),
    });

    // The shared start, the middle, then the shared end.
    let mut lines: Vec<Line> = (0..start).map(|i| Line::Both { a: i, b: i }).collect();
    lines.extend(shifted);
    lines.extend((0..end).map(|i| Line::Both {
        a: a.len() - end + i,
        b: b.len() - end + i,
    }));
    LineDiff {
        lines: lines_only_in_a_first(lines),
        alignment,
    }
}

/// The lines of `a` and `b` matched with the fewest lines only in one, or
/// None when that takes more than `MAX_EDITS`.
///
/// Myers' greedy forward search: `furthest[k]` is how far into `a` the
/// best path so far reaches on diagonal k, where k is the line of `a`
/// less the line of `b`. Each round d allows one more line taken out or
/// put in, then follows matching lines as far as they go. A copy of
/// `furthest` before each round is kept to walk the path back.
fn myers<T: PartialEq>(a: &[T], b: &[T]) -> Option<Vec<Line>> {
    let (n, m) = (a.len() as isize, b.len() as isize);
    let most = (a.len() + b.len()).min(MAX_EDITS) as isize;

    // Diagonal k is kept at index k + offset; one more on each side for
    // the neighbors the outermost diagonals look at.
    let offset = most + 1;
    let mut furthest = vec![0isize; 2 * most as usize + 3];
    let mut rounds: Vec<Vec<isize>> = Vec::new();
    for d in 0..=most {
        rounds.push(furthest.clone());
        for k in (-d..=d).step_by(2) {
            let i = (k + offset) as usize;

            // Down from diagonal k + 1 (a line put in), or right from
            // k - 1 (a line taken out), whichever reaches further.
            let down = k == -d || (k != d && furthest[i - 1] < furthest[i + 1]);
            let mut x = if down {
                furthest[i + 1]
            } else {
                furthest[i - 1] + 1
            };
            let mut y = x - k;

            // Then along the lines both have.
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            furthest[i] = x;
            if x >= n && y >= m {
                return Some(walk_back(&rounds, offset, n, m));
            }
        }
    }
    None
}

/// The path Myers' search found to the ends of copies `n` and `m` lines
/// long, from the copies of its furthest points kept before each round.
fn walk_back(rounds: &[Vec<isize>], offset: isize, n: isize, m: isize) -> Vec<Line> {
    let (mut x, mut y) = (n, m);
    let mut backwards = Vec::new();
    for (d, furthest) in rounds.iter().enumerate().rev() {
        let d = d as isize;
        let k = x - y;
        let i = (k + offset) as usize;

        // The diagonal this round's step came from, as the search chose it.
        let down = k == -d || (k != d && furthest[i - 1] < furthest[i + 1]);
        let previous_k = if down { k + 1 } else { k - 1 };
        let previous_x = furthest[(previous_k + offset) as usize];
        let previous_y = previous_x - previous_k;

        // The lines both have, back to where the step ended.
        while x > previous_x && y > previous_y {
            x -= 1;
            y -= 1;
            backwards.push(Line::Both {
                a: x as usize,
                b: y as usize,
            });
        }

        // The step itself: none before the first round.
        if d == 0 {
            break;
        }
        if x == previous_x {
            backwards.push(Line::OnlyB(previous_y as usize));
        } else {
            backwards.push(Line::OnlyA(previous_x as usize));
        }
        (x, y) = (previous_x, previous_y);
    }
    backwards.reverse();
    backwards
}

/// `lines` with each stretch of lines only in one copy put in order: the
/// lines only in `a`, then the lines only in `b`, as a unified diff
/// writes them.
fn lines_only_in_a_first(lines: Vec<Line>) -> Vec<Line> {
    let mut ordered = Vec::with_capacity(lines.len());
    let mut only_b = Vec::new();
    for line in lines {
        match line {
            Line::OnlyA(_) => ordered.push(line),
            Line::OnlyB(_) => only_b.push(line),
            Line::Both { .. } => {
                ordered.append(&mut only_b);
                ordered.push(line);
            }
        }
    }
    ordered.append(&mut only_b);
    ordered
}

/// A row of a side-by-side view: a line both copies have, a line each has
/// in the other's place, or a line only one has, by index in each copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    Same { a: usize, b: usize },
    Changed { a: usize, b: usize },
    OnlyA(usize),
    OnlyB(usize),
}

/// The rows of a side-by-side view of `lines`: where the copies differ,
/// the lines only in `a` are paired in order with the lines only in `b`,
/// and the lines left over have a row of their own.
pub fn side_by_side(lines: &[Line]) -> Vec<Row> {
    let mut rows = Vec::with_capacity(lines.len());
    let (mut only_a, mut only_b) = (Vec::new(), Vec::new());
    for line in lines {
        match *line {
            Line::OnlyA(a) => only_a.push(a),
            Line::OnlyB(b) => only_b.push(b),
            Line::Both { a, b } => {
                pair_up(&mut rows, &mut only_a, &mut only_b);
                rows.push(Row::Same { a, b });
            }
        }
    }
    pair_up(&mut rows, &mut only_a, &mut only_b);
    rows
}

/// Rows for one stretch where the copies differ: its lines only in `a`
/// beside its lines only in `b`, in order, and the rest alone.
fn pair_up(rows: &mut Vec<Row>, only_a: &mut Vec<usize>, only_b: &mut Vec<usize>) {
    let paired = only_a.len().min(only_b.len());
    for (&a, &b) in only_a.iter().zip(only_b.iter()) {
        rows.push(Row::Changed { a, b });
    }
    rows.extend(only_a[paired..].iter().map(|&a| Row::OnlyA(a)));
    rows.extend(only_b[paired..].iter().map(|&b| Row::OnlyB(b)));
    only_a.clear();
    only_b.clear();
}

/// How two copies' bytes differ, offset by offset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ByteDiff {
    /// Each stretch of offsets where the bytes differ, in order. Offsets
    /// past the end of the shorter copy differ.
    pub ranges: Vec<Range<usize>>,
    /// How many offsets differ.
    pub differing: usize,
    /// How many offsets were compared: the longer copy's length.
    pub compared: usize,
}

impl ByteDiff {
    /// The first offset that differs.
    pub fn first(&self) -> Option<usize> {
        self.ranges.first().map(|r| r.start)
    }

    /// Whether the byte at `offset` differs.
    pub fn differs_at(&self, offset: usize) -> bool {
        let after = self.ranges.partition_point(|r| r.start <= offset);
        after
            .checked_sub(1)
            .is_some_and(|i| self.ranges[i].contains(&offset))
    }
}

/// How copies `a` and `b` differ, offset by offset.
pub fn byte_diff(a: &[u8], b: &[u8]) -> ByteDiff {
    let compared = a.len().max(b.len());
    let mut ranges: Vec<Range<usize>> = Vec::new();
    for offset in 0..compared {
        if a.get(offset) == b.get(offset) {
            continue;
        }

        // A byte next to the last stretch makes it longer.
        match ranges.last_mut() {
            Some(last) if last.end == offset => last.end += 1,
            _ => ranges.push(offset..offset + 1),
        }
    }
    let differing = ranges.iter().map(|r| r.len()).sum();
    ByteDiff {
        ranges,
        differing,
        compared,
    }
}

/// What a summary calls each run, such as "this run" and "the other run",
/// or the runs' ids.
#[derive(Clone, Copy, Debug)]
pub struct Sides<'a> {
    pub a: &'a str,
    pub b: &'a str,
}

/// How one file differs between two copies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileDiff {
    /// The bytes are the same.
    Same,
    /// Both copies are text, matched line by line.
    Text(LineDiff),
    /// Either copy is binary, or the copies' lines are the same and only
    /// how they end differs: compared byte by byte.
    Binary(ByteDiff),
}

/// The lines of a text copy, as the app's file viewer splits them.
pub fn lines_of(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::to_string)
        .collect()
}

/// Where each line [`lines_of`] splits `bytes` into starts in `bytes`,
/// by byte offset.
pub fn line_starts(bytes: &[u8]) -> Vec<usize> {
    if bytes.is_empty() {
        return Vec::new();
    }

    // A line starts at the beginning and after each newline, except the
    // newline that ends the copy, which lines_of starts no line after.
    let mut starts = vec![0];
    for (i, &byte) in bytes.iter().enumerate() {
        if byte == b'\n' && i + 1 < bytes.len() {
            starts.push(i + 1);
        }
    }
    starts
}

/// The byte of `bytes` at `column` of line `line`, both counted from 0,
/// where the line starts at `starts[line]` as [`line_starts`] has it.
/// `column` is a byte offset into the line as [`lines_of`] has it, which
/// is the file's own bytes when the line is UTF-8; in a line that is not,
/// the line's first byte stands for the whole line.
pub fn byte_in_line(bytes: &[u8], starts: &[usize], line: usize, column: usize) -> usize {
    let Some(&start) = starts.get(line) else {
        return bytes.len();
    };

    // The line's own bytes, without its newline and, as lines_of drops
    // it, a carriage return before that.
    let rest = &bytes[start..];
    let mut end = rest.iter().position(|&b| b == b'\n').unwrap_or(rest.len());
    if rest[..end].ends_with(b"\r") {
        end -= 1;
    }
    let own = &rest[..end];

    // Only a UTF-8 line's offsets are the file's.
    if std::str::from_utf8(own).is_err() {
        return start;
    }
    start + column.min(own.len())
}

/// How copies `a` and `b` of one file differ.
pub fn compare(a: &[u8], b: &[u8]) -> FileDiff {
    if a == b {
        return FileDiff::Same;
    }
    if kind_of(a) == Kind::Binary || kind_of(b) == Kind::Binary {
        return FileDiff::Binary(byte_diff(a, b));
    }

    // Text whose lines all match differs only in how they end, which only
    // the bytes show.
    let lines = line_diff(&lines_of(a), &lines_of(b));
    if lines.lines.iter().all(|l| matches!(l, Line::Both { .. })) {
        return FileDiff::Binary(byte_diff(a, b));
    }
    FileDiff::Text(lines)
}

/// What a person reads first about how file `name` differs: one fact a
/// line, the answer first. `a` and `b` are the copies `diff` compared.
pub fn summary(name: &str, a: &[u8], b: &[u8], diff: &FileDiff, sides: Sides) -> Vec<String> {
    match diff {
        FileDiff::Same => vec![format!(
            "{name} is the same in both runs: {} bytes.",
            thousands(a.len())
        )],
        FileDiff::Binary(bytes) => {
            // The count and where to look first, then the sizes when they
            // are not the same.
            let mut said = vec![format!(
                "{name} differs: {} of {} bytes, first at byte {}.",
                thousands(bytes.differing),
                thousands(bytes.compared),
                thousands(bytes.first().unwrap_or(0))
            )];
            if a.len() != b.len() {
                said.push(size_in(sides.a, a.len()));
                said.push(size_in(sides.b, b.len()));
            }
            said
        }
        FileDiff::Text(lines) => {
            // The first line that differs is the same line in both copies:
            // every line before it matched.
            let first = lines
                .lines
                .iter()
                .take_while(|l| matches!(l, Line::Both { .. }))
                .count()
                + 1;
            let only_a = lines
                .lines
                .iter()
                .filter(|l| matches!(l, Line::OnlyA(_)))
                .count();
            let only_b = lines
                .lines
                .iter()
                .filter(|l| matches!(l, Line::OnlyB(_)))
                .count();
            let mut said = vec![
                format!("{name} differs, first at line {}.", thousands(first)),
                format!("Only in {}: {}.", sides.a, count_lines(only_a)),
                format!("Only in {}: {}.", sides.b, count_lines(only_b)),
            ];
            if lines.alignment == Alignment::Rough {
                said.push(ROUGH_NOTE.to_string());
            }
            said
        }
    }
}

/// `MAX_COMPARED` in KiB, as the notes about long copies write it.
fn compared_kib() -> String {
    const KIB: usize = 1024;
    thousands(MAX_COMPARED / KIB)
}

/// What a summary says when a copy was longer than `MAX_COMPARED`.
pub fn cut_note() -> String {
    format!(
        "Only the first {} KiB of each copy was compared.",
        compared_kib()
    )
}

/// The summary for whole copies `a` and `b` of file `name` whose first
/// `MAX_COMPARED` bytes are the same and whose later bytes are not:
/// where the copies first differ, then the sizes when they are not the
/// same.
pub fn differs_past_compared(name: &str, a: &[u8], b: &[u8], sides: Sides) -> Vec<String> {
    // The first offset whose bytes differ, or the shorter copy's length
    // when the longer copy starts with the whole shorter one.
    let first = a
        .iter()
        .zip(b)
        .position(|(x, y)| x != y)
        .unwrap_or(a.len().min(b.len()));
    let mut said = vec![
        format!(
            "{name} differs past the first {} KiB: first at byte {}.",
            compared_kib(),
            thousands(first)
        ),
        format!("The first {} KiB of each copy is the same.", compared_kib()),
    ];
    if a.len() != b.len() {
        said.push(size_in(sides.a, a.len()));
        said.push(size_in(sides.b, b.len()));
    }
    said
}

/// What a summary says of a copy's size in run `side`.
fn size_in(side: &str, bytes: usize) -> String {
    format!("Size in {side}: {} bytes.", thousands(bytes))
}

/// What a summary says of file `name` when only run `side` has it.
pub fn only_in(name: &str, side: &str) -> String {
    format!("{name} is only in {side}.")
}

/// A number of lines, as words.
fn count_lines(n: usize) -> String {
    match n {
        1 => "1 line".to_string(),
        n => format!("{} lines", thousands(n)),
    }
}

/// `n` with its digits in groups of three, as the app writes numbers.
fn thousands(n: usize) -> String {
    const GROUP: usize = 3;
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / GROUP);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(GROUP) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    // The line and byte diffs on small copies written out in each test,
    // checked against the lines and offsets worked out by hand, and the
    // summaries they make.
    use super::*;

    /// The lines of a diff as a unified diff writes them: " " for a line
    /// both copies have, "-" for one only in a, "+" for one only in b.
    fn unified(a: &[&str], b: &[&str], lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|line| match *line {
                Line::Both { a: i, .. } => format!(" {}", a[i]),
                Line::OnlyA(i) => format!("-{}", a[i]),
                Line::OnlyB(j) => format!("+{}", b[j]),
            })
            .collect()
    }

    /// Text, by its NUL-free UTF-8, and binary, by a NUL or bytes that are
    /// not UTF-8; a character cut by the sniff's end is still text.
    #[test]
    fn copies_are_told_apart_as_the_viewer_does() {
        assert_eq!(kind_of(b"prefix=/nix/store\n"), Kind::Text);
        assert_eq!(kind_of(b""), Kind::Text);
        assert_eq!(kind_of(b"\x7fELF\x02\x01\x01\x00"), Kind::Binary);
        assert_eq!(kind_of(b"\xff\xfe text"), Kind::Binary);
        let mut cut = vec![b'a'; SNIFF - 1];
        cut.extend_from_slice("\u{e9}".as_bytes());
        assert_eq!(kind_of(&cut), Kind::Text);
    }

    /// One line changed in the middle of five: the diff keeps the four
    /// both have and shows the old line taken out and the new put in.
    #[test]
    fn a_changed_line_is_taken_out_and_put_in() {
        let a = [
            "CC = cc",
            "CFLAGS = -O1",
            "all: pool",
            "pool: pool.c",
            "clean:",
        ];
        let b = [
            "CC = cc",
            "CFLAGS = -O2",
            "all: pool",
            "pool: pool.c",
            "clean:",
        ];
        let diff = line_diff(&a, &b);
        assert_eq!(diff.alignment, Alignment::Shortest);
        assert_eq!(
            unified(&a, &b, &diff.lines),
            [
                " CC = cc",
                "-CFLAGS = -O1",
                "+CFLAGS = -O2",
                " all: pool",
                " pool: pool.c",
                " clean:"
            ]
        );
    }

    /// Myers' paper's own example, ABCABBA against CBABAC: five lines
    /// taken out or put in is the fewest, and every line of each copy is
    /// there once, in order.
    #[test]
    fn the_diff_finds_the_fewest_changes() {
        let a: Vec<&str> = "A B C A B B A".split(' ').collect();
        let b: Vec<&str> = "C B A B A C".split(' ').collect();
        let diff = line_diff(&a, &b);
        let changes = diff
            .lines
            .iter()
            .filter(|l| !matches!(l, Line::Both { .. }))
            .count();
        assert_eq!(changes, 5);
        let in_a: Vec<usize> = diff
            .lines
            .iter()
            .filter_map(|l| match *l {
                Line::Both { a, .. } | Line::OnlyA(a) => Some(a),
                Line::OnlyB(_) => None,
            })
            .collect();
        let in_b: Vec<usize> = diff
            .lines
            .iter()
            .filter_map(|l| match *l {
                Line::Both { b, .. } | Line::OnlyB(b) => Some(b),
                Line::OnlyA(_) => None,
            })
            .collect();
        assert_eq!(in_a, (0..a.len()).collect::<Vec<_>>());
        assert_eq!(in_b, (0..b.len()).collect::<Vec<_>>());
        for line in &diff.lines {
            if let Line::Both { a: i, b: j } = *line {
                assert_eq!(a[i], b[j]);
            }
        }
    }

    /// Lines put in at the start, taken out at the end, and copies with
    /// no lines at all: each edge of the copies.
    #[test]
    fn lines_at_the_edges_and_empty_copies() {
        let a = ["two", "three", "four"];
        let b = ["one", "two", "three"];
        let diff = line_diff(&a, &b);
        assert_eq!(
            unified(&a, &b, &diff.lines),
            ["+one", " two", " three", "-four"]
        );
        let none: [&str; 0] = [];
        assert_eq!(
            line_diff(&none, &b).lines,
            vec![Line::OnlyB(0), Line::OnlyB(1), Line::OnlyB(2)]
        );
        assert_eq!(
            line_diff(&a, &none).lines,
            vec![Line::OnlyA(0), Line::OnlyA(1), Line::OnlyA(2)]
        );
        assert_eq!(line_diff(&none, &none).lines, vec![]);
    }

    /// Two copies with no line in common, longer than MAX_EDITS lines
    /// between them, under a shared first and last line: matched roughly,
    /// the shared lines kept and every other line only in one copy, the
    /// lines only in a first.
    #[test]
    fn copies_too_far_apart_are_matched_roughly() {
        let a: Vec<String> = std::iter::once("start".to_string())
            .chain((0..MAX_EDITS).map(|i| format!("a{i}")))
            .chain(std::iter::once("end".to_string()))
            .collect();
        let b: Vec<String> = std::iter::once("start".to_string())
            .chain((0..MAX_EDITS).map(|i| format!("b{i}")))
            .chain(std::iter::once("end".to_string()))
            .collect();
        let diff = line_diff(&a, &b);
        assert_eq!(diff.alignment, Alignment::Rough);
        let last = MAX_EDITS + 1;
        assert_eq!(diff.lines.first(), Some(&Line::Both { a: 0, b: 0 }));
        assert_eq!(diff.lines[1], Line::OnlyA(1));
        assert_eq!(diff.lines[MAX_EDITS + 1], Line::OnlyB(1));
        assert_eq!(diff.lines.last(), Some(&Line::Both { a: last, b: last }));
        assert_eq!(diff.lines.len(), 2 + 2 * MAX_EDITS);
    }

    /// Where the copies differ, lines only in a pair up with lines only in
    /// b in order, and the one left over gets a row to itself.
    #[test]
    fn side_by_side_pairs_the_lines_each_copy_has_alone() {
        let lines = [
            Line::Both { a: 0, b: 0 },
            Line::OnlyA(1),
            Line::OnlyA(2),
            Line::OnlyB(1),
            Line::Both { a: 3, b: 2 },
            Line::OnlyB(3),
        ];
        assert_eq!(
            side_by_side(&lines),
            vec![
                Row::Same { a: 0, b: 0 },
                Row::Changed { a: 1, b: 1 },
                Row::OnlyA(2),
                Row::Same { a: 3, b: 2 },
                Row::OnlyB(3),
            ]
        );
    }

    /// A pkg-config file whose store hash differs in two places, with one
    /// copy two bytes longer: the stretches that differ, the count, the
    /// first offset, and the bytes past the shorter copy's end.
    #[test]
    fn bytes_are_compared_offset_by_offset() {
        let a = b"prefix=/nix/store/1a2b-x\nlibdir=/nix/store/1a2b-x/lib\n";
        let b = b"prefix=/nix/store/9z2b-x\nlibdir=/nix/store/9z2b-x/lib\n!!";
        let diff = byte_diff(a, b);
        assert_eq!(diff.ranges, vec![18..20, 43..45, 54..56]);
        assert_eq!(diff.differing, 6);
        assert_eq!(diff.compared, 56);
        assert_eq!(diff.first(), Some(18));
        assert!(diff.differs_at(19) && diff.differs_at(55));
        assert!(!diff.differs_at(20) && !diff.differs_at(0));

        let same = byte_diff(b"same", b"same");
        assert_eq!((same.ranges, same.differing, same.compared), (vec![], 0, 4));
    }

    /// Equal bytes, two text copies, a binary copy, and text whose lines
    /// match but whose last line ends differently, which only the bytes
    /// show.
    #[test]
    fn copies_are_compared_by_their_kind() {
        assert_eq!(compare(b"x\n", b"x\n"), FileDiff::Same);
        let FileDiff::Text(diff) = compare(b"a\nb\n", b"a\nc\n") else {
            panic!("two text copies are compared by line");
        };
        assert_eq!(
            diff.lines,
            vec![Line::Both { a: 0, b: 0 }, Line::OnlyA(1), Line::OnlyB(1)]
        );
        assert!(matches!(
            compare(b"\x7fELF\x00", b"\x7fELF\x01"),
            FileDiff::Binary(_)
        ));
        let FileDiff::Binary(ends) = compare(b"a\nb\n", b"a\nb") else {
            panic!("lines that match, ending differently, are compared by byte");
        };
        assert_eq!(ends.ranges, vec![3..4]);
    }

    /// The summaries a person reads: the answer first, then one fact a
    /// line, in the runs' own names.
    #[test]
    fn summaries_lead_with_the_answer() {
        let sides = Sides {
            a: "this run",
            b: "the other run",
        };

        // Binary bytes that differ, with the sizes when they are not the
        // same.
        let a = vec![0u8; 407];
        let mut b = a.clone();
        for byte in &mut b[216..232] {
            *byte = 0xff;
        }
        let diff = compare(&a, &b);
        assert_eq!(
            summary("neatvnc.pc", &a, &b, &diff, sides),
            ["neatvnc.pc differs: 16 of 407 bytes, first at byte 216."]
        );
        let longer = [&a[..], b"\x00\x01"].concat();
        let diff = compare(&a, &longer);
        assert_eq!(
            summary("libneatvnc.so", &a, &longer, &diff, sides),
            [
                "libneatvnc.so differs: 2 of 409 bytes, first at byte 407.",
                "Size in this run: 407 bytes.",
                "Size in the other run: 409 bytes.",
            ]
        );

        // Lines only in each copy, and a single line in the singular.
        let (a, b) = (b"one\ntwo\nthree\n", b"one\n2\n3\nthree\n");
        let diff = compare(a, b);
        assert_eq!(
            summary("test.log", a, b, &diff, sides),
            [
                "test.log differs, first at line 2.",
                "Only in this run: 1 line.",
                "Only in the other run: 2 lines.",
            ]
        );

        // The same bytes, with a size large enough to group its digits.
        let big = vec![b'x'; 12_345];
        assert_eq!(
            summary("pool.o", &big, &big, &FileDiff::Same, sides),
            ["pool.o is the same in both runs: 12,345 bytes."]
        );
    }

    /// Lines ending in a newline, a CRLF, an empty line and a last line
    /// with no newline: each line starts where lines_of says it does, and
    /// no line starts past the end.
    #[test]
    fn lines_start_where_lines_of_splits_them() {
        let bytes = b"CC = cc\r\n\nall: pool\nclean:";
        let starts = line_starts(bytes);
        assert_eq!(starts, [0, 9, 10, 20]);
        assert_eq!(starts.len(), lines_of(bytes).len());
        for (start, line) in starts.iter().zip(lines_of(bytes)) {
            assert!(bytes[*start..].starts_with(line.as_bytes()));
        }
        assert_eq!(line_starts(b"a\nb\n"), [0, 2]);
        assert_eq!(line_starts(b""), Vec::<usize>::new());
    }

    /// A column of a UTF-8 line is the file's byte at that offset into the
    /// line, a column past the line's end stops at its end, and a line
    /// that is not UTF-8 is pointed to by its first byte, since lines_of
    /// replaced its bad bytes with characters of another length.
    #[test]
    fn a_column_is_a_byte_of_the_file() {
        let bytes = [
            b"x = 1\n".as_slice(),
            "naïve = 2\n".as_bytes(),
            b"bad \xff = 3\n",
        ]
        .concat();
        let starts = line_starts(&bytes);
        assert_eq!(starts, [0, 6, 17]);
        assert_eq!(byte_in_line(&bytes, &starts, 0, 4), 4);
        assert_eq!(byte_in_line(&bytes, &starts, 1, 7), 6 + 7);
        assert_eq!(bytes[byte_in_line(&bytes, &starts, 1, 7)], b'=');
        assert_eq!(byte_in_line(&bytes, &starts, 0, 99), 5);
        assert_eq!(byte_in_line(&bytes, &starts, 2, 6), 17);
    }

    /// A rough match says plainly that the lines were not paired up.
    #[test]
    fn a_rough_match_says_so() {
        let a: String = (0..=MAX_EDITS).map(|i| format!("a{i}\n")).collect();
        let b: String = (0..=MAX_EDITS).map(|i| format!("b{i}\n")).collect();
        let diff = compare(a.as_bytes(), b.as_bytes());
        let said = summary(
            "out.txt",
            a.as_bytes(),
            b.as_bytes(),
            &diff,
            Sides {
                a: "this run",
                b: "the other run",
            },
        );
        assert_eq!(said.last().map(String::as_str), Some(ROUGH_NOTE));
    }
}
