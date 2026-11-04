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
pub use rewind_trace::located::{Expression, ValueClass};
pub use rewind_trace::searching::{Phase, Searching};

use crate::describe::thousands;
use crate::engine::EngineError;
use crate::source::Frame;

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

    /// The chip's words in the app bar.
    pub fn chip(&self) -> String {
        let text = &self.target.text;
        match (&self.answer, &self.progress) {
            (Some(Ok(blamed)), _) => match blamed.last_write() {
                Some(write) => format!("blame {text} \u{b7} step {}", thousands(write.step)),
                None => format!("blame {text} \u{b7} no write found"),
            },
            (Some(Err(_)), _) => format!("blame {text} \u{b7} failed"),
            (None, Some(p)) => {
                let seconds = p.elapsed_ms / 1000;
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

    /// The popover's lines while the engine searches: what it is doing,
    /// then its counts.
    pub fn progress_lines(&self) -> Vec<String> {
        let Some(p) = &self.progress else {
            return vec!["forking the run".to_string()];
        };
        let seconds = format!("{:.1} s", p.elapsed_ms as f64 / 1000.0);
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

/// The value a click on `expression` in `frame`'s thread at `step` asks
/// about.
pub fn target(expression: &Expression, pid: u32, tid: u32, step: u64) -> Option<Target> {
    let (address, size) = expression.location.watchable()?;
    let rewind_trace::located::Location::Memory { class, .. } = &expression.location else {
        return None;
    };
    Some(Target {
        text: expression.text.clone(),
        address,
        size,
        class: *class,
        pid,
        tid,
        step,
    })
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
    use rewind_trace::located::Location;

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
            blame.progress_lines(),
            [
                "restoring keyframe at step 4,130 \u{b7} 6.2 s",
                "nothing searched yet"
            ]
        );
        assert_eq!(
            blame.chip(),
            "blame p->queue \u{b7} restoring keyframe \u{b7} 6 s"
        );
        assert_eq!(blame.searched(), Some((4782, 4782)));
        p.phase = Phase::Searching;
        p.steps = 312;
        p.writes = 2;
        blame.progress = Some(p);
        assert_eq!(
            blame.progress_lines()[1],
            "searched back 312 steps \u{b7} 1 window \u{b7} 2 writes seen \u{b7} 6.2 s"
        );
        assert_eq!(blame.searched(), Some((4130, 4782)));
    }
}
