//! How the engine says why a command failed, to a program reading its
//! standard error.
//!
//! A person reads a failure as words after `rewind:`. A program, such as
//! the desktop app, needs to know which failure it was without matching
//! those words, which change as they are improved. With `--errors json`,
//! or `REWIND_ERRORS=json`, a command that fails prints one line of JSON
//! on standard error instead, after any progress lines:
//!
//! ```text
//! {"error":{"kind":"went_another_way","run":"6380bb57fcb451da","step":311,"message":"replaying run …"}}
//! ```
//!
//! `kind` names the failure, with what a program needs to act on it, and
//! `message` holds the words a person would have read. Kinds a program
//! does not know read as [`Kind::Other`], so an engine can name new ones.

use serde::{Deserialize, Serialize};

use crate::manifest::RunId;

/// The option, and its environment variable, that choose how the engine
/// says why a command failed.
pub const ERRORS_OPTION: &str = "errors";
pub const ERRORS_ENV: &str = "REWIND_ERRORS";

/// The value of `--errors` that asks for a line of JSON.
pub const ERRORS_JSON: &str = "json";

/// Which failure it was, for a program to act on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Kind {
    /// Replaying `run` made a record other than its recording's at `step`,
    /// so this build of rewind cannot bring the run back to a step: any
    /// later request to look inside it, or fork it, fails the same way.
    WentAnotherWay { run: RunId, step: u64 },
    /// Any other failure, or one a newer engine names that this build does
    /// not know: its message says what it was.
    #[serde(other)]
    Other,
}

/// A failed command: which failure, and the words for a person.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    #[serde(flatten)]
    pub kind: Kind,
    pub message: String,
}

/// The line a failure is printed as, its fields under `error`.
#[derive(Serialize, Deserialize)]
struct Line {
    error: Failure,
}

impl Failure {
    /// The failure as its line of JSON, without the newline.
    pub fn line(&self) -> String {
        let line = Line {
            error: self.clone(),
        };
        serde_json::to_string(&line).expect("a failure is plain data")
    }

    /// The failure `line` reports, if it is a failure's line.
    pub fn parse(line: &str) -> Option<Failure> {
        serde_json::from_str::<Line>(line.trim())
            .ok()
            .map(|l| l.error)
    }

    /// The last failure among the lines of `text`, as the engine prints
    /// one at the end of its standard error.
    pub fn last_in(text: &str) -> Option<Failure> {
        text.lines().rev().find_map(Failure::parse)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run() -> RunId {
        RunId::parse("6380bb57fcb451da").unwrap()
    }

    #[test]
    fn a_failure_reads_back_from_its_line() {
        let failure = Failure {
            kind: Kind::WentAnotherWay {
                run: run(),
                step: 311,
            },
            message: "replaying run 6380bb57fcb451da went another way at step 311".into(),
        };
        let line = failure.line();
        assert_eq!(
            line,
            r#"{"error":{"kind":"went_another_way","run":"6380bb57fcb451da","step":311,"message":"replaying run 6380bb57fcb451da went another way at step 311"}}"#
        );
        assert_eq!(Failure::parse(&line), Some(failure));
    }

    #[test]
    fn a_kind_this_build_does_not_know_is_other() {
        // A newer engine's kind, with fields of its own: the message still
        // reads.
        let line = r#"{"error":{"kind":"disk_full","bytes":4096,"message":"no space left"}}"#;
        assert_eq!(
            Failure::parse(line),
            Some(Failure {
                kind: Kind::Other,
                message: "no space left".into(),
            })
        );
    }

    #[test]
    fn only_a_failure_s_line_is_one() {
        // Progress lines and words come before it on standard error; the
        // last failure's line is the one.
        let text = format!(
            "{}\nrewind: something\n{}\n",
            r#"{"run":"6380bb57fcb451da","from":0,"at":5,"to":9,"elapsed_ms":3}"#,
            r#"{"error":{"kind":"other","message":"no process 7"}}"#
        );
        assert_eq!(Failure::parse("rewind: no process 7"), None);
        assert_eq!(
            Failure::last_in(&text).map(|f| f.message),
            Some("no process 7".into())
        );
    }
}
