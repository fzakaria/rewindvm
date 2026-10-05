//! What Previous and Next stop at: every event, or only the
//! events of one thread, one process or one kind, the build log's lines,
//! the events on one file, or the processes starting and exiting.
//!
//! A stride that follows something, a thread or a file, names it when it
//! is chosen, from the event at the playhead then, so stepping keeps
//! following it as the playhead passes other events.

use rewind_trace::{Event, EventKind};

use crate::model::{LogFilter, Timeline};

/// The process id of the kernel's own events.
const KERNEL_PID: u32 = 0;

/// A kind of event, as the event card names its call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Console,
    Output,
    Exec,
    Fork,
    Exit,
    Signal,
    Open,
    Unlink,
    Rename,
    Mark,
    Unknown,
}

impl Kind {
    pub fn of(kind: &EventKind) -> Kind {
        match kind {
            EventKind::Console { .. } => Kind::Console,
            EventKind::Output { .. } => Kind::Output,
            EventKind::Exec { .. } => Kind::Exec,
            EventKind::Fork { .. } => Kind::Fork,
            EventKind::Exit { .. } => Kind::Exit,
            EventKind::Signal { .. } => Kind::Signal,
            EventKind::Open { .. } => Kind::Open,
            EventKind::Unlink { .. } => Kind::Unlink,
            EventKind::Rename { .. } => Kind::Rename,
            EventKind::Mark { .. } => Kind::Mark,
            EventKind::Unknown { .. } => Kind::Unknown,
        }
    }

    /// The call the event card shows for this kind.
    pub fn call(self) -> &'static str {
        match self {
            Kind::Console => "printk",
            Kind::Output => "write",
            Kind::Exec => "execve",
            Kind::Fork => "clone",
            Kind::Exit => "exit",
            Kind::Signal => "signal",
            Kind::Open => "open",
            Kind::Unlink => "unlink",
            Kind::Rename => "rename",
            Kind::Mark => "mark",
            Kind::Unknown => "unknown record",
        }
    }
}

/// What Previous and Next stop at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stride {
    /// Every event; kernel console lines only while the log shows them.
    Every,
    /// The events of one thread.
    Thread { pid: u32, tid: u32 },
    /// The events of any thread of one process.
    Process { pid: u32 },
    /// The events of one kind.
    Kind(Kind),
    /// The lines of the build log as it is filtered.
    LogLine,
    /// The events that write, remove or rename one file.
    File(String),
    /// Processes starting, replacing their program, and exiting.
    Lifecycle,
}

/// Which way to step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Back,
    Forward,
}

impl Stride {
    /// Whether `event` is one this stride stops at, with the log shown
    /// under `filter`.
    fn stops_at(&self, event: &Event, filter: LogFilter) -> bool {
        match self {
            Stride::Every => {
                let console = matches!(event.kind, EventKind::Console { .. });
                !console || filter == LogFilter::WithConsole
            }
            Stride::Thread { pid, tid } => event.pid == *pid && event.tid == *tid,
            Stride::Process { pid } => event.pid == *pid,
            Stride::Kind(kind) => Kind::of(&event.kind) == *kind,
            Stride::File(path) => touches(event, path),
            Stride::Lifecycle => match &event.kind {
                EventKind::Fork { thread, .. } | EventKind::Exit { thread, .. } => !thread,
                EventKind::Exec { .. } => true,
                _ => false,
            },
            Stride::LogLine => false,
        }
    }

    /// The strides that follow the event `event`: its thread, its
    /// process, its kind, and the file it names, if any. The kernel's own
    /// events have no thread or process to follow.
    pub fn following(event: &Event) -> Vec<Stride> {
        let mut strides = Vec::new();
        if event.pid != KERNEL_PID {
            if event.tid != event.pid {
                strides.push(Stride::Thread {
                    pid: event.pid,
                    tid: event.tid,
                });
            }
            strides.push(Stride::Process { pid: event.pid });
        }
        strides.push(Stride::Kind(Kind::of(&event.kind)));
        if let Some(path) = named_file(&event.kind) {
            strides.push(Stride::File(path.to_string()));
        }
        strides
    }
}

