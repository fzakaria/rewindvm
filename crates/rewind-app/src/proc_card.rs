//! The card for a process or thread picked in the process tree: what the
//! trace says of it as of a step, apart from how the app draws it.

use rewind_trace::Process;
use rewind_trace::ending::ExitStatus;

use crate::model::{LogFilter, LogLine, ProcRow, RowKind, Stream, Timeline, command_name};

/// A row of the process tree, by its process id, its thread id (the pid
/// for a process row) and the step it started at, since Linux hands ids
/// out again once their process or thread has exited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pick {
    pub pid: u32,
    pub tid: u32,
    pub start: u64,
}

impl Pick {
    /// The pick for a row of the tree.
    pub fn of(row: &ProcRow) -> Pick {
        Pick {
            pid: row.pid,
            tid: row.tid,
            start: row.start,
        }
    }
}

/// Another row the card links to: a process's parent, or a thread's
/// process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Linked {
    pub pick: Pick,
    /// Its command, short: the file name of its program.
    pub command: String,
}

/// How a process or thread ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct End {
    pub step: u64,
    /// The kernel's exit status, for a process; a thread's exit has none.
    pub status: Option<ExitStatus>,
    /// Whether the playhead is at or past the end.
    pub reached: bool,
}

/// A thread of a process, alive from `start` until `end`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThreadSpan {
    pub tid: u32,
    pub start: u64,
    pub end: Option<u64>,
}

/// What the card says of a process or thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcCard<'a> {
    pub pick: Pick,
    pub kind: RowKind,
    /// Its program's name; a thread's process's.
    pub command: String,
    /// The command line as of the step, whole: its last exec by then, or
    /// its parent's before it execs. Empty for kernel threads.
    pub argv: Vec<String>,
    /// For a process, its parent; for a thread, its process.
    pub parent: Option<Linked>,
    /// The step it was forked at, and the steps it exec'd at.
    pub start: u64,
    pub execs: Vec<u64>,
    /// None while it runs to the end of the recording.
    pub end: Option<End>,
    /// A process's threads other than its main one; none for a thread.
    pub threads: Vec<ThreadSpan>,
    /// The lines its process wrote to standard output and error, up to
    /// the step, and how many more it writes after it.
    pub output: Vec<&'a LogLine>,
    pub later_lines: usize,
}

/// A command line as a shell would take it back: arguments that are
/// empty or hold spaces, quotes or backslashes are quoted.
pub fn command_line(argv: &[String]) -> String {
    let word = |arg: &String| {
        let bare = !arg.is_empty()
            && !arg
                .chars()
                .any(|c| c.is_whitespace() || matches!(c, '\'' | '"' | '\\'));
        if bare {
            return arg.clone();
        }
        format!("'{}'", arg.replace('\'', r"'\''"))
    };
    argv.iter().map(word).collect::<Vec<_>>().join(" ")
}

/// The card for `pick` as of `step`, or None when the run has no such
/// row.
pub fn proc_card(t: &Timeline, pick: Pick, step: u64) -> Option<ProcCard<'_>> {
    let row = t.rows.iter().find(|r| Pick::of(r) == pick)?;
    let index = process_index(&t.procs, row)?;
    let process = &t.procs[index];
    let command_of = |pick: Pick| {
        t.rows
            .iter()
            .find(|r| Pick::of(r) == pick)
            .map_or_else(String::new, |r| command_name(r.label_at(step)))
    };
    let linked = |pick: Pick| Linked {
        command: command_of(pick),
        pick,
    };
    let process_pick = Pick {
        pid: process.pid,
        tid: process.pid,
        start: process.start,
    };

    // A process names its parent, execs and threads; a thread names its
    // process.
    let (command, parent, execs, threads) = match row.kind {
        RowKind::Process => (
            command_name(row.label_at(step)),
            parent_index(&t.procs, index).map(|j| {
                let parent = &t.procs[j];
                linked(Pick {
                    pid: parent.pid,
                    tid: parent.pid,
                    start: parent.start,
                })
            }),
            process.execs.iter().map(|(at, _)| *at).collect(),
            process
                .threads
                .iter()
                .map(|(tid, start, end)| ThreadSpan {
                    tid: *tid,
                    start: *start,
                    end: *end,
                })
                .collect(),
        ),
        RowKind::Thread => (
            command_of(process_pick),
            Some(linked(process_pick)),
            Vec::new(),
            Vec::new(),
        ),
    };

    // The command line of its last exec by the step, or of its first one
    // when the step is before it, or its parent's when it never exec'd.
    let argv = process
        .execs
        .iter()
        .rev()
        .find(|(at, _)| *at <= step)
        .or(process.execs.first())
        .map_or_else(|| process.argv.clone(), |(_, argv)| argv.clone());

    let end = row.end.map(|end| End {
        step: end,
        status: match row.kind {
            RowKind::Process => row.status.map(ExitStatus::from_raw),
            RowKind::Thread => None,
        },
        reached: step >= end,
    });

    // Its process's lines on standard output and error, from its own
    // lifetime, since a pid is handed out again after it exits.
    let mut output: Vec<&LogLine> = t
        .lines(LogFilter::Output)
        .iter()
        .filter(|l| matches!(l.stream, Stream::Stdout | Stream::Stderr))
        .filter(|l| l.pid == process.pid && l.step >= process.start)
        .filter(|l| process.end.is_none_or(|end| l.step <= end))
        .collect();
    let shown = output.partition_point(|l| l.step <= step);
    let later_lines = output.len() - shown;
    output.truncate(shown);

    Some(ProcCard {
        pick,
        kind: row.kind,
        command,
        argv,
        parent,
        start: row.start,
        execs,
        end,
        threads,
        output,
        later_lines,
    })
}

