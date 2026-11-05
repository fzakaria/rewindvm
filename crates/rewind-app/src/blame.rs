//! Blame for memory: click a value in the source panel and the app names
//! the line, thread and step that last wrote it, as `rewind blame RUN --at
//! STEP` does.
//!
//! `rewind where --json` already says, for each expression on the lines
//! around a frame's line, whether it is in memory and small enough for
//! the CPU's debug registers to watch. Those get an underline. A click
//! asks the engine to search back for the value's writes by its address,
//! which replays keyframe windows newest first and says how far it has
//! got as it goes. Nothing waits on the answer: the panel stays live, the
//! clicked name and a popover beside it show the search's counts, and the
//! timeline shows the stretch searched so far.

use std::ops::Range;

pub use rewind_trace::blamed::{AccessKind, Blamed, BlamedAccess, SourcePlace, ThreadAt};
pub use rewind_trace::located::{Expression, Location, ValueClass};
pub use rewind_trace::searching::{Phase, Searching};

use crate::describe::thousands;
use crate::engine::EngineError;
use crate::source::{AfterAccess, Frame, Thread};

/// A value to blame: the expression clicked, its bytes in the process's
/// memory, and the thread, step and line it was clicked at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub text: String,
    pub address: u64,
    pub size: u64,
    pub class: ValueClass,
    pub pid: u32,
    pub tid: u32,
    pub step: u64,
}

/// What a search asks the engine for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Ask {
    /// The value's last `n` writes.
    Writes(usize),
    /// Every read and write since the process began.
    Reads,
}

/// One search and what has come of it.
#[derive(Clone, Debug)]
pub struct Blame {
    pub target: Target,
    pub ask: Ask,
    /// The engine's latest word on how far it has got.
    pub progress: Option<Searching>,
    /// The answer, once the engine gives it.
    pub answer: Option<Result<Blamed, EngineError>>,
}

impl Blame {
    pub fn new(target: Target, ask: Ask) -> Blame {
        Blame {
            target,
            ask,
            progress: None,
            answer: None,
        }
    }

    /// Whether the engine is still searching.
    pub fn searching(&self) -> bool {
        self.answer.is_none()
    }

    /// The answer, when it came and is one.
    pub fn blamed(&self) -> Option<&Blamed> {
        self.answer.as_ref()?.as_ref().ok()
    }

    /// The stretch of steps searched so far, from the earliest step to
    /// the target's, for the timeline's band.
    pub fn searched(&self) -> Option<(u64, u64)> {
        if let Some(blamed) = self.blamed() {
            return Some((blamed.searched_from, blamed.step));
        }
        let p = self.progress.as_ref()?;
        let reached = match p.phase {
            Phase::Restoring => p.window.1,
            Phase::Searching | Phase::Naming => p.window.0,
        };
        Some((reached, p.before))
    }

    /// The step of the write the answer names last, for the timeline's
    /// mark: the latest write.
    pub fn write_step(&self) -> Option<u64> {
        Some(self.blamed()?.last_write()?.step)
    }

    /// Where going to the last write found looks: its step, and its
    /// writer just after it.
    pub fn last_write_look(&self) -> Option<(u64, (Thread, Option<AfterAccess>))> {
        let blamed = self.blamed()?;
        let write = blamed.last_write()?;
        Some((write.step, look_after(blamed, write)))
    }

    /// The chip's words in the app bar, `elapsed_ms` into the search.
    pub fn chip(&self, elapsed_ms: u64) -> String {
        let text = &self.target.text;
        match (&self.answer, &self.progress) {
            (Some(Ok(blamed)), _) => match blamed.last_write() {
                Some(write) => format!("blame {text} \u{b7} step {}", thousands(write.step)),
                None => format!("blame {text} \u{b7} no write found"),
            },
            (Some(Err(_)), _) => format!("blame {text} \u{b7} failed"),
            (None, Some(p)) => {
                let seconds = elapsed_ms / 1000;
                let doing = match p.phase {
                    Phase::Restoring => "restoring keyframe",
                    Phase::Searching => "searching",
                    Phase::Naming => "naming the lines",
                };
                format!("blame {text} \u{b7} {doing} \u{b7} {seconds} s")
            }
            (None, None) => format!("blame {text} \u{b7} starting"),
        }
    }