/// Whether `event` writes, removes or renames `path`, or renames
/// something onto it.
fn touches(event: &Event, path: &str) -> bool {
    match &event.kind {
        EventKind::Open { path: p, .. } | EventKind::Unlink { path: p } => p == path,
        EventKind::Rename { from, to } => from == path || to == path,
        _ => false,
    }
}

/// The file an event names, if any: the one written, removed, or renamed
/// onto.
fn named_file(kind: &EventKind) -> Option<&str> {
    match kind {
        EventKind::Open { path, .. } | EventKind::Unlink { path } => Some(path),
        EventKind::Rename { to, .. } => Some(to),
        _ => None,
    }
}

/// The step of the next or previous stop of `stride` from `step`, with
/// the log shown under `filter`; None when there is none that way.
pub fn step_by(
    timeline: &Timeline,
    stride: &Stride,
    filter: LogFilter,
    direction: Direction,
    step: u64,
) -> Option<u64> {
    // The log's lines are steps of their own, in step order.
    if *stride == Stride::LogLine {
        let lines = timeline.lines(filter);
        return match direction {
            Direction::Forward => {
                let after = lines.partition_point(|l| l.step <= step);
                lines.get(after).map(|l| l.step)
            }
            Direction::Back => {
                let before = lines.partition_point(|l| l.step < step);
                before.checked_sub(1).map(|i| lines[i].step)
            }
        };
    }

    // Events, from the playhead outward.
    let events = &timeline.trace.events;
    match direction {
        Direction::Forward => {
            let after = timeline.trace.index_after(step);
            events[after..]
                .iter()
                .find(|e| stride.stops_at(e, filter))
                .map(|e| e.step)
        }
        Direction::Back => {
            let before = events.partition_point(|e| e.step < step);
            events[..before]
                .iter()
                .rev()
                .find(|e| stride.stops_at(e, filter))
                .map(|e| e.step)
        }
    }
}

#[cfg(test)]
mod tests {
    // Stepping through a hand-built trace of two processes, one with two
    // threads, a file and kernel console lines, under each stride.
    use super::*;
    use rewind_trace::Trace;

    fn event(step: u64, pid: u32, tid: u32, kind: EventKind) -> Event {
        Event {
            step,
            pid,
            tid,
            kind,
        }
    }

    fn output(text: &str) -> EventKind {
        EventKind::Output {
            fd: 1,
            bytes: format!("{text}\n").into_bytes(),
        }
    }

    fn timeline() -> Timeline {
        let console = |text: &str| EventKind::Console {
            text: format!("{text}\n"),
        };
        let open = EventKind::Open {
            path: "/build/out.o".into(),
            flags: 0o1101,
        };
        let trace = Trace {
            events: vec![
                event(
                    10,
                    1,
                    1,
                    EventKind::Fork {
                        child: 5,
                        thread: false,
                    },
                ),
                event(20, 5, 5, output("five starts")),
                event(25, 0, 0, console("kernel says")),
                event(
                    30,
                    5,
                    6,
                    EventKind::Fork {
                        child: 7,
                        thread: true,
                    },
                ),
                event(40, 5, 7, open.clone()),
                event(50, 1, 1, output("one speaks")),
                event(55, 0, 0, console("kernel again")),
                event(60, 5, 7, output("thread seven")),
                event(
                    70,
                    5,
                    7,
                    EventKind::Rename {
                        from: "/build/out.o".into(),
                        to: "/build/final.o".into(),
                    },
                ),
                event(
                    80,
                    5,
                    5,
                    EventKind::Exit {
                        status: 0,
                        comm: "five".into(),
                        thread: false,
                    },
                ),
            ],
        };
        Timeline::new(trace, None, None)
    }