/// The process a row of the tree is, or is a thread of.
fn process_index(procs: &[Process], row: &ProcRow) -> Option<usize> {
    procs.iter().position(|p| {
        p.pid == row.pid
            && match row.kind {
                RowKind::Process => p.start == row.start,
                RowKind::Thread => p
                    .threads
                    .iter()
                    .any(|(tid, start, _)| *tid == row.tid && *start == row.start),
            }
    })
}

/// A process's parent: of the processes with its parent's pid, the latest
/// to start by the time it was forked, as the tree finds it.
fn parent_index(procs: &[Process], index: usize) -> Option<usize> {
    let child = &procs[index];
    if child.parent == 0 {
        return None;
    }
    procs
        .iter()
        .enumerate()
        .filter(|(j, p)| *j != index && p.pid == child.parent && p.start <= child.start)
        .max_by_key(|(_, p)| p.start)
        .map(|(j, _)| j)
}

#[cfg(test)]
mod tests {
    // The card for rows of the model's small build, and of a trace with a
    // long command line, at steps before, at and after their ends.
    use super::*;
    use crate::model::tests::{build, exec, fork, out};
    use rewind_trace::{Trace, signal};

    /// The row of `t`'s tree for `tid`.
    fn pick(t: &Timeline, tid: u32) -> Pick {
        Pick::of(t.rows.iter().find(|r| r.tid == tid).unwrap())
    }

    #[test]
    fn a_process_card_has_its_command_line_parent_start_and_exit() {
        // The compiler, forked by make at step 5, exec'd at 6, exits 0 at
        // 9; at step 7 it has not exited yet, at 9 it has.
        let t = build();
        let card = proc_card(&t, pick(&t, 3), 7).unwrap();
        assert_eq!(card.kind, RowKind::Process);
        assert_eq!(card.command, "cc");
        assert_eq!(card.argv, vec!["cc", "-c", "pool.c"]);
        assert_eq!(
            card.parent,
            Some(Linked {
                pick: pick(&t, 2),
                command: "make".to_string(),
            })
        );
        assert_eq!((card.start, card.execs.clone()), (5, vec![6]));
        assert_eq!(
            card.end,
            Some(End {
                step: 9,
                status: Some(ExitStatus::Code(0)),
                reached: false,
            })
        );
        assert!(proc_card(&t, pick(&t, 3), 9).unwrap().end.unwrap().reached);
    }

    #[test]
    fn a_long_command_line_is_kept_whole() {
        // Forty arguments, none cut or dropped.
        let args: Vec<String> = (0..40).map(|i| format!("--flag-{i}=value")).collect();
        let mut argv = vec!["/nix/store/x-nix/bin/nix"];
        argv.extend(args.iter().map(String::as_str));
        let events = vec![
            exec(1, 1, &["init"]),
            fork(2, 1, 7, false),
            exec(3, 7, &argv),
        ];
        let t = Timeline::new(
            Trace {
                events,
                ..Default::default()
            },
            None,
            None,
        );
        let card = proc_card(&t, pick(&t, 7), 3).unwrap();
        assert_eq!(card.argv, argv);
        assert_eq!(card.command, "nix");
    }

