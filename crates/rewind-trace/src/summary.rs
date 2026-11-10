//! A run in plain text: whether it passed and why not, how it ended, the
//! step it failed on, the last lines the job printed, and, beside another
//! run, where the two part. `rewind summary` prints it, every export
//! carries it as `summary.txt`, and the browser viewer shows it on a
//! screen too narrow for the app, so a person or a program can read a
//! recording without the scrubber. The text has no color codes, and the
//! job's own escape sequences are taken out of its lines.

use std::fmt::Write;

use crate::Trace;
use crate::compare::{Comparison, Side};
use crate::ending;
use crate::failing::{self, Failure, FailureKind, Stopped};
use crate::manifest::{Manifest, RunOutcome};
use crate::signal_name;
use crate::verdict::{FailedAssertion, Reason, Verdict};

/// The name of the summary inside an export.
pub const SUMMARY: &str = "summary.txt";

/// How many of the job's last lines of output the summary shows.
pub const OUTPUT_LINES: usize = 20;

/// nixpkgs' setup.sh writes structured messages for nix itself on lines
/// starting with this; nix-build never shows them, and neither does the
/// desktop app's log nor the summary.
const NIX_LOG_PREFIX: &str = "@nix ";

/// How the lines under a heading are set in.
const INDENT: &str = "  ";

/// What a summary is made from: a run's manifest's fields and its trace.
#[derive(Clone, Copy, Debug)]
pub struct Recording<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub verdict: &'a Verdict,
    /// How the machine stopped and the job's wait status; None for a run
    /// that has not ended.
    pub outcome: Option<&'a RunOutcome>,
    pub trace: &'a Trace,
}

impl<'a> Recording<'a> {
    /// The recording a run's manifest and trace make.
    pub fn of(manifest: &'a Manifest, trace: &'a Trace) -> Recording<'a> {
        Recording {
            id: manifest.id.as_str(),
            name: &manifest.name,
            verdict: &manifest.verdict,
            outcome: manifest.outcome.as_ref(),
            trace,
        }
    }

    /// The step the run ended at: the last step its manifest or its trace
    /// knows.
    fn end(&self) -> u64 {
        let last_switch = self.trace.switches.last().map_or(0, |s| s.step);
        let ended = self.outcome.map_or(0, |o| o.step);
        self.trace.last_step().max(last_switch).max(ended)
    }

    /// The step the run failed on, as the desktop app's failure marker
    /// finds it.
    fn failure(&self) -> Option<Failure> {
        let stopped = Stopped::of(self.outcome.map(|o| &o.stop));
        failing::find(self.trace, self.trace.job_exit(), self.end(), stopped, None)
    }

    /// The process that had `pid` at `step`, in words: its pid, and the
    /// name of its program when the trace names one.
    fn process(&self, pid: u32, step: u64) -> String {
        match self.trace.process_name_at(pid, step) {
            Some(name) => format!("process {pid} ({name})"),
            None => format!("process {pid}"),
        }
    }
}

/// The summary of `run`, and, given `other`, where the two runs part and
/// what each did there.
pub fn summary(run: Recording, other: Option<Recording>) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "run {}: {}", run.id, run.name);
    verdict_lines(&mut out, &run, "");
    failure_line(&mut out, &run);
    output_lines(&mut out, &run);
    if let Some(other) = other {
        comparison_lines(&mut out, &run, &other);
    }
    out
}

/// The verdict and every reason the run failed for, then how it ended,
/// each line after `lead`.
fn verdict_lines(out: &mut String, run: &Recording, lead: &str) {
    let state = match run.verdict {
        Verdict::Unfinished => "unfinished, the run has not ended",
        Verdict::Passed => "passed",
        Verdict::Failed { .. } => "failed",
    };
    let _ = writeln!(out, "{lead}verdict: {state}");
    for reason in run.verdict.reasons() {
        for line in reason_lines(run, reason) {
            let _ = writeln!(out, "{lead}{INDENT}{line}");
        }
    }
    let Some(outcome) = run.outcome else {
        return;
    };
    let _ = writeln!(
        out,
        "{lead}ended: {} at step {} ({})",
        ending::label(outcome, run.verdict),
        outcome.step,
        outcome.stop
    );
}

