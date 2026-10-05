//! A line of output as a terminal shows it. A program that writes to a
//! terminal, as a Nix builder does, may color its text with escape
//! sequences, go back to the start of the line with a carriage return and
//! write over it, as progress counters do, or erase the rest of the line.
//! The log shows what the terminal would: the text left on the line, with
//! the escape sequences taken out.

/// The escape character that starts a terminal's control sequences.
const ESC: char = '\u{1b}';
/// The bell, which can end an operating system command.
const BEL: char = '\u{7}';
const BACKSPACE: char = '\u{8}';

/// The control sequence introducer's second character, `ESC [`, and the
/// operating system command's, `ESC ]`.
const CSI: char = '[';
const OSC: char = ']';

/// The final character of the sequence that erases the line from the
/// cursor on, `ESC [ K`, and of `ESC \`, which ends an operating system
/// command.
const ERASE_LINE: char = 'K';
const STRING_END: char = '\\';

/// What a terminal shows of `text`, one line of a program's output.
pub fn shown(text: &str) -> String {
    let mut line: Vec<char> = Vec::with_capacity(text.len());
    let mut cursor: usize = 0;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => cursor = 0,
            BACKSPACE => cursor = cursor.saturating_sub(1),
            ESC => match chars.next() {
                // A control sequence: parameters, then one final character
                // from @ to ~. Erasing the line cuts it at the cursor;
                // every other one, colors among them, shows nothing.
                Some(CSI) => {
                    let mut params = String::new();
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            if c == ERASE_LINE {
                                erase(&mut line, cursor, &params);
                            }
                            break;
                        }
                        params.push(c);
                    }
                }
                // An operating system command, such as a window title,
                // ends at a bell or at ESC \.
                Some(OSC) => {
                    while let Some(c) = chars.next() {
                        if c == BEL {
                            break;
                        }
                        if c == ESC && chars.peek() == Some(&STRING_END) {
                            chars.next();
                            break;
                        }
                    }
                }
                // Any other escape is two characters long.
                _ => {}
            },
            '\t' => put(&mut line, &mut cursor, c),
            c if c.is_control() => {}
            c => put(&mut line, &mut cursor, c),
        }
    }
    line.into_iter().collect()
}

/// Writes `c` at the cursor, over what is there, and moves past it.
fn put(line: &mut Vec<char>, cursor: &mut usize, c: char) {
    match line.get_mut(*cursor) {
        Some(at) => *at = c,
        None => line.push(c),
    }
    *cursor += 1;
}

/// `ESC [ K` with its parameter: 0 or none erases from the cursor to the
/// end, 1 from the start to the cursor, 2 the whole line.
fn erase(line: &mut Vec<char>, cursor: usize, params: &str) {
    match params {
        "1" => {
            for c in line.iter_mut().take(cursor + 1) {
                *c = ' ';
            }
        }
        "2" => line.clear(),
        _ => line.truncate(cursor),
    }
}

#[cfg(test)]
mod tests {
    // Lines with the sequences programs write to a terminal, and what a
    // terminal leaves on screen of each.
    use super::*;

    #[test]
    fn colors_are_taken_out() {
        // meson's colored YES and gcc's bold error, as SGR sequences.
        assert_eq!(
            shown("Run-time dependency GTest found: \u{1b}[1;32mYES\u{1b}[0m 1.17.0"),
            "Run-time dependency GTest found: YES 1.17.0"
        );
        assert_eq!(
            shown("\u{1b}[01m\u{1b}[Kpool.c:77:\u{1b}[m\u{1b}[K error"),
            "pool.c:77: error"
        );
    }

    #[test]
    fn a_carriage_return_writes_over_the_line() {
        // A counter rewritten in place leaves its last value; a shorter
        // write over a longer one leaves the longer one's tail, unless the
        // line is erased after it.
        assert_eq!(shown("[1/3] a\r[2/3] b\r[3/3] c"), "[3/3] c");
        assert_eq!(shown("downloading 100%\rdone"), "doneloading 100%");
        assert_eq!(shown("downloading 100%\rdone\u{1b}[K"), "done");
        assert_eq!(shown("abc\u{8}\u{8}X"), "aXc");
    }

    #[test]
    fn titles_and_other_controls_are_dropped() {
        // An operating system command that sets the window title, ended by
        // a bell or by ESC \, and stray control characters leave nothing;
        // a tab stays.
        assert_eq!(shown("\u{1b}]0;make\u{7}building"), "building");
        assert_eq!(shown("\u{1b}]2;title\u{1b}\\ok"), "ok");
        assert_eq!(shown("a\u{1}b\tc"), "ab\tc");
        assert_eq!(shown("plain text"), "plain text");
    }
}