    #[test]
    fn a_command_line_quotes_the_arguments_a_shell_would_split() {
        // A plain word stays bare; one with a space, one with a quote and
        // an empty one are quoted, the quote escaped.
        let argv: Vec<String> = ["nix", "build", "a b", "it's", ""]
            .iter()
            .map(|a| a.to_string())
            .collect();
        assert_eq!(command_line(&argv), r#"nix build 'a b' 'it'\''s' ''"#);
    }

    #[test]
    fn output_is_split_at_the_step() {
        // bash writes a line at step 2 and another at 10: at step 5 the
        // card holds the first and counts the second as later.
        let t = build();
        let card = proc_card(&t, pick(&t, 1), 5).unwrap();
        let shown: Vec<(u64, &str)> = card
            .output
            .iter()
            .map(|l| (l.step, l.text.as_str()))
            .collect();
        assert_eq!(shown, vec![(2, "Running phase: buildPhase")]);
        assert_eq!(card.later_lines, 1);

        // Only its own lines: the compiler's warning went to stderr.
        let cc = proc_card(&t, pick(&t, 3), 9).unwrap();
        assert_eq!(cc.output.len(), 1);
        assert_eq!(cc.output[0].stream, Stream::Stderr);
    }

    #[test]
    fn a_thread_card_names_its_process_and_its_own_span() {
        // The test's worker thread, started at 13 and gone at 15: its
        // process is the test, it has no exit status of its own, and the
        // test's threads list has it.
        let t = build();
        let card = proc_card(&t, pick(&t, 5), 14).unwrap();
        assert_eq!(card.kind, RowKind::Thread);
        assert_eq!(card.command, "test_pool");
        assert_eq!(card.parent.as_ref().unwrap().pick, pick(&t, 4));
        assert_eq!(card.start, 13);
        assert_eq!(
            card.end,
            Some(End {
                step: 15,
                status: None,
                reached: false,
            })
        );
        let test = proc_card(&t, pick(&t, 4), 14).unwrap();
        assert_eq!(
            test.threads,
            vec![ThreadSpan {
                tid: 5,
                start: 13,
                end: Some(15),
            }]
        );
    }

    #[test]
    fn a_process_killed_by_a_signal_says_so_and_outlives_the_tree() {
        // The test dies of SIGSEGV at 16; at 18 it has left the alive
        // tree, and its card still answers.
        let t = build();
        let card = proc_card(&t, pick(&t, 4), 18).unwrap();
        assert_eq!(
            card.end,
            Some(End {
                step: 16,
                status: Some(ExitStatus::Signal {
                    signo: signal::SIGSEGV,
                    core: true,
                }),
                reached: true,
            })
        );
        assert!(t.alive_rows(18).all(|r| r.pid != 4));
    }

    #[test]
    fn a_process_alive_at_the_end_has_no_end() {
        // bash never exits in the build.
        let t = build();
        let card = proc_card(&t, pick(&t, 1), t.total).unwrap();
        assert_eq!(card.end, None);
        assert_eq!(card.parent, None);
        assert_eq!(card.later_lines, 0);
        assert_eq!(card.output.len(), 2);
    }

    #[test]
    fn a_pick_the_run_has_no_row_for_has_no_card() {
        // Pid 9 never ran.
        let t = build();
        let none = Pick {
            pid: 9,
            tid: 9,
            start: 0,
        };
        assert_eq!(proc_card(&t, none, 5), None);
    }

    #[test]
    fn lines_before_a_reused_pid_started_are_not_its_own() {
        // Pid 7 runs echo, exits, and is handed out again to cat: cat's
        // card has only cat's line.
        let events = vec![
            exec(1, 1, &["sh"]),
            fork(2, 1, 7, false),
            exec(3, 7, &["echo", "one"]),
            out(4, 7, 1, "one\n"),
            crate::model::tests::exit(5, 7, 7, 0, "echo", false),
            fork(6, 1, 7, false),
            exec(7, 7, &["cat"]),
            out(8, 7, 1, "two\n"),
        ];
        let t = Timeline::new(
            Trace {
                events,
                ..Default::default()
            },
            None,
            None,
        );
        let cat = t.rows.iter().find(|r| r.pid == 7 && r.start == 6).unwrap();
        let card = proc_card(&t, Pick::of(cat), 8).unwrap();
        let texts: Vec<&str> = card.output.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, vec!["two"]);
    }
}