/// A reason the run failed, in a line or, for outputs and assertions, a
/// line each.
fn reason_lines(run: &Recording, reason: &Reason) -> Vec<String> {
    match reason {
        Reason::TimedOut => vec!["timed out: the machine reached its time limit".into()],
        Reason::Exited { code } => vec![format!("exited with status {code}")],
        Reason::Killed { signal, core } => {
            let dumped = if *core { ", core dumped" } else { "" };
            vec![format!("killed by {}{dumped}", signal_name(*signal))]
        }
        Reason::Crashed => {
            vec!["the machine stopped before init reported the job's end".into()]
        }
        Reason::MissingOutputs { paths } => paths
            .iter()
            .map(|path| format!("did not create {path}"))
            .collect(),
        Reason::FailedAssertions { assertions } => assertions
            .iter()
            .map(|assertion| assertion_line(run, assertion))
            .collect(),
    }
}

/// A broken assertion, where it broke and who broke it.
fn assertion_line(run: &Recording, assertion: &FailedAssertion) -> String {
    let FailedAssertion {
        step,
        pid,
        tid,
        text,
    } = assertion;
    format!(
        "{text}, at step {step}, in {}, thread {tid}",
        run.process(*pid, *step)
    )
}

/// The step the run failed on, the process and thread there, and what
/// happened; nothing when the trace shows no failure.
fn failure_line(out: &mut String, run: &Recording) {
    let Some(failure) = run.failure() else {
        return;
    };
    let Failure {
        step,
        pid,
        tid,
        index,
        kind,
    } = failure;
    let event = &run.trace.events[index];
    let what = match kind {
        FailureKind::Signal { signo, addr } => {
            format!("{} delivered, fault address {addr:#x}", signal_name(signo))
        }
        FailureKind::Exit { .. } => event.kind.words(),
        FailureKind::Assertion => event
            .sdk_mark()
            .map_or_else(|| event.kind.words(), |mark| mark.words()),
        FailureKind::Stopped => format!(
            "the machine stopped before the job exited, after {}",
            event.kind.words()
        ),
    };
    let _ = writeln!(
        out,
        "failing step: {step}, in {}, thread {tid}: {what}",
        run.process(pid, step)
    );
}

/// The job's last lines of output, as a terminal shows them.
fn output_lines(out: &mut String, run: &Recording) {
    let lines: Vec<String> = run
        .trace
        .lines_until(u64::MAX)
        .iter()
        .map(|line| as_shown(&line.text))
        .filter(|text| !text.starts_with(NIX_LOG_PREFIX))
        .collect();
    if lines.is_empty() {
        let _ = writeln!(out, "output: none");
        return;
    }
    let shown = &lines[lines.len().saturating_sub(OUTPUT_LINES)..];
    let counted = if lines.len() == 1 {
        "1 line".to_string()
    } else {
        format!("{} lines", lines.len())
    };
    if shown.len() == lines.len() {
        let _ = writeln!(out, "output ({counted}):");
    } else {
        let _ = writeln!(out, "output (the last {} of {counted}):", shown.len());
    }
    for line in shown {
        let _ = writeln!(out, "{INDENT}{line}");
    }
}

/// The other run's verdict, and where the two runs part, each run's
/// event there.
fn comparison_lines(out: &mut String, run: &Recording, other: &Recording) {
    let _ = writeln!(out, "compared with run {}: {}", other.id, other.name);
    verdict_lines(out, other, INDENT);
    let comparison = Comparison::of(run.trace, run.end(), other.trace, other.end());
    let Some(point) = &comparison.point else {
        let _ = writeln!(out, "the runs did the same things");
        return;
    };
    let within = match &comparison.program {
        Some(argv) => format!(", in the events of {}", argv.join(" ")),
        None => String::new(),
    };
    let _ = writeln!(
        out,
        "the runs part at step {} here and step {} there{within}",
        point.step, point.other_step
    );
    let _ = writeln!(out, "{INDENT}here:  {}", side_words(run, point.here));
    let _ = writeln!(out, "{INDENT}there: {}", side_words(other, point.there));
}

/// What one run did where the runs part: its event, or that its events
/// ran out.
fn side_words(run: &Recording, side: Option<Side>) -> String {
    let Some(event) = side.and_then(|s| run.trace.events.get(s.index)) else {
        return format!("no more events; the run ended at step {}", run.end());
    };
    format!(
        "{}, thread {}: {}",
        run.process(event.pid, event.step),
        event.tid,
        event.kind.words()
    )
}