    /// Every stop of `stride` going forward from step 0, and going back
    /// from the end.
    fn stops(stride: &Stride, filter: LogFilter) -> (Vec<u64>, Vec<u64>) {
        let t = timeline();
        let walk = |direction, from| {
            let mut at = from;
            let mut seen = Vec::new();
            while let Some(next) = step_by(&t, stride, filter, direction, at) {
                seen.push(next);
                at = next;
            }
            seen
        };
        (
            walk(Direction::Forward, 0),
            walk(Direction::Back, t.total + 1),
        )
    }

    #[test]
    fn every_event_skips_console_lines_the_log_hides() {
        // With the console off, the kernel's lines at 25 and 55 are not
        // stops; with it on, they are.
        let (forward, back) = stops(&Stride::Every, LogFilter::Output);
        assert_eq!(forward, vec![10, 20, 30, 40, 50, 60, 70, 80]);
        assert_eq!(back, vec![80, 70, 60, 50, 40, 30, 20, 10]);
        let (forward, _) = stops(&Stride::Every, LogFilter::WithConsole);
        assert_eq!(forward, vec![10, 20, 25, 30, 40, 50, 55, 60, 70, 80]);
    }

    #[test]
    fn a_thread_or_a_process_stops_at_its_own_events() {
        // Thread 7 of process 5 did 40, 60 and 70; process 5 as a whole
        // also did 20, 30 and 80.
        let thread = Stride::Thread { pid: 5, tid: 7 };
        assert_eq!(stops(&thread, LogFilter::Output).0, vec![40, 60, 70]);
        let process = Stride::Process { pid: 5 };
        assert_eq!(
            stops(&process, LogFilter::Output).0,
            vec![20, 30, 40, 60, 70, 80]
        );
    }

    #[test]
    fn a_kind_a_file_and_lifecycles_stop_where_they_happen() {
        // Writes to standard output at 20, 50 and 60; the file out.o is
        // opened at 40 and renamed away at 70; process 5 starts at 10 and
        // exits at 80, and the thread's clone at 30 is not a process
        // starting.
        let writes = Stride::Kind(Kind::Output);
        assert_eq!(stops(&writes, LogFilter::Output).0, vec![20, 50, 60]);
        let file = Stride::File("/build/out.o".into());
        assert_eq!(stops(&file, LogFilter::Output).0, vec![40, 70]);
        assert_eq!(stops(&Stride::Lifecycle, LogFilter::Output).0, vec![10, 80]);
    }

    #[test]
    fn log_lines_follow_the_log_s_filter() {
        // The log's lines are the outputs, and the console's lines too
        // when the log shows them.
        assert_eq!(
            stops(&Stride::LogLine, LogFilter::Output).0,
            vec![20, 50, 60]
        );
        assert_eq!(
            stops(&Stride::LogLine, LogFilter::WithConsole).1,
            vec![60, 55, 50, 25, 20]
        );
    }

    #[test]
    fn an_event_offers_its_thread_process_kind_and_file() {
        // Thread 7's open of out.o offers the thread, the process, opens,
        // and the file; a main thread's event has no thread of its own to
        // offer, and the kernel's has no process either.
        let t = timeline();
        let open = &t.trace.events[4];
        assert_eq!(
            Stride::following(open),
            vec![
                Stride::Thread { pid: 5, tid: 7 },
                Stride::Process { pid: 5 },
                Stride::Kind(Kind::Open),
                Stride::File("/build/out.o".into()),
            ]
        );
        assert_eq!(
            Stride::following(&t.trace.events[1]),
            vec![Stride::Process { pid: 5 }, Stride::Kind(Kind::Output)]
        );
        assert_eq!(
            Stride::following(&t.trace.events[2]),
            vec![Stride::Kind(Kind::Console)]
        );
    }
}