    /// The popover's lines while the engine searches, `elapsed_ms` into
    /// it: what it is doing, then its counts.
    pub fn progress_lines(&self, elapsed_ms: u64) -> Vec<String> {
        let Some(p) = &self.progress else {
            return vec!["starting the search".to_string()];
        };
        let seconds = format!("{:.1} s", elapsed_ms as f64 / 1000.0);
        match p.phase {
            Phase::Restoring => vec![
                format!(
                    "restoring keyframe at step {} \u{b7} {seconds}",
                    thousands(p.window.0)
                ),
                match p.windows {
                    0 => "nothing searched yet".to_string(),
                    n => format!(
                        "searched back {} steps \u{b7} {} \u{b7} {}",
                        thousands(p.steps),
                        counted(n as u64, "window"),
                        counted(p.writes, "write")
                    ),
                },
            ],
            Phase::Searching => vec![
                format!("window from step {}", thousands(p.window.0)),
                format!(
                    "searched back {} steps \u{b7} {} \u{b7} {} seen \u{b7} {seconds}",
                    thousands(p.steps),
                    counted(p.windows.max(1) as u64, "window"),
                    counted(p.writes, "write")
                ),
            ],
            Phase::Naming => vec![
                format!(
                    "found {} in {} steps",
                    counted(p.writes, "write"),
                    thousands(p.steps)
                ),
                "naming the line and where the other threads were".to_string(),
            ],
        }
    }
}

/// `n` of `thing`, with a plural s when not one.
pub fn counted(n: u64, thing: &str) -> String {
    match n {
        1 => format!("1 {thing}"),
        n => format!("{n} {thing}s"),
    }
}

/// The expressions a person can click on line `line` of the file `frame`
/// is shown in, `text` being the line: those in memory and small enough
/// to watch, each with the byte range of its last name in `text`.
pub fn blameable_on<'a>(
    frame: &'a Frame,
    line: u32,
    text: &str,
) -> Vec<(Range<usize>, &'a Expression)> {
    let mut found: Vec<(Range<usize>, &Expression)> = frame
        .expressions
        .iter()
        .filter(|e| e.line == line && e.location.watchable().is_some())
        .filter_map(|e| Some((byte_range(text, e.name_start, e.end)?, e)))
        .collect();
    found.sort_by_key(|(range, _)| range.start);
    found.dedup_by(|b, a| b.0.start < a.0.end);
    found
}

/// The bytes of `text` from character `start` to character `end`.
fn byte_range(text: &str, start: u32, end: u32) -> Option<Range<usize>> {
    let at = |n: u32| -> Option<usize> {
        if n as usize == text.chars().count() {
            return Some(text.len());
        }
        text.char_indices().nth(n as usize).map(|(i, _)| i)
    };
    let range = at(start)?..at(end)?;
    (!range.is_empty()).then_some(range)
}

/// The value a click on `expression` in thread `tid` of process `pid` at
/// `step` asks about.
pub fn target(expression: &Expression, pid: u32, tid: u32, step: u64) -> Option<Target> {
    target_of(&expression.text, &expression.location, (pid, tid), step)
}

/// The value called `text`, at `location`, in thread `tid` of process
/// `pid` at `step`, when it can be watched.
pub fn target_of(
    text: &str,
    location: &Location,
    (pid, tid): (u32, u32),
    step: u64,
) -> Option<Target> {
    let (address, size) = location.watchable()?;
    let Location::Memory { class, .. } = location else {
        return None;
    };
    Some(Target {
        text: text.to_string(),
        address,
        size,
        class: *class,
        pid,
        tid,
        step,
    })
}

