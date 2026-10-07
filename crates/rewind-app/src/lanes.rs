//! The Threads tab's lanes: which thread held the CPU at each step of a
//! window of this run and, while a run is compared, of the same window of
//! the compared run, the two lined up on a step each window is centered on.
//!
//! The engine finds the threads by replaying the window one step at a time
//! (`rewind threads --json`); this module groups its slices into a row per
//! thread, places steps on the window's axis, and finds the first step at
//! which the two runs had different threads on the CPU.

use std::collections::BTreeMap;

use serde::Deserialize;

/// What held the CPU, as `rewind threads --json` says it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum On {
    /// A thread of a process with memory of its own.
    Thread,
    /// A thread without memory: one of the kernel's own, or a process's
    /// thread exiting, past letting its memory go.
    Kernel,
    /// The idle task.
    Idle,
}

/// The steps `from..=to` one task held the CPU through.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Slice {
    pub from: u64,
    pub to: u64,
    pub on: On,
    pub pid: Option<u32>,
    pub tid: Option<u32>,
    pub name: String,
}

/// The row a slice is drawn in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Lane {
    /// A thread of a process.
    Thread { pid: u32, tid: u32 },
    /// The kernel's own threads, together.
    Kernel,
    /// Nothing ready to run.
    Idle,
}

/// One row of the tab: a lane's label and its slices in each run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub lane: Lane,
    pub label: String,
    pub here: Vec<Slice>,
    pub there: Vec<Slice>,
}

/// Which run a slice is from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// The run on screen.
    Here,
    /// The compared run.
    There,
}

/// What the kernel's threads' row is called.
const KERNEL_LABEL: &str = "kernel threads";

/// What the idle row is called.
const IDLE_LABEL: &str = "idle";

/// The rows the slices of this run, `here`, and of the compared run,
/// `there`, make: a row per thread of a process, by process then thread,
/// then one for the kernel's threads and one for idle. A thread's own row
/// also takes the slices where the kernel reports it without memory, which
/// is the thread exiting.
pub fn rows(here: &[Slice], there: &[Slice]) -> Vec<Row> {
    // The threads of processes in either run, by tid, with the name each
    // first had.
    let mut threads: BTreeMap<u32, (u32, String)> = BTreeMap::new();
    for slice in here.iter().chain(there) {
        if let (On::Thread, Some(pid), Some(tid)) = (slice.on, slice.pid, slice.tid) {
            threads.entry(tid).or_insert((pid, slice.name.clone()));
        }
    }
    let lane_of = |slice: &Slice| match (slice.on, slice.tid) {
        (On::Idle, _) => Lane::Idle,
        (_, Some(tid)) if threads.contains_key(&tid) => Lane::Thread {
            pid: threads[&tid].0,
            tid,
        },
        _ => Lane::Kernel,
    };

    // Each run's slices, into their lane's row; BTreeMap keeps the lanes
    // in Lane's order.
    let label = |lane: Lane| match lane {
        Lane::Thread { pid, tid } => format!("{pid}/{tid} {}", threads[&tid].1),
        Lane::Kernel => KERNEL_LABEL.to_string(),
        Lane::Idle => IDLE_LABEL.to_string(),
    };
    let mut rows: BTreeMap<Lane, Row> = BTreeMap::new();
    for (slice, side) in here
        .iter()
        .map(|s| (s, Side::Here))
        .chain(there.iter().map(|s| (s, Side::There)))
    {
        let lane = lane_of(slice);
        let row = rows.entry(lane).or_insert_with(|| Row {
            lane,
            label: label(lane),
            here: Vec::new(),
            there: Vec::new(),
        });
        match side {
            Side::Here => row.here.push(slice.clone()),
            Side::There => row.there.push(slice.clone()),
        }
    }
    rows.into_values().collect()
}

/// The steps a lane shows: `half` steps either side of `center`. Each run
/// has a center of its own, and the same offset from it lines up across
/// the two.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub center: u64,
    pub half: u64,
}

impl Window {
    /// The window's first step, which is never before step 0.
    pub fn from(&self) -> u64 {
        self.center.saturating_sub(self.half)
    }

    /// The window's last step.
    pub fn to(&self) -> u64 {
        self.center + self.half
    }

    /// How far along the window `step` is, from 0 at the left edge of its
    /// first offset to 1 at the right edge of its last; None outside it.
    pub fn at(&self, step: u64) -> Option<f32> {
        let offset = step as i64 - self.center as i64 + self.half as i64;
        if offset < 0 || offset > 2 * self.half as i64 {
            return None;
        }
        Some(offset as f32 * self.step_width())
    }

    /// How much of the window's width one step takes.
    pub fn step_width(&self) -> f32 {
        1.0 / (2 * self.half + 1) as f32
    }

    /// This window's step at `offset` steps from its center, when there is
    /// one.
    pub fn step_at(&self, offset: i64) -> Option<u64> {
        self.center.checked_add_signed(offset)
    }
}

/// The lane holding the CPU at `step`, among `slices`.
pub fn lane_at(slices: &[Slice], step: u64) -> Option<Lane> {
    let slice = slices.iter().find(|s| s.from <= step && step <= s.to)?;
    Some(match (slice.on, slice.pid, slice.tid) {
        (On::Thread, Some(pid), Some(tid)) => Lane::Thread { pid, tid },
        (On::Idle, _, _) => Lane::Idle,
        _ => Lane::Kernel,
    })
}

