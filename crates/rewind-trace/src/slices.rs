//! What held the CPU over a stretch of a run, from the run's context
//! switches: a slice per stretch one task held. The guest kernel records
//! every switch as a step of its own, so the slices of any window are read
//! from the trace, with nothing replayed. `rewind threads` prints them and
//! the app's Threads tab draws them.

use crate::switches::Switch;
use crate::{Event, EventKind};

/// The length of a task's name, NUL included when shorter.
pub const TASK_COMM_LEN: usize = 16;

/// What the CPU was running.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OnTheCpu {
    /// A thread of a process with memory of its own.
    Thread { pid: u32, tid: u32 },
    /// A thread with no memory of its own, and so no program: one of the
    /// kernel's own threads, or a process's thread exiting, past letting
    /// its memory go.
    WithoutMemory { tid: u32, name: String },
    /// The idle task: nothing was ready to run.
    Idle,
}

/// What held the CPU over the steps `from..=to`, and the name the kernel
/// gives that task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slice {
    pub from: u64,
    pub to: u64,
    pub on: OnTheCpu,
    pub name: String,
}

/// A thread's exec: the step, the thread, and the name the kernel gives
/// the task from then on, the program's file name cut to what a task's
/// name holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exec {
    pub step: u64,
    pub pid: u32,
    pub tid: u32,
    pub name: String,
}

impl Exec {
    /// The exec of `tid` of `pid` at `step` of the program at `filename`:
    /// named as the kernel names the task, by the file's base name, cut
    /// to the task name's length.
    pub fn of(step: u64, pid: u32, tid: u32, filename: &str) -> Exec {
        let base = filename.rsplit('/').next().unwrap_or(filename);
        let name = base
            .char_indices()
            .take_while(|(i, c)| i + c.len_utf8() < TASK_COMM_LEN)
            .map(|(_, c)| c)
            .collect();
        Exec {
            step,
            pid,
            tid,
            name,
        }
    }
}

/// What held the CPU over the steps `from..=to`, from the run's context
/// switches, in step order: a task holds the CPU from its switch to the
/// step before the next. The switch at or before `from` says who held it
/// at `from`; steps before the first switch, early in boot, are left out.
/// A switch names the task as it was then; an exec among `execs`, in
/// step order, renames the task from its step on, so a slice is cut
/// there. Slices are clipped to the window.
pub fn held_over(switches: &[Switch], execs: &[Exec], from: u64, to: u64) -> Vec<Slice> {
    // The first switch that matters: the last at or before `from`, or
    // the first inside the window when none is.
    let first = switches
        .partition_point(|s| s.step <= from)
        .saturating_sub(1);
    let mut slices: Vec<Slice> = Vec::new();
    let first_step = switches.get(first).map_or(0, |s| s.step);
    let mut exec_at = execs.partition_point(|e| e.step < first_step);
    for (i, switch) in switches.iter().enumerate().skip(first) {
        if switch.step > to {
            break;
        }
        let next = switches.get(i + 1).map_or(u64::MAX, |s| s.step);
        if next <= from {
            exec_at = execs.partition_point(|e| e.step < next);
            continue;
        }
        let on = if switch.is_idle() {
            OnTheCpu::Idle
        } else if switch.memory {
            OnTheCpu::Thread {
                pid: switch.pid,
                tid: switch.tid,
            }
        } else {
            OnTheCpu::WithoutMemory {
                tid: switch.tid,
                name: switch.name.clone(),
            }
        };

        // The task's execs while it held the CPU cut its slice, each
        // giving the rest its new name.
        let mut name = switch.name.clone();
        let mut start = switch.step;
        while let Some(exec) = execs.get(exec_at)
            && exec.step < next
        {
            exec_at += 1;
            if exec.pid != switch.pid || exec.tid != switch.tid || exec.step < start {
                continue;
            }
            if exec.step > start {
                push_clipped(&mut slices, start, exec.step - 1, &on, &name, from, to);
                start = exec.step;
            }
            name = exec.name.clone();
        }
        push_clipped(
            &mut slices,
            start,
            next.saturating_sub(1),
            &on,
            &name,
            from,
            to,
        );
    }
    slices
}

/// Appends the slice `start..=end` of `on`, clipped to `from..=to`, when
/// any of it lies inside.
fn push_clipped(
    slices: &mut Vec<Slice>,
    start: u64,
    end: u64,
    on: &OnTheCpu,
    name: &str,
    from: u64,
    to: u64,
) {
    if end < from || start > to {
        return;
    }
    slices.push(Slice {
        from: start.max(from),
        to: end.min(to),
        on: on.clone(),
        name: name.to_string(),
    });
}