/// The values to search for ahead of a click: those on the line of
/// `frame`, the frame of thread `tid` of process `pid` the source panel
/// shows at `step`, when `step` is `failure`, the step the run failed at.
/// None anywhere else, nor for a frame with no line.
pub fn ahead_of_click(
    failure: Option<u64>,
    step: u64,
    shown: Option<(&Frame, (u32, u32))>,
) -> Vec<Target> {
    let Some((frame, (pid, tid))) = shown.filter(|_| failure == Some(step)) else {
        return Vec::new();
    };
    let Some(line) = frame.line else {
        return Vec::new();
    };
    frame
        .expressions
        .iter()
        .filter(|e| e.line == line)
        .filter_map(|e| target(e, pid, tid, step))
        .collect()
}

/// A source place as `file:line`, by the file's last part.
pub fn file_line(place: &SourcePlace) -> Option<String> {
    let file = place.file.as_ref()?;
    let name = file.rsplit('/').next().unwrap_or(file);
    Some(format!("{name}:{}", place.line?))
}

/// Where an access was made, in words: its source line, or the kernel's
/// symbol for one the kernel made for a system call.
pub fn access_place(access: &BlamedAccess) -> String {
    if let Some(symbol) = &access.kernel {
        return match access.syscall {
            Some(n) => format!("the kernel, {symbol}, system call {n}"),
            None => format!("the kernel, {symbol}"),
        };
    }
    let Some(place) = &access.place else {
        return access.pc.clone().unwrap_or_default();
    };
    let at = file_line(place).unwrap_or_default();
    match &place.function {
        Some(function) => format!("{at}  {function}").trim().to_string(),
        None => at,
    }
}

/// A thread's place at the last write, in words: its line and function,
/// and the function at the top of its stack when that is another, as a
/// thread in printf waits in the C library's lock.
pub fn thread_place(thread: &ThreadAt) -> String {
    if let Some(error) = &thread.error {
        return error.clone();
    }
    let place = thread.place.as_ref();
    let line = place.and_then(file_line).unwrap_or_default();
    let function = place.and_then(|p| p.function.clone()).unwrap_or_default();
    let innermost = thread
        .innermost
        .clone()
        .filter(|f| *f != function)
        .map(|f| format!(" \u{b7} in {f}"))
        .unwrap_or_default();
    format!("{line} {function}{innermost}").trim().to_string()
}

/// `over`'s styles laid over `base`'s on a line, as GPUI takes its
/// highlights, in order and not overlapping: each stretch where either
/// has a style gets `merge` of the two, or the one there is.
pub fn overlay<S: Clone>(
    base: &[(Range<usize>, S)],
    over: &[(Range<usize>, S)],
    merge: impl Fn(&S, &S) -> S,
) -> Vec<(Range<usize>, S)> {
    let mut edges: Vec<usize> = base
        .iter()
        .chain(over)
        .flat_map(|(r, _)| [r.start, r.end])
        .collect();
    edges.sort_unstable();
    edges.dedup();
    let at = |list: &[(Range<usize>, S)], i: usize| {
        list.iter()
            .find(|(r, _)| r.start <= i && i < r.end)
            .map(|(_, s)| s.clone())
    };
    edges
        .windows(2)
        .filter_map(|w| {
            let style = match (at(base, w[0]), at(over, w[0])) {
                (Some(b), Some(o)) => merge(&b, &o),
                (Some(b), None) => b,
                (None, Some(o)) => o,
                (None, None) => return None,
            };
            Some((w[0]..w[1], style))
        })
        .collect()
}

/// What going to `access`, one of `blamed`'s, looks just after: the
/// bytes of the value it touched and its place in its step. None when the
/// value's address does not read as hex.
pub fn after_access(blamed: &Blamed, access: &BlamedAccess) -> Option<AfterAccess> {
    let value = blamed.values.get(access.value)?;
    let address = u64::from_str_radix(value.address.trim_start_matches("0x"), 16).ok()?;
    Some(AfterAccess {
        address,
        size: value.size,
        kind: access.kind,
        hit: access.hit,
    })
}