/// The first step of this run's window, `here`, at which the compared
/// run's window, `there`, had a different thread on the CPU at the same
/// offset from its center. Offsets where either run has no slice, past
/// its end, are passed over.
pub fn first_switch(here: (&[Slice], Window), there: (&[Slice], Window)) -> Option<u64> {
    let (here, w_here) = here;
    let (there, w_there) = there;
    let half = w_here.half.min(w_there.half) as i64;
    for offset in -half..=half {
        let (Some(a), Some(b)) = (w_here.step_at(offset), w_there.step_at(offset)) else {
            continue;
        };
        let (Some(mine), Some(theirs)) = (lane_at(here, a), lane_at(there, b)) else {
            continue;
        };
        if mine != theirs {
            return Some(a);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    // The lanes from made-up slices of two runs of a process 40 with
    // threads 40, 41 and 42, the second run's steps 100 later than the
    // first's.
    use super::*;

    fn thread(from: u64, to: u64, tid: u32) -> Slice {
        Slice {
            from,
            to,
            on: On::Thread,
            pid: Some(40),
            tid: Some(tid),
            name: "pool".into(),
        }
    }

    fn kernel(from: u64, to: u64, tid: u32, name: &str) -> Slice {
        Slice {
            from,
            to,
            on: On::Kernel,
            pid: None,
            tid: Some(tid),
            name: name.into(),
        }
    }

    fn idle(from: u64, to: u64) -> Slice {
        Slice {
            from,
            to,
            on: On::Idle,
            pid: None,
            tid: None,
            name: "swapper/0".into(),
        }
    }

    /// A row per thread of a process, ordered by pid and tid, then the
    /// kernel's threads and idle; a thread exiting, which the kernel
    /// reports without memory under its own tid, stays in its own row, in
    /// either run.
    #[test]
    fn slices_group_into_a_row_per_thread() {
        let here = vec![
            thread(0, 9, 42),
            kernel(10, 10, 11, "ksoftirqd/0"),
            thread(11, 19, 40),
            kernel(20, 20, 40, "pool"),
            idle(21, 30),
        ];
        let there = vec![thread(100, 110, 41), kernel(111, 111, 41, "pool")];
        let rows = rows(&here, &there);
        let lanes: Vec<Lane> = rows.iter().map(|r| r.lane).collect();
        assert_eq!(
            lanes,
            vec![
                Lane::Thread { pid: 40, tid: 40 },
                Lane::Thread { pid: 40, tid: 41 },
                Lane::Thread { pid: 40, tid: 42 },
                Lane::Kernel,
                Lane::Idle,
            ]
        );
        assert_eq!(rows[0].label, "40/40 pool");
        assert_eq!(
            rows[0].here,
            vec![thread(11, 19, 40), kernel(20, 20, 40, "pool")]
        );
        assert_eq!(rows[1].here, vec![]);
        assert_eq!(
            rows[1].there,
            vec![thread(100, 110, 41), kernel(111, 111, 41, "pool")]
        );
        assert_eq!(rows[3].label, KERNEL_LABEL);
        assert_eq!(rows[3].here, vec![kernel(10, 10, 11, "ksoftirqd/0")]);
        assert_eq!(rows[4].label, IDLE_LABEL);
    }

    /// A step's place runs from its offset's left edge: the first step of
    /// a window at 0, its center in the middle; steps outside it have
    /// none. A window near step 0 keeps its width, with no steps before 0.
    #[test]
    fn a_step_is_placed_by_its_offset_from_the_center() {
        let w = Window {
            center: 50,
            half: 2,
        };
        assert_eq!((w.from(), w.to()), (48, 52));
        assert_eq!(w.at(48), Some(0.0));
        assert_eq!(w.at(50), Some(0.4));
        assert_eq!(w.at(52), Some(0.8));
        assert_eq!(w.at(47), None);
        assert_eq!(w.at(53), None);

        let early = Window { center: 1, half: 2 };
        assert_eq!(early.from(), 0);
        assert_eq!(early.at(0), Some(0.2));
        assert_eq!(early.step_at(-2), None);
        assert_eq!(early.step_at(1), Some(2));
    }

    /// The first switch is the first offset where the two runs had
    /// different threads on the CPU, as a step of this run, wherever each
    /// run's window is centered; runs that never differ have none.
    #[test]
    fn the_first_switch_is_where_the_threads_on_the_cpu_differ() {
        let here = vec![thread(0, 9, 41), thread(10, 20, 42)];
        let there = vec![thread(100, 114, 41), thread(115, 120, 42)];
        let w_here = Window {
            center: 10,
            half: 10,
        };
        let w_there = Window {
            center: 110,
            half: 10,
        };
        assert_eq!(first_switch((&here, w_here), (&there, w_there)), Some(10));
        assert_eq!(first_switch((&here, w_here), (&here, w_here)), None);
        assert_eq!(lane_at(&here, 12), Some(Lane::Thread { pid: 40, tid: 42 }));
        assert_eq!(lane_at(&here, 21), None);
    }
}
