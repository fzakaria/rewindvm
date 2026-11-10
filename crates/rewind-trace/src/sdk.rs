//! The marks the Rewind SDK writes from inside a job.
//!
//! A program asserts what must hold, and marks where its tests start and
//! end, by writing a line to /dev/rewind, which the trace keeps as a mark
//! with the process, thread and step that wrote it. The SDK's libraries,
//! one file per language under sdk/ at the repository's root, write these
//! lines; this module reads them back. A line is
//!
//! ```text
//! rewind-sdk 1 always pass <id>
//! rewind-sdk 1 always fail <id> <message>
//! rewind-sdk 1 sometimes hit <id>
//! rewind-sdk 1 sometimes miss <id>
//! rewind-sdk 1 unreachable <id> <message>
//! rewind-sdk 1 test-begin <name>
//! rewind-sdk 1 test-end pass|fail|skip <name>
//! ```
//!
//! where an id has no spaces and a message or a test's name is the rest
//! of the line.

use std::fmt;

use crate::Event;

/// The word every SDK mark starts with.
pub const PREFIX: &str = "rewind-sdk";

/// The version of the lines this module reads. A library that writes
/// another is not one this build reads.
pub const VERSION: &str = "1";

const ALWAYS: &str = "always";
const SOMETIMES: &str = "sometimes";
const UNREACHABLE: &str = "unreachable";
const TEST_BEGIN: &str = "test-begin";
const TEST_END: &str = "test-end";
const PASS: &str = "pass";
const FAIL: &str = "fail";
const SKIP: &str = "skip";
const HIT: &str = "hit";
const MISS: &str = "miss";

/// One SDK mark.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SdkMark {
    /// A condition that must hold every time the program checks it: held
    /// or not, with what the program said when it did not.
    Always {
        id: String,
        held: bool,
        message: String,
    },
    /// A condition some run should make true: whether this check of it
    /// was true.
    Sometimes { id: String, hit: bool },
    /// A place no run should reach, reached.
    Unreachable { id: String, message: String },
    /// A test started.
    TestBegin { name: String },
    /// A test ended.
    TestEnd { name: String, result: TestResult },
}

/// How a test ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestResult {
    Pass,
    Fail,
    Skip,
}

impl fmt::Display for TestResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let word = match self {
            TestResult::Pass => PASS,
            TestResult::Fail => FAIL,
            TestResult::Skip => SKIP,
        };
        write!(f, "{word}")
    }
}

impl SdkMark {
    /// The mark a line of text is, or None when it is not an SDK mark of
    /// this version or is not well formed.
    pub fn parse(text: &str) -> Option<SdkMark> {
        let rest = text.strip_prefix(PREFIX)?.strip_prefix(' ')?;
        let rest = rest.strip_prefix(VERSION)?.strip_prefix(' ')?;
        let (verb, rest) = rest.split_once(' ').unwrap_or((rest, ""));
        let word = |s: &str| -> Option<(String, String)> {
            let (first, tail) = s.split_once(' ').unwrap_or((s, ""));
            (!first.is_empty()).then(|| (first.to_string(), tail.to_string()))
        };
        match verb {
            ALWAYS => {
                let (status, rest) = word(rest)?;
                let (id, message) = word(&rest)?;
                let held = match status.as_str() {
                    PASS => true,
                    FAIL => false,
                    _ => return None,
                };
                Some(SdkMark::Always { id, held, message })
            }
            SOMETIMES => {
                let (status, rest) = word(rest)?;
                let (id, _) = word(&rest)?;
                let hit = match status.as_str() {
                    HIT => true,
                    MISS => false,
                    _ => return None,
                };
                Some(SdkMark::Sometimes { id, hit })
            }
            UNREACHABLE => {
                let (id, message) = word(rest)?;
                Some(SdkMark::Unreachable { id, message })
            }
            TEST_BEGIN if !rest.is_empty() => Some(SdkMark::TestBegin {
                name: rest.to_string(),
            }),
            TEST_END => {
                let (status, name) = word(rest)?;
                let result = match status.as_str() {
                    PASS => TestResult::Pass,
                    FAIL => TestResult::Fail,
                    SKIP => TestResult::Skip,
                    _ => return None,
                };
                (!name.is_empty()).then_some(SdkMark::TestEnd { name, result })
            }
            _ => None,
        }
    }
}

impl SdkMark {
    /// The mark in words, as the command and the desktop app say it:
    /// `always "even" failed: byte 221 is odd`, `test coin flip passed`.
    pub fn words(&self) -> String {
        let said = |what: String, message: &str| {
            if message.is_empty() {
                what
            } else {
                format!("{what}: {message}")
            }
        };
        match self {
            SdkMark::Always {
                id,
                held: false,
                message,
            } => said(format!("always \"{id}\" failed"), message),
            SdkMark::Always { id, .. } => format!("always \"{id}\" held"),
            SdkMark::Sometimes { id, hit: true } => format!("sometimes \"{id}\" was true"),
            SdkMark::Sometimes { id, hit: false } => format!("sometimes \"{id}\" was false"),
            SdkMark::Unreachable { id, message } => {
                said(format!("unreachable \"{id}\" reached"), message)
            }
            SdkMark::TestBegin { name } => format!("test {name} began"),
            SdkMark::TestEnd { name, result } => match result {
                TestResult::Pass => format!("test {name} passed"),
                TestResult::Fail => format!("test {name} failed"),
                TestResult::Skip => format!("test {name} was skipped"),
            },
        }
    }