/// Whose stack going to `access` shows, and just after what: the thread
/// that made it, just after it.
pub fn look_after(blamed: &Blamed, access: &BlamedAccess) -> (Thread, Option<AfterAccess>) {
    let thread = Thread::Of {
        pid: blamed.pid,
        tid: access.tid,
    };
    (thread, after_access(blamed, access))
}

/// Which row of its thread's lane an access sits on: a thread often
/// writes a value and reads it again in the same step, which puts both
/// at the same place along the lane, and a read drawn over a write would
/// hide it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaneRow {
    Writes,
    Reads,
}

/// The row of a lane that an access of `kind` sits on.
pub fn row_of(kind: AccessKind) -> LaneRow {
    match kind {
        AccessKind::Write => LaneRow::Writes,
        AccessKind::Read => LaneRow::Reads,
    }
}

/// The lanes of an answer with reads: each thread that accessed a value,
/// in the order it first did, with its accesses as fractions of the steps
/// from the first access to the last.
pub fn lanes(blamed: &Blamed) -> Vec<(u32, Vec<(f32, &BlamedAccess)>)> {
    let (Some(first), Some(last)) = (blamed.accesses.first(), blamed.accesses.last()) else {
        return Vec::new();
    };
    let span = (last.step - first.step).max(1) as f32;
    let mut lanes: Vec<(u32, Vec<(f32, &BlamedAccess)>)> = Vec::new();
    for access in &blamed.accesses {
        let at = (access.step - first.step) as f32 / span;
        match lanes.iter_mut().find(|(tid, _)| *tid == access.tid) {
            Some((_, accesses)) => accesses.push((at, access)),
            None => lanes.push((access.tid, vec![(at, access)])),
        }
    }
    lanes
}

#[cfg(test)]
mod tests {
    // Which values a line underlines, and what a search says as it goes,
    // from expressions and progress as the engine prints them.
    use super::*;

    /// A frame at pool.c:77 with p->queue in memory on its line, a value
    /// in a register on it, and p->stopping in memory on the line before.
    fn frame_at_77() -> Frame {
        serde_json::from_value(serde_json::json!({
            "level": 0, "function": "worker", "file": "src/pool.c", "fullname": null,
            "line": 77, "pc": "0x1", "object": null, "arguments": [], "locals": [],
            "expressions": [
                {"text": "p->queue", "value": "0x0", "line": 77, "start": 0, "name_start": 3,
                 "end": 8, "location": {"kind": "memory", "address": "0x5556f833c068",
                 "size": 8, "class": "pointer", "type": "struct queue *"}},
                {"text": "job", "value": "16", "line": 77, "start": 0, "name_start": 0,
                 "end": 3, "location": {"kind": "register", "register": "r15"}},
                {"text": "p->stopping", "value": "0", "line": 74, "start": 0,
                 "name_start": 3, "end": 11, "location": {"kind": "memory",
                 "address": "0x5556f833c070", "size": 4, "class": "signed", "type": "int"}}
            ],
            "evaluated": []
        }))
        .unwrap()
    }

    /// Ahead of a click, only the values in memory on the frame's own
    /// line are searched for, and only at the step the run failed at: at
    /// another step, with no frame shown, or for a frame without a line,
    /// none are.
    #[test]
    fn searches_ahead_are_for_the_failing_line_alone() {
        let frame = frame_at_77();
        let whose = Some((&frame, (166, 171)));
        let at_failure = ahead_of_click(Some(4782), 4782, whose);
        let texts: Vec<&str> = at_failure.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, ["p->queue"]);
        assert_eq!((at_failure[0].pid, at_failure[0].step), (166, 4782));

