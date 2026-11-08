//! What Previous and Next stop at: every event, or only the
//! events of one thread, one process or one kind, the build log's lines,
//! the events on one file, the processes starting and exiting, every
//! context switch, or the switches that give one thread or one process
//! the CPU and take it away.
//!
//! A stride that follows something, a thread or a file, names it when it
//! is chosen, from the event or the switch at the playhead then, so
//! stepping keeps following it as the playhead passes other events.

use rewind_trace::switches::Switch;
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
    ReadOpen,
    Close,
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
            EventKind::ReadOpen { .. } => Kind::ReadOpen,
            EventKind::Close { .. } => Kind::Close,
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
            Kind::ReadOpen => "open while written",
            Kind::Close => "close",
            Kind::Unknown => "unknown record",
        }
    }
}

/// What Previous and Next stop at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stride {
    /// Every event; kernel console lines only while the log shows them.
    Every,
    /// Every recorded context switch, including kernel and idle tasks.
    Switches,
    /// The switches that give one thread the CPU and the ones that take
    /// it away.
    ThreadSwitches { pid: u32, tid: u32 },
    /// The switches that give any thread of one process the CPU and the
    /// ones that hand it to another process, a kernel thread or idle.
    ProcessSwitches { pid: u32 },
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
    /// The run's bookmarks.
    Bookmarks,
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
            Stride::Switches
            | Stride::ThreadSwitches { .. }
            | Stride::ProcessSwitches { .. }
            | Stride::LogLine
            | Stride::Bookmarks => false,
        }
    }

    /// Whether `switch` hands the CPU to what this stride follows: the
    /// thread, or a thread of the process. A task without memory of its
    /// own, a kernel thread or a thread already past its exit, is not
    /// the thread it once was.
    fn takes(&self, switch: &Switch) -> bool {
        if !switch.memory || switch.is_idle() {
            return false;
        }
        match self {
            Stride::ThreadSwitches { pid, tid } => switch.pid == *pid && switch.tid == *tid,
            Stride::ProcessSwitches { pid } => switch.pid == *pid,
            _ => false,
        }
    }

    /// Whether the switch at `i` is a stop of this stride: the followed
    /// task takes the CPU there, or the task before it had it and loses
    /// it there.
    fn switch_stops_at(&self, switches: &[Switch], i: usize) -> bool {
        let takes = self.takes(&switches[i]);
        let had = i > 0 && self.takes(&switches[i - 1]);
        takes != had
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

    /// The strides that follow the task `switch` gave the CPU: its
    /// thread, when the thread is not the process's main one, and its
    /// process. Idle and kernel tasks have nothing to follow.
    pub fn following_switch(switch: &Switch) -> Vec<Stride> {
        if !switch.memory || switch.is_idle() {
            return Vec::new();
        }
        let mut strides = Vec::new();
        if switch.tid != switch.pid {
            strides.push(Stride::ThreadSwitches {
                pid: switch.pid,
                tid: switch.tid,
            });
        }
        strides.push(Stride::ProcessSwitches { pid: switch.pid });
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

/// The nearest of `steps`, in step order, after `step` going forward or
/// before it going back.
pub fn nearest(steps: &[u64], direction: Direction, step: u64) -> Option<u64> {
    match direction {
        Direction::Forward => {
            let after = steps.partition_point(|s| *s <= step);
            steps.get(after).copied()
        }
        Direction::Back => {
            let before = steps.partition_point(|s| *s < step);
            before.checked_sub(1).map(|i| steps[i])
        }
    }
}

/// The step of the next or previous stop of `stride` from `step`, with
/// the log shown under `filter` and the run's bookmarks at `bookmarks`;
/// None when there is none that way.
pub fn step_by(
    timeline: &Timeline,
    stride: &Stride,
    filter: LogFilter,
    bookmarks: &[u64],
    direction: Direction,
    step: u64,
) -> Option<u64> {
    // Switches, log lines and bookmarks have their own ordered stops.
    match stride {
        Stride::Switches => {
            let switches = &timeline.trace.switches;
            return match direction {
                Direction::Forward => {
                    let after = switches.partition_point(|s| s.step <= step);
                    switches.get(after).map(|s| s.step)
                }
                Direction::Back => {
                    let before = switches.partition_point(|s| s.step < step);
                    before.checked_sub(1).map(|i| switches[i].step)
                }
            };
        }
        Stride::ThreadSwitches { .. } | Stride::ProcessSwitches { .. } => {
            let switches = &timeline.trace.switches;
            return match direction {
                Direction::Forward => {
                    let after = switches.partition_point(|s| s.step <= step);
                    (after..switches.len())
                        .find(|i| stride.switch_stops_at(switches, *i))
                        .map(|i| switches[i].step)
                }
                Direction::Back => {
                    let before = switches.partition_point(|s| s.step < step);
                    (0..before)
                        .rev()
                        .find(|i| stride.switch_stops_at(switches, *i))
                        .map(|i| switches[i].step)
                }
            };
        }
        Stride::LogLine => {
            let lines: Vec<u64> = timeline.lines(filter).iter().map(|l| l.step).collect();
            return nearest(&lines, direction, step);
        }
        Stride::Bookmarks => return nearest(bookmarks, direction, step),
        _ => {}
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

/// How many stops `stride` has over the whole run, with the log shown
/// under `filter` and the run's bookmarks at `bookmarks`: the length of
/// the walk Next makes from the start, which the chooser shows beside
/// each stride so the size of a walk is known before it is chosen.
pub fn count(timeline: &Timeline, stride: &Stride, filter: LogFilter, bookmarks: &[u64]) -> usize {
    match stride {
        Stride::Switches => timeline.trace.switches.len(),
        Stride::ThreadSwitches { .. } | Stride::ProcessSwitches { .. } => {
            let switches = &timeline.trace.switches;
            (0..switches.len())
                .filter(|i| stride.switch_stops_at(switches, *i))
                .count()
        }
        Stride::LogLine => timeline.lines(filter).len(),
        Stride::Bookmarks => bookmarks.len(),
        _ => timeline
            .trace
            .events
            .iter()
            .filter(|e| stride.stops_at(e, filter))
            .count(),
    }
}

#[cfg(test)]
mod tests {
    // Stepping through a hand-built trace of two processes, one with two
    // threads, a file and kernel console lines, under each stride.
    use super::*;
    use rewind_trace::Trace;
    use rewind_trace::switches::Switch;

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
            switches: vec![
                Switch {
                    step: 9,
                    pid: 1,
                    tid: 1,
                    memory: true,
                    name: "init".into(),
                },
                Switch {
                    step: 19,
                    pid: 5,
                    tid: 5,
                    memory: true,
                    name: "five".into(),
                },
                Switch {
                    step: 35,
                    pid: 2,
                    tid: 2,
                    memory: false,
                    name: "kworker".into(),
                },
                Switch {
                    step: 79,
                    pid: 0,
                    tid: 0,
                    memory: false,
                    name: "swapper".into(),
                },
                Switch {
                    step: 90,
                    pid: 1,
                    tid: 1,
                    memory: true,
                    name: "init".into(),
                },
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
            while let Some(next) = step_by(&t, stride, filter, &[15, 65], direction, at) {
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
    fn context_switches_have_their_own_stops() {
        // Walk all switches, including kernel, idle and the tail after the last event.
        let (forward, back) = stops(&Stride::Switches, LogFilter::Output);
        assert_eq!(forward, vec![9, 19, 35, 79, 90]);
        assert_eq!(back, vec![90, 79, 35, 19, 9]);

        let mut t = timeline();
        let step = |t: &Timeline, direction, at| {
            step_by(t, &Stride::Switches, LogFilter::Output, &[], direction, at)
        };
        assert_eq!(step(&t, Direction::Forward, 20), Some(35));
        assert_eq!(step(&t, Direction::Back, 20), Some(19));
        assert_eq!(step(&t, Direction::Forward, 90), None);
        assert_eq!(step(&t, Direction::Back, 9), None);
        t.trace.switches.clear();
        assert_eq!(step(&t, Direction::Forward, 0), None);
        assert_eq!(step(&t, Direction::Back, t.total), None);
    }

    #[test]
    fn following_a_thread_s_switches_stops_where_it_takes_and_loses_the_cpu() {
        // Process 5 takes the CPU at 19 and loses it to the kernel thread
        // at 35; init has it from 9, loses it at 19 and gets it back at
        // 90. Neither the kernel thread's switch nor idle's is a stop of
        // its own.
        let five = Stride::ThreadSwitches { pid: 5, tid: 5 };
        assert_eq!(
            stops(&five, LogFilter::Output),
            (vec![19, 35], vec![35, 19])
        );
        let init = Stride::ProcessSwitches { pid: 1 };
        assert_eq!(stops(&init, LogFilter::Output).0, vec![9, 19, 90]);
        let nobody = Stride::ThreadSwitches { pid: 77, tid: 78 };
        assert_eq!(stops(&nobody, LogFilter::Output).0, Vec::<u64>::new());
    }

    #[test]
    fn a_switch_offers_its_thread_and_process() {
        // A worker thread's switch offers the thread and its process; a
        // main thread's offers the process alone; a kernel thread's and
        // idle's offer nothing.
        let worker = Switch {
            step: 1,
            pid: 5,
            tid: 7,
            memory: true,
            name: "five".into(),
        };
        assert_eq!(
            Stride::following_switch(&worker),
            vec![
                Stride::ThreadSwitches { pid: 5, tid: 7 },
                Stride::ProcessSwitches { pid: 5 },
            ]
        );
        let t = timeline();
        assert_eq!(
            Stride::following_switch(&t.trace.switches[1]),
            vec![Stride::ProcessSwitches { pid: 5 }]
        );
        assert_eq!(Stride::following_switch(&t.trace.switches[2]), vec![]);
        assert_eq!(Stride::following_switch(&t.trace.switches[3]), vec![]);
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
    fn bookmarks_are_stops_of_their_own() {
        // The bookmarks at 15 and 65 are the stops, though no event is
        // there.
        let (forward, back) = stops(&Stride::Bookmarks, LogFilter::Output);
        assert_eq!(forward, vec![15, 65]);
        assert_eq!(back, vec![65, 15]);
    }

    #[test]
    fn a_stride_s_count_is_the_length_of_its_walk() {
        // For each stride, the count the chooser shows is how many stops
        // Next visits from the start, under the same log filter and
        // bookmarks.
        let t = timeline();
        let strides = [
            Stride::Every,
            Stride::Switches,
            Stride::ThreadSwitches { pid: 5, tid: 5 },
            Stride::ProcessSwitches { pid: 1 },
            Stride::Thread { pid: 5, tid: 7 },
            Stride::Process { pid: 5 },
            Stride::Kind(Kind::Output),
            Stride::LogLine,
            Stride::File("/build/out.o".into()),
            Stride::Lifecycle,
            Stride::Bookmarks,
        ];
        for stride in &strides {
            for filter in [LogFilter::Output, LogFilter::WithConsole] {
                let walked = stops(stride, filter).0.len();
                assert_eq!(count(&t, stride, filter, &[15, 65]), walked, "{stride:?}");
            }
        }
        assert_eq!(count(&t, &Stride::Every, LogFilter::WithConsole, &[]), 10);
        assert_eq!(count(&t, &Stride::Switches, LogFilter::Output, &[]), 5);
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