/// Every exec among `events`, in the order of the events, which is step
/// order: what [`held_over`] renames tasks by.
pub fn execs(events: &[Event]) -> Vec<Exec> {
    events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::Exec { filename, .. } => Some(Exec::of(e.step, e.pid, e.tid, filename)),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod held_over_tests {
    // Slices from switches: a window is cut out of the switches of a run
    // built by hand, and each slice names the task the CPU ran from its
    // switch to the step before the next.
    use super::*;

    fn switch(step: u64, pid: u32, tid: u32, memory: bool, name: &str) -> Switch {
        Switch {
            step,
            pid,
            tid,
            memory,
            name: name.into(),
        }
    }

    fn run() -> Vec<Switch> {
        vec![
            switch(10, 1, 1, true, "init"),
            switch(20, 34, 34, true, "shuffle"),
            switch(25, 34, 35, true, "shuffle"),
            switch(31, 11, 11, false, "ksoftirqd/0"),
            switch(32, 0, 0, false, "swapper/0"),
            switch(40, 34, 34, true, "shuffle"),
        ]
    }

    #[test]
    fn a_task_holds_the_cpu_from_its_switch_to_the_step_before_the_next() {
        let slices = held_over(&run(), &[], 20, 50);
        let summary: Vec<(u64, u64, OnTheCpu)> =
            slices.into_iter().map(|s| (s.from, s.to, s.on)).collect();
        assert_eq!(
            summary,
            vec![
                (20, 24, OnTheCpu::Thread { pid: 34, tid: 34 }),
                (25, 30, OnTheCpu::Thread { pid: 34, tid: 35 }),
                (
                    31,
                    31,
                    OnTheCpu::WithoutMemory {
                        tid: 11,
                        name: "ksoftirqd/0".into()
                    }
                ),
                (32, 39, OnTheCpu::Idle),
                (40, 50, OnTheCpu::Thread { pid: 34, tid: 34 }),
            ]
        );
    }

    #[test]
    fn a_window_starting_between_switches_starts_with_the_task_then_on_the_cpu() {
        let slices = held_over(&run(), &[], 27, 33);
        assert_eq!(slices.len(), 3);
        assert_eq!((slices[0].from, slices[0].to), (27, 30));
        assert_eq!(slices[0].on, OnTheCpu::Thread { pid: 34, tid: 35 });
        assert_eq!((slices[2].from, slices[2].to), (32, 33));
        assert_eq!(slices[2].on, OnTheCpu::Idle);
    }

    #[test]
    fn steps_before_the_first_switch_belong_to_nobody() {
        let slices = held_over(&run(), &[], 0, 12);
        assert_eq!(slices.len(), 1);
        assert_eq!((slices[0].from, slices[0].to), (10, 12));
        assert_eq!(slices[0].name, "init");
        assert!(held_over(&run(), &[], 0, 5).is_empty());
        assert!(held_over(&[], &[], 0, 100).is_empty());
    }

    /// An exec renames the task from its step on: the slice is cut there,
    /// the part before keeping the old name; one at the slice's first step
    /// renames the whole slice, and one by another thread cuts nothing.
    #[test]
    fn an_exec_cuts_the_slice_and_renames_the_rest() {
        let execs = vec![
            Exec::of(22, 34, 34, "/build/scriptGgRKWz.sh"),
            Exec::of(27, 34, 36, "/bin/other"),
            Exec::of(40, 34, 34, "/bin/sh"),
        ];
        let slices = held_over(&run(), &execs, 20, 50);
        let summary: Vec<(u64, u64, String)> =
            slices.into_iter().map(|s| (s.from, s.to, s.name)).collect();
        assert_eq!(
            summary,
            vec![
                (20, 21, "shuffle".into()),
                (22, 24, "scriptGgRKWz.sh".into()),
                (25, 30, "shuffle".into()),
                (31, 31, "ksoftirqd/0".into()),
                (32, 39, "swapper/0".into()),
                (40, 50, "sh".into()),
            ]
        );
        assert_eq!(
            Exec::of(1, 2, 2, "/nix/store/x-coreutils/bin/mkdir").name,
            "mkdir"
        );
        assert_eq!(
            Exec::of(1, 2, 2, "/build/a-long-program-name.sh").name,
            "a-long-program-"
        );
    }

    #[test]
    fn a_window_of_one_step_is_the_task_on_the_cpu_at_it() {
        let slices = held_over(&run(), &[], 30, 30);
        assert_eq!(slices.len(), 1);
        assert_eq!((slices[0].from, slices[0].to), (30, 30));
        assert_eq!(slices[0].on, OnTheCpu::Thread { pid: 34, tid: 35 });
    }
}
