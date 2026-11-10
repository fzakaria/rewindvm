//! The command that opens, in the desktop app, what the browser viewer
//! shows: for the notices that say a pane or an action is the desktop
//! app's, so the reader can go on there at the same step.

use crate::ui::RightColumn;

/// The desktop app's command, as `rewind-app --help` names it.
const APP: &str = "rewind-app";
const COMPARE_FLAG: &str = "--compare";
const STEP_FLAG: &str = "--step";
const SOURCE_FLAG: &str = "--source";

/// What the notice says before the command.
const GO_ON_WITH: &str = "To go on from here in the desktop app:";

/// The body of a notice that says `doing` is the desktop app's: the
/// browser viewer's words on that, then `command`, the one that opens
/// the recording on screen there at this step, on a line of its own for
/// the Copy command button and the eye.
pub fn go_on_words(said: &str, command: &str) -> String {
    format!("{said}\n{GO_ON_WITH}\n{command}")
}

/// The command that opens `run` in the desktop app at `step`, compared
/// with `compare` when there is one, with the right column on `right`.
/// Each run is a link, or the name of a file, as the app takes either.
pub fn desktop_command(run: &str, compare: Option<&str>, step: u64, right: RightColumn) -> String {
    let mut words = vec![APP.to_string(), shell_word(run)];
    if let Some(compare) = compare {
        words.push(COMPARE_FLAG.to_string());
        words.push(shell_word(compare));
    }
    words.push(STEP_FLAG.to_string());
    words.push(step.to_string());
    if right == RightColumn::Source {
        words.push(SOURCE_FLAG.to_string());
    }
    words.join(" ")
}

/// The bytes a shell takes as they are in a word: letters, digits and the
/// punctuation of a path or a link that has no query.
fn is_plain(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"-_./:%@=+,~".contains(&byte)
}

/// `word` as a shell reads it back whole: as it is when every byte is
/// plain, else in single quotes, with a quote inside written as `'\''`.
fn shell_word(word: &str) -> String {
    if !word.is_empty() && word.bytes().all(is_plain) {
        return word.to_string();
    }
    format!("'{}'", word.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    // The command for a run at a step, as a shell on another machine
    // would read it.
    use super::*;

    #[test]
    fn the_command_names_the_run_the_comparison_the_step_and_the_source() {
        // A run alone opens at the step; a compared run adds --compare;
        // the source panel adds --source at the end.
        assert_eq!(
            desktop_command(
                "https://rewindvm.dev/viewer/examples/bank.rwd",
                None,
                40,
                RightColumn::AtStep
            ),
            "rewind-app https://rewindvm.dev/viewer/examples/bank.rwd --step 40"
        );
        assert_eq!(
            desktop_command(
                "https://rewindvm.dev/viewer/examples/mylib-fail.rwd",
                Some("https://rewindvm.dev/viewer/examples/mylib-pass.rwd"),
                5775,
                RightColumn::Source
            ),
            "rewind-app https://rewindvm.dev/viewer/examples/mylib-fail.rwd --compare https://rewindvm.dev/viewer/examples/mylib-pass.rwd --step 5775 --source"
        );
    }

    #[test]
    fn the_notice_ends_with_the_command_on_its_own_line() {
        // The viewer's words come first, then what the command is for,
        // then the command alone on the last line.
        let body = go_on_words(
            "Attaching gdb runs the recording again.",
            "rewind-app https://rewindvm.dev/viewer/examples/bank.rwd --step 40",
        );
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "Attaching gdb runs the recording again.");
        assert_eq!(
            lines[2],
            "rewind-app https://rewindvm.dev/viewer/examples/bank.rwd --step 40"
        );
    }

    #[test]
    fn a_link_a_shell_would_split_is_quoted() {
        // A signed link has a query, which a shell would read as a
        // background command and a variable; it goes in single quotes. A
        // name with a quote in it is quoted around the quote. An empty
        // name is a pair of quotes, so the step does not take its place.
        assert_eq!(
            shell_word("https://ci.example/run.rwd?sig=a&b"),
            "'https://ci.example/run.rwd?sig=a&b'"
        );
        assert_eq!(shell_word("it's.rwd"), "'it'\\''s.rwd'");
        assert_eq!(shell_word("my run.rwd"), "'my run.rwd'");
        assert_eq!(shell_word(""), "''");
        assert_eq!(shell_word("run-1_2.rwd"), "run-1_2.rwd");
    }
}