/// The escape character that starts a terminal's control sequences, and
/// the bytes after it that start a control sequence and an operating
/// system command.
const ESCAPE: char = '\x1b';
const CONTROL_SEQUENCE: char = '[';
const OS_COMMAND: char = ']';
/// The bell, and the escape then backslash, that end an operating system
/// command.
const BELL: char = '\x07';
const STRING_END: char = '\\';
/// The final character of the control sequence that erases the line from
/// the cursor on.
const ERASE_LINE: char = 'K';
/// The range of characters that end a control sequence.
const FINAL_BYTES: std::ops::RangeInclusive<char> = '\x40'..='\x7e';
/// The control characters a line acts on: back to its start, and back one
/// character.
const CARRIAGE_RETURN: char = '\r';
const BACKSPACE: char = '\x08';
const TAB: char = '\t';

/// `text`, a line a program wrote, as a terminal shows it without color:
/// escape sequences taken out, a carriage return or a backspace writing
/// over what came before, and an erase of the line cutting it at the
/// cursor. Other control characters are dropped.
fn as_shown(text: &str) -> String {
    let mut line: Vec<char> = Vec::new();
    let mut cursor = 0;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ESCAPE => match chars.next() {
                // A control sequence runs to its final character.
                Some(CONTROL_SEQUENCE) => {
                    let end = chars.by_ref().find(|c| FINAL_BYTES.contains(c));
                    if end == Some(ERASE_LINE) {
                        line.truncate(cursor);
                    }
                }

                // An operating system command, such as a title or a link,
                // runs to a bell or to an escape and a backslash.
                Some(OS_COMMAND) => {
                    while let Some(c) = chars.next() {
                        if c == BELL {
                            break;
                        }
                        if c == ESCAPE && chars.peek() == Some(&STRING_END) {
                            chars.next();
                            break;
                        }
                    }
                }

                // Any other escape is two characters.
                _ => {}
            },
            CARRIAGE_RETURN => cursor = 0,
            BACKSPACE => cursor = cursor.saturating_sub(1),
            c if c.is_control() && c != TAB => {}
            c => {
                if cursor < line.len() {
                    line[cursor] = c;
                } else {
                    line.push(c);
                }
                cursor += 1;
            }
        }
    }
    line.into_iter().collect()
}

#[cfg(test)]
mod tests {
    // Summaries of runs built by hand: a run that passed, one that exited
    // 2 after a crash, one that broke an assertion and exited 0, one
    // stopped at its time limit, and two runs that part. Each is checked
    // line for line, so a change to the words shows here first.
    use super::*;
    use crate::stop::{Doing, Stop, Timeout};
    use crate::{Event, EventKind, Line};

    const JOB: u32 = 7;
    const WORKER: u32 = 8;
    const SIGSEGV: u32 = 11;

    fn event(step: u64, pid: u32, tid: u32, kind: EventKind) -> Event {
        Event {
            step,
            pid,
            tid,
            kind,
        }
    }

    fn write(step: u64, pid: u32, text: &str) -> Event {
        event(
            step,
            pid,
            pid,
            EventKind::Output {
                fd: 1,
                bytes: text.as_bytes().to_vec(),
            },
        )
    }

    fn init_mark(step: u64, text: &str) -> Event {
        event(
            step,
            rewind_init::INIT_PID,
            rewind_init::INIT_PID,
            EventKind::Mark { text: text.into() },
        )
    }

    /// The job, ./pool, started by init and run as process 7.
    fn started() -> Vec<Event> {
        vec![
            init_mark(1, "rewind-start"),
            event(
                2,
                rewind_init::INIT_PID,
                rewind_init::INIT_PID,
                EventKind::Fork {
                    child: JOB,
                    thread: false,
                },
            ),
            event(
                3,
                JOB,
                JOB,
                EventKind::Exec {
                    filename: "/bin/pool".into(),
                    argv: vec!["./pool".into()],
                    old_pid: JOB,
                },
            ),
        ]
    }

    /// The job's exit with wait status `status`, as the process ends and
    /// as init reports it.
    fn exited(step: u64, status: u32) -> Vec<Event> {
        vec![
            event(
                step,
                JOB,
                JOB,
                EventKind::Exit {
                    status,
                    comm: "pool".into(),
                    thread: false,
                },
            ),
            init_mark(step + 1, &format!("rewind-exit {status}")),
        ]
    }