    /// Whether the mark says something broke: an `always` that failed, an
    /// `unreachable` reached, or a test that failed.
    pub fn broke(&self) -> bool {
        match self {
            SdkMark::Always { held, .. } => !held,
            SdkMark::Unreachable { .. } => true,
            SdkMark::TestEnd { result, .. } => *result == TestResult::Fail,
            SdkMark::Sometimes { .. } | SdkMark::TestBegin { .. } => false,
        }
    }

    /// Whether the mark is an assertion the job broke: an `always` that
    /// failed or an `unreachable` reached. A test that failed is reported
    /// by the job's own exit, and is not one.
    pub fn is_failed_assertion(&self) -> bool {
        matches!(
            self,
            SdkMark::Always { held: false, .. } | SdkMark::Unreachable { .. }
        )
    }
}

impl Event {
    /// The SDK mark this event is, if a process other than init wrote one.
    pub fn sdk_mark(&self) -> Option<SdkMark> {
        match &self.kind {
            crate::EventKind::Mark { text } if self.pid != rewind_init::INIT_PID => {
                SdkMark::parse(text)
            }
            _ => None,
        }
    }
}

impl crate::Trace {
    /// Every SDK mark in the run, with the event that wrote it, in step
    /// order.
    pub fn sdk_marks(&self) -> impl Iterator<Item = (&Event, SdkMark)> {
        self.events.iter().filter_map(|e| Some((e, e.sdk_mark()?)))
    }

    /// Every assertion the job made with the Rewind SDK and broke, in
    /// step order, each in its words.
    pub fn failed_assertions(&self) -> Vec<crate::verdict::FailedAssertion> {
        self.sdk_marks()
            .filter(|(_, mark)| mark.is_failed_assertion())
            .map(|(e, mark)| crate::verdict::FailedAssertion {
                step: e.step,
                pid: e.pid,
                tid: e.tid,
                text: mark.words(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    // Lines as the SDK's libraries write them parse to their marks, and
    // anything else is not an SDK mark.
    use super::*;

    #[test]
    fn sdk_lines_parse() {
        assert_eq!(
            SdkMark::parse("rewind-sdk 1 always fail never-oversell sold=11 of 10"),
            Some(SdkMark::Always {
                id: "never-oversell".into(),
                held: false,
                message: "sold=11 of 10".into(),
            })
        );
        assert_eq!(
            SdkMark::parse("rewind-sdk 1 always pass never-oversell"),
            Some(SdkMark::Always {
                id: "never-oversell".into(),
                held: true,
                message: String::new(),
            })
        );
        assert_eq!(
            SdkMark::parse("rewind-sdk 1 sometimes miss sold-out"),
            Some(SdkMark::Sometimes {
                id: "sold-out".into(),
                hit: false,
            })
        );
        assert_eq!(
            SdkMark::parse("rewind-sdk 1 unreachable retry-after-close pool closed"),
            Some(SdkMark::Unreachable {
                id: "retry-after-close".into(),
                message: "pool closed".into(),
            })
        );
        assert_eq!(
            SdkMark::parse("rewind-sdk 1 test-begin InventoryTest neverOversells()"),
            Some(SdkMark::TestBegin {
                name: "InventoryTest neverOversells()".into(),
            })
        );
        assert_eq!(
            SdkMark::parse("rewind-sdk 1 test-end fail tests/test_cache.py::test_stale"),
            Some(SdkMark::TestEnd {
                name: "tests/test_cache.py::test_stale".into(),
                result: TestResult::Fail,
            })
        );
    }

    // Each mark in the words the command and the app say it in, and which
    // of them say something broke or are an assertion the job broke.
    #[test]
    fn marks_have_words_and_say_whether_something_broke() {
        let mark = |line: &str| SdkMark::parse(line).unwrap();
        let failed = mark("rewind-sdk 1 always fail even byte 221 is odd");
        assert_eq!(failed.words(), "always \"even\" failed: byte 221 is odd");
        assert!(failed.broke() && failed.is_failed_assertion());
        let held = mark("rewind-sdk 1 always pass even");
        assert_eq!(held.words(), "always \"even\" held");
        assert!(!held.broke() && !held.is_failed_assertion());
        let reached = mark("rewind-sdk 1 unreachable closed");
        assert_eq!(reached.words(), "unreachable \"closed\" reached");
        assert!(reached.broke() && reached.is_failed_assertion());
        let test_failed = mark("rewind-sdk 1 test-end fail coin flip");
        assert_eq!(test_failed.words(), "test coin flip failed");
        assert!(test_failed.broke() && !test_failed.is_failed_assertion());
        assert_eq!(
            mark("rewind-sdk 1 sometimes miss sold-out").words(),
            "sometimes \"sold-out\" was false"
        );
        assert_eq!(
            mark("rewind-sdk 1 test-begin coin flip").words(),
            "test coin flip began"
        );
        assert_eq!(
            mark("rewind-sdk 1 test-end skip coin flip").words(),
            "test coin flip was skipped"
        );
    }

    // Another version, an unknown verb or status, a missing id, and a
    // mark that only starts like one are not SDK marks.
    #[test]
    fn other_lines_are_not_sdk_marks() {
        for text in [
            "rewind-sdk 2 always pass x",
            "rewind-sdk 1 eventually pass x",
            "rewind-sdk 1 always maybe x",
            "rewind-sdk 1 always pass",
            "rewind-sdk 1 test-begin",
            "rewind-sdk 1 test-end pass",
            "rewind-sdkx 1 always pass x",
            "phase: compile",
        ] {
            assert_eq!(SdkMark::parse(text), None, "{text}");
        }
    }
}