        assert!(ahead_of_click(Some(4782), 4700, whose).is_empty());
        assert!(ahead_of_click(None, 4782, whose).is_empty());
        assert!(ahead_of_click(Some(4782), 4782, None).is_empty());
        let mut no_line = frame_at_77();
        no_line.line = None;
        assert!(ahead_of_click(Some(4782), 4782, Some((&no_line, (166, 171)))).is_empty());
    }

    /// Going to an access looks just after it: a write by its place among
    /// the step's writes, a read by its place among all the step's
    /// accesses, each at the bytes of the value it touched.
    #[test]
    fn going_to_an_access_looks_just_after_it() {
        let blamed: Blamed = serde_json::from_value(serde_json::json!({
            "run": "r", "step": 4782, "pid": 166, "tid": 171, "place": null,
            "values": [
                {"text": "p->stopping", "address": "0x5556f833c070", "size": 4,
                 "class": "signed", "type": "int", "now": "1", "initial": "0"},
                {"text": "p->queue", "address": "0x5556f833c068", "size": 8,
                 "class": "pointer", "type": null, "now": "0x0", "initial": null}
            ],
            "accesses": [
                {"step": 4756, "value": 0, "kind": "read", "tid": 171, "thread": "t",
                 "before": "0", "after": "0", "pc": "0x1", "kernel": null, "syscall": null,
                 "place": null, "hit": 2},
                {"step": 4768, "value": 1, "kind": "write", "tid": 166, "thread": "t",
                 "before": "0x1", "after": "0x0", "pc": "0x2", "kernel": null, "syscall": null,
                 "place": null, "hit": 1}
            ],
            "threads": [], "searched_from": 4742, "since": 4742, "windows": 2, "listed": "accesses", "dropped": 0,
            "elapsed_ms": 1600
        }))
        .unwrap();
        assert_eq!(
            after_access(&blamed, &blamed.accesses[0]),
            Some(AfterAccess {
                address: 0x5556f833c070,
                size: 4,
                kind: AccessKind::Read,
                hit: 2
            })
        );
        assert_eq!(
            after_access(&blamed, &blamed.accesses[1]),
            Some(AfterAccess {
                address: 0x5556f833c068,
                size: 8,
                kind: AccessKind::Write,
                hit: 1
            })
        );
    }

    /// A thread that writes a value and reads it again in one step, as
    /// each deposit of the bank example does, shows both: the write and
    /// the read land at the same place along the lane, on rows of their
    /// own.
    #[test]
    fn a_write_and_a_read_in_one_step_are_on_rows_of_their_own() {
        let blamed: Blamed = serde_json::from_value(serde_json::json!({
            "run": "r", "step": 3291, "pid": 139, "tid": 139, "place": null,
            "values": [
                {"text": "balance", "address": "0x56055a0ef030", "size": 8,
                 "class": "signed", "type": "long", "now": "310", "initial": "0"}
            ],
            "accesses": [
                {"step": 3233, "value": 0, "kind": "read", "tid": 140, "thread": "bank",
                 "before": "0", "after": "0", "pc": "0x1", "kernel": null, "syscall": null,
                 "place": null, "hit": 1},
                {"step": 3234, "value": 0, "kind": "write", "tid": 140, "thread": "bank",
                 "before": "0", "after": "10", "pc": "0x2", "kernel": null, "syscall": null,
                 "place": null, "hit": 1},
                {"step": 3234, "value": 0, "kind": "read", "tid": 140, "thread": "bank",
                 "before": "10", "after": "10", "pc": "0x1", "kernel": null, "syscall": null,
                 "place": null, "hit": 2}
            ],
            "threads": [], "searched_from": 3210, "since": 3210, "windows": 1, "listed": "accesses", "dropped": 0,
            "elapsed_ms": 900
        }))
        .unwrap();
        let lanes = lanes(&blamed);
        let [(140, marks)] = lanes.as_slice() else {
            panic!("one lane, thread 140's: {lanes:?}");
        };
        let [_, (write_at, write), (read_at, read)] = marks.as_slice() else {
            panic!("three marks: {marks:?}");
        };
        assert_eq!(write_at, read_at);
        assert_eq!(row_of(write.kind), LaneRow::Writes);
        assert_eq!(row_of(read.kind), LaneRow::Reads);
    }

    fn expression(text: &str, line: u32, name: (u32, u32), location: Location) -> Expression {
        Expression {
            text: text.into(),
            value: "0x0".into(),
            location,
            line,
            start: 0,
            name_start: name.0,
            end: name.1,
        }
    }

    fn memory(address: &str, size: u64) -> Location {
        Location::Memory {
            address: address.into(),
            size,
            class: ValueClass::Pointer,
            type_name: "struct queue *".into(),
        }
    }

    fn frame(expressions: Vec<Expression>) -> Frame {
        Frame {
            level: 0,
            function: Some("worker".into()),
            file: Some("src/pool.c".into()),
            fullname: None,
            line: Some(77),
            pc: "0x401000".into(),
            object: None,
            arguments: Vec::new(),
            locals: Vec::new(),
            expressions,
            evaluated: Vec::new(),
        }
    }

    /// On `\t\t\tp->queue->completed++;`, p is in a register and the
    /// completed field reads through NULL, so only queue is underlined,
    /// at its bytes after the tabs.
    #[test]
    fn only_values_in_memory_are_underlined() {
        let line = "\t\t\tp->queue->completed++;";
        let f = frame(vec![
            expression(
                "p",
                77,
                (3, 4),
                Location::Register {
                    register: Some("r14".into()),
                },
            ),
            expression("p->queue", 77, (6, 11), memory("0x5556f833c068", 8)),
            expression("p->queue->completed", 77, (13, 22), Location::Unreadable),
            expression("p->stopping", 74, (10, 18), memory("0x5556f833c070", 4)),
        ]);
        let found = blameable_on(&f, 77, line);
        assert_eq!(found.len(), 1);
        assert_eq!(&line[found[0].0.clone()], "queue");
        let target = target(found[0].1, 166, 171, 4782).unwrap();
        assert_eq!(target.address, 0x5556_f833_c068);
        assert_eq!(target.size, 8);
    }

    /// A search says what it is doing: restoring with nothing searched,
    /// then its counts, then naming once it found the write.
    #[test]
    fn a_search_says_its_counts_as_it_goes() {
        let target = Target {
            text: "p->queue".into(),
            address: 0x10,
            size: 8,
            class: ValueClass::Pointer,
            pid: 1,
            tid: 1,
            step: 4782,
        };
        let mut blame = Blame::new(target, Ask::Writes(1));
        let mut p = Searching {
            phase: Phase::Restoring,
            before: 4782,
            window: (4130, 4782),
            reached: 4130,
            steps: 0,
            windows: 0,
            writes: 0,
            reads: 0,
            elapsed_ms: 6200,
        };
        blame.progress = Some(p.clone());
        assert_eq!(
            blame.progress_lines(6200),
            [
                "restoring keyframe at step 4,130 \u{b7} 6.2 s",
                "nothing searched yet"
            ]
        );
        assert_eq!(
            blame.chip(6200),
            "blame p->queue \u{b7} restoring keyframe \u{b7} 6 s"
        );
        assert_eq!(blame.searched(), Some((4782, 4782)));
        p.phase = Phase::Searching;
        p.steps = 312;
        p.writes = 2;
        blame.progress = Some(p);
        assert_eq!(
            blame.progress_lines(6200)[1],
            "searched back 312 steps \u{b7} 1 window \u{b7} 2 writes seen \u{b7} 6.2 s"
        );
        assert_eq!(blame.searched(), Some((4130, 4782)));
    }

    /// An underline over part of a colored word splits the word's color
    /// in three, the middle with both; one past the colors stands alone.
    #[test]
    fn an_underline_splits_the_colors_under_it() {
        let base = [(0..6, "kw"), (8..10, "num")];
        let over = [(2..4, "u"), (11..13, "u")];
        let merged = overlay(&base, &over, |b, o| if *b == "kw" { "kw+u" } else { o });
        assert_eq!(
            merged,
            [
                (0..2, "kw"),
                (2..4, "kw+u"),
                (4..6, "kw"),
                (8..10, "num"),
                (11..13, "u"),
            ]
        );
    }
}