    fn outcome(stop: Stop, step: u64, status: Option<i32>) -> RunOutcome {
        RunOutcome {
            stop,
            step,
            virtual_ns: 0,
            status,
            wall_ms: 0,
        }
    }

    fn recording<'a>(
        name: &'a str,
        verdict: &'a Verdict,
        outcome: &'a RunOutcome,
        trace: &'a Trace,
    ) -> Recording<'a> {
        Recording {
            id: "0123456789abcdef",
            name,
            verdict,
            outcome: Some(outcome),
            trace,
        }
    }

    fn trace(events: Vec<Event>) -> Trace {
        Trace {
            events,
            ..Default::default()
        }
    }

    #[test]
    fn a_run_that_passed_says_so_and_its_output() {
        // Exit 0 with two lines printed: the verdict, the ending, no
        // failing step, and both lines.
        let mut events = started();
        events.extend([write(4, JOB, "one\n"), write(5, JOB, "two\n")]);
        events.extend(exited(6, 0));
        let trace = trace(events);
        let ended = outcome(Stop::PoweredOff, 9, Some(0));
        let text = summary(recording("pool", &Verdict::Passed, &ended, &trace), None);
        assert_eq!(
            text,
            "run 0123456789abcdef: pool\n\
             verdict: passed\n\
             ended: exited:0 at step 9 (poweroff)\n\
             output (2 lines):\n  one\n  two\n"
        );
    }

    #[test]
    fn a_run_that_exited_2_after_a_crash_names_the_crash() {
        // A worker thread of the job takes SIGSEGV at step 5 and the job
        // exits 2: the reason is the exit, and the failing step is the
        // crash, in the job's process and the worker's thread.
        let mut events = started();
        events.push(write(4, JOB, "working\n"));
        events.push(event(
            5,
            JOB,
            WORKER,
            EventKind::Signal {
                signo: SIGSEGV,
                code: 1,
                addr: 0x10,
            },
        ));
        events.extend(exited(6, 2 << 8));
        let trace = trace(events);
        let verdict = Verdict::Failed {
            reasons: vec![Reason::Exited { code: 2 }],
        };
        let ended = outcome(Stop::PoweredOff, 9, Some(2 << 8));
        let text = summary(recording("pool", &verdict, &ended, &trace), None);
        assert_eq!(
            text,
            "run 0123456789abcdef: pool\n\
             verdict: failed\n  exited with status 2\n\
             ended: exited:2 at step 9 (poweroff)\n\
             failing step: 5, in process 7 (pool), thread 8: SIGSEGV delivered, fault address 0x10\n\
             output (1 line):\n  working\n"
        );
    }

    #[test]
    fn a_broken_assertion_fails_a_run_that_exited_0() {
        // The job breaks an `always` at step 4 and exits 0: the verdict
        // names the assertion where it broke, the ending says so, and the
        // failing step is the assertion's.
        let mut events = started();
        events.push(event(
            4,
            JOB,
            JOB,
            EventKind::Mark {
                text: "rewind-sdk 1 always fail even byte 3 is odd".into(),
            },
        ));
        events.extend(exited(6, 0));
        let trace = trace(events);
        let verdict = Verdict::judge(&Stop::PoweredOff, Some(0), &[], &trace);
        let ended = outcome(Stop::PoweredOff, 9, Some(0));
        let text = summary(recording("pool", &verdict, &ended, &trace), None);
        assert_eq!(
            text,
            "run 0123456789abcdef: pool\n\
             verdict: failed\n  \
             always \"even\" failed: byte 3 is odd, at step 4, in process 7 (pool), thread 7\n\
             ended: exited:0, assertion failed at step 9 (poweroff)\n\
             failing step: 4, in process 7 (pool), thread 7: always \"even\" failed: byte 3 is odd\n\
             output: none\n"
        );
    }

    #[test]
    fn a_run_stopped_at_its_time_limit_fails_where_it_stopped() {
        // The job prints 25 lines with color and a progress counter, and
        // the machine reaches its time limit at step 900 before the job
        // exits: the verdict is the timeout, the failing step the end, and
        // only the last OUTPUT_LINES lines show, as a terminal shows them.
        let mut events = started();
        for i in 0..25u64 {
            let text = format!("\x1b[1;32mline\x1b[0m {i}\r\x1b[Kline {i}\n");
            events.push(write(10 + i, JOB, &text));
        }
        let trace = trace(events);
        let stop = Stop::TimedOut(Timeout {
            since_exit_ms: 0,
            doing: Doing::MakingExits,
        });
        let verdict = Verdict::judge(&stop, None, &[], &trace);
        let ended = outcome(stop, 900, None);
        let text = summary(recording("pool", &verdict, &ended, &trace), None);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[..5],
            [
                "run 0123456789abcdef: pool",
                "verdict: failed",
                "  timed out: the machine reached its time limit",
                "ended: timed-out at step 900 (timed out while still making exits)",
                "failing step: 900, in process 7 (pool), thread 7: the machine stopped \
                 before the job exited, after write(1, \"\\u{1b}[1;32mline\\u{1b}[0m 24\\r\\u{1b}[Kline 24\\n\")",
            ]
        );
        assert_eq!(lines[5], "output (the last 20 of 25 lines):");
        assert_eq!(lines[6], "  line 5");
        assert_eq!(lines.last(), Some(&"  line 24"));
        assert_eq!(lines.len(), 6 + OUTPUT_LINES);
        assert!(!text.contains(ESCAPE));
    }

    #[test]
    fn two_runs_that_part_say_where_and_what_each_did() {
        // A failing run prints "three" where a passing run of the same
        // program prints "two": the comparison names the step in each run
        // and each run's write there.
        let failing_events = {
            let mut events = started();
            events.extend([write(4, JOB, "one\n"), write(5, JOB, "three\n")]);
            events.extend(exited(6, 1 << 8));
            events
        };
        let passing_events = {
            let mut events = started();
            events.extend([write(4, JOB, "one\n"), write(7, JOB, "two\n")]);
            events.extend(exited(8, 0));
            events
        };
        let (failing, passing) = (trace(failing_events), trace(passing_events));
        let failed = Verdict::Failed {
            reasons: vec![Reason::Exited { code: 1 }],
        };
        let failed_end = outcome(Stop::PoweredOff, 9, Some(1 << 8));
        let passed_end = outcome(Stop::PoweredOff, 11, Some(0));
        let text = summary(
            recording("fail", &failed, &failed_end, &failing),
            Some(Recording {
                id: "fedcba9876543210",
                ..recording("pass", &Verdict::Passed, &passed_end, &passing)
            }),
        );
        let comparison: Vec<&str> = text
            .lines()
            .skip_while(|line| !line.starts_with("compared with"))
            .collect();
        assert_eq!(
            comparison,
            [
                "compared with run fedcba9876543210: pass",
                "  verdict: passed",
                "  ended: exited:0 at step 11 (poweroff)",
                "the runs part at step 5 here and step 7 there, in the events of ./pool",
                "  here:  process 7 (pool), thread 7: write(1, \"three\\n\")",
                "  there: process 7 (pool), thread 7: write(1, \"two\\n\")",
            ]
        );

        // A run beside itself does the same things.
        let same = summary(
            recording("fail", &failed, &failed_end, &failing),
            Some(recording("fail", &failed, &failed_end, &failing)),
        );
        assert!(same.ends_with("the runs did the same things\n"), "{same}");
    }

    #[test]
    fn a_line_shows_as_a_terminal_shows_it() {
        // Colors and a window title go; a carriage return and a backspace
        // write over what came before; an erase cuts the line at the
        // cursor; a tab stays.
        assert_eq!(as_shown("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(as_shown("\x1b]0;title\x07text"), "text");
        assert_eq!(as_shown("\x1b]8;;http://x\x1b\\link"), "link");
        assert_eq!(as_shown("10%\r20%"), "20%");
        assert_eq!(as_shown("abc\rX"), "Xbc");
        assert_eq!(as_shown("abc\r\x1b[Kdone"), "done");
        assert_eq!(as_shown("ab\x08c"), "ac");
        assert_eq!(as_shown("a\tb"), "a\tb");
        let line = Line {
            step: 0,
            pid: 0,
            fd: 1,
            text: "\x1b[1mbold\x1b[m".into(),
        };
        assert_eq!(as_shown(&line.text), "bold");
    }
}
