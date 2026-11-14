//! A run as a Perfetto trace, for ui.perfetto.dev and every tool that
//! reads Perfetto's format.
//!
//! The guest's context switches and wakes are what Linux's scheduler
//! tracepoints say, so they are written as ftrace `sched_switch` and
//! `sched_waking` events on CPU 0, with the forks, execs and exits, and
//! Perfetto draws its own thread state tracks, wakeup arrows and process
//! tree from them. Two tracks of `rewind time`'s own go beside them: the
//! longest chain of work, on the real timeline, and the Nix phases.
//! Times are the VM's clock in nanoseconds.
//!
//! The protobuf is written by hand: the handful of messages and fields
//! below, numbered as in Perfetto's protos/perfetto/trace.

use std::collections::HashMap;

use crate::critical::PieceKind;
use crate::time_report::Timing;
use crate::{EventKind, Trace};

/// Field numbers, from protos/perfetto/trace.
mod field {
    pub(super) const TRACE_PACKET: u32 = 1;

    pub(super) const PACKET_FTRACE_EVENTS: u32 = 1;
    pub(super) const PACKET_PROCESS_TREE: u32 = 2;
    pub(super) const PACKET_TIMESTAMP: u32 = 8;
    pub(super) const PACKET_SEQUENCE_ID: u32 = 10;
    pub(super) const PACKET_TRACK_EVENT: u32 = 11;
    pub(super) const PACKET_SEQUENCE_FLAGS: u32 = 13;
    pub(super) const PACKET_TRACK_DESCRIPTOR: u32 = 60;

    pub(super) const BUNDLE_CPU: u32 = 1;
    pub(super) const BUNDLE_EVENT: u32 = 2;

    pub(super) const EVENT_TIMESTAMP: u32 = 1;
    pub(super) const EVENT_PID: u32 = 2;
    pub(super) const EVENT_SCHED_SWITCH: u32 = 4;
    pub(super) const EVENT_SCHED_WAKING: u32 = 20;
    pub(super) const EVENT_PROCESS_EXEC: u32 = 237;
    pub(super) const EVENT_PROCESS_EXIT: u32 = 238;
    pub(super) const EVENT_PROCESS_FORK: u32 = 239;

    pub(super) const SWITCH_PREV_COMM: u32 = 1;
    pub(super) const SWITCH_PREV_PID: u32 = 2;
    pub(super) const SWITCH_PREV_PRIO: u32 = 3;
    pub(super) const SWITCH_PREV_STATE: u32 = 4;
    pub(super) const SWITCH_NEXT_COMM: u32 = 5;
    pub(super) const SWITCH_NEXT_PID: u32 = 6;
    pub(super) const SWITCH_NEXT_PRIO: u32 = 7;

    pub(super) const WAKING_COMM: u32 = 1;
    pub(super) const WAKING_PID: u32 = 2;
    pub(super) const WAKING_PRIO: u32 = 3;
    pub(super) const WAKING_SUCCESS: u32 = 4;
    pub(super) const WAKING_TARGET_CPU: u32 = 5;

    pub(super) const FORK_PARENT_COMM: u32 = 1;
    pub(super) const FORK_PARENT_PID: u32 = 2;
    pub(super) const FORK_CHILD_COMM: u32 = 3;
    pub(super) const FORK_CHILD_PID: u32 = 4;

    pub(super) const EXEC_FILENAME: u32 = 1;
    pub(super) const EXEC_PID: u32 = 2;
    pub(super) const EXEC_OLD_PID: u32 = 3;

    pub(super) const EXIT_COMM: u32 = 1;
    pub(super) const EXIT_PID: u32 = 2;
    pub(super) const EXIT_TGID: u32 = 3;

    pub(super) const TREE_PROCESSES: u32 = 1;
    pub(super) const TREE_THREADS: u32 = 2;
    pub(super) const PROCESS_PID: u32 = 1;
    pub(super) const PROCESS_PPID: u32 = 2;
    pub(super) const PROCESS_CMDLINE: u32 = 3;
    pub(super) const THREAD_TID: u32 = 1;
    pub(super) const THREAD_NAME: u32 = 2;
    pub(super) const THREAD_TGID: u32 = 3;

    pub(super) const TRACK_UUID: u32 = 1;
    pub(super) const TRACK_NAME: u32 = 2;
    pub(super) const TRACK_EVENT_TYPE: u32 = 9;
    pub(super) const TRACK_EVENT_TRACK_UUID: u32 = 11;
    pub(super) const TRACK_EVENT_NAME: u32 = 23;
}

/// Protobuf wire types.
const VARINT: u32 = 0;
const LEN: u32 = 2;

/// A priority every task is given; the guest records none.
const PRIO: u64 = 120;

/// The kernel's state bits for the task that left the CPU: runnable, or
/// blocked.
const STATE_RUNNABLE: u64 = 0;
const STATE_SLEEPING: u64 = 1;

/// TrackEvent types.
const SLICE_BEGIN: u64 = 1;
const SLICE_END: u64 = 2;

/// The sequence the track events are written on, and its flag for a
/// first packet.
const SEQUENCE: u64 = 1;
const SEQ_INCREMENTAL_STATE_CLEARED: u64 = 1;

/// The tracks of `rewind time`'s own.
const CHAIN_TRACK: u64 = 1;
const PHASE_TRACK: u64 = 2;

/// How many ftrace events go in one bundle.
const BUNDLE_EVENTS: usize = 4096;

/// A protobuf message being written.
#[derive(Default)]
struct Msg(Vec<u8>);

impl Msg {
    fn key(&mut self, field: u32, wire: u32) {
        varint(&mut self.0, u64::from(field << 3 | wire));
    }

    fn uint(&mut self, field: u32, value: u64) -> &mut Msg {
        self.key(field, VARINT);
        varint(&mut self.0, value);
        self
    }

    /// An int32 field: negative values take ten bytes, as protobuf says.
    fn int(&mut self, field: u32, value: i64) -> &mut Msg {
        self.uint(field, value as u64)
    }

    fn bytes(&mut self, field: u32, value: &[u8]) -> &mut Msg {
        self.key(field, LEN);
        varint(&mut self.0, value.len() as u64);
        self.0.extend_from_slice(value);
        self
    }

    fn str(&mut self, field: u32, value: &str) -> &mut Msg {
        self.bytes(field, value.as_bytes())
    }

    fn msg(&mut self, field: u32, value: &Msg) -> &mut Msg {
        self.bytes(field, &value.0)
    }
}

fn varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// One scheduler event, before it is sorted into the stream.
struct Ftrace {
    ns: u64,
    /// The task the event happened on.
    pid: u32,
    field: u32,
    body: Msg,
}

/// The run as a Perfetto trace's bytes.
pub fn trace(trace: &Trace, timing: &Timing) -> Vec<u8> {
    let graph = &timing.graph;
    let mut out = Msg::default();

    // The process tree: each process's command line and each thread's
    // name, as the trace last knew them.
    let mut tree = Msg::default();
    let procs = trace.processes();
    for p in &procs {
        let mut m = Msg::default();
        m.int(field::PROCESS_PID, i64::from(p.pid))
            .int(field::PROCESS_PPID, i64::from(p.parent));
        for word in &p.argv {
            m.str(field::PROCESS_CMDLINE, word);
        }
        tree.msg(field::TREE_PROCESSES, &m);
    }
    let mut names: HashMap<u32, (u32, String)> = HashMap::new();
    for s in &trace.switches {
        names.insert(s.tid, (s.pid, s.name.clone()));
    }
    for (tid, (tgid, name)) in &names {
        let mut m = Msg::default();
        m.int(field::THREAD_TID, i64::from(*tid))
            .str(field::THREAD_NAME, name)
            .int(field::THREAD_TGID, i64::from(*tgid));
        tree.msg(field::TREE_THREADS, &m);
    }
    let mut packet = Msg::default();
    packet
        .uint(field::PACKET_TIMESTAMP, 0)
        .msg(field::PACKET_PROCESS_TREE, &tree);
    out.msg(field::TRACE_PACKET, &packet);

    // The scheduler events: switches, wakes, forks, execs and exits.
    let mut events: Vec<Ftrace> = Vec::new();
    let mut comm: HashMap<u32, String> = HashMap::new();
    let mut prev: Option<(u32, String)> = None;
    for s in &trace.switches {
        let (prev_pid, prev_comm) = prev.clone().unwrap_or((0, "swapper/0".to_string()));
        let mut body = Msg::default();
        body.str(field::SWITCH_PREV_COMM, &prev_comm)
            .int(field::SWITCH_PREV_PID, i64::from(prev_pid))
            .uint(field::SWITCH_PREV_PRIO, PRIO)
            .uint(
                field::SWITCH_PREV_STATE,
                if s.prev_runnable {
                    STATE_RUNNABLE
                } else {
                    STATE_SLEEPING
                },
            )
            .str(field::SWITCH_NEXT_COMM, &s.name)
            .int(field::SWITCH_NEXT_PID, i64::from(s.tid))
            .uint(field::SWITCH_NEXT_PRIO, PRIO);
        events.push(Ftrace {
            ns: s.ns,
            pid: prev_pid,
            field: field::EVENT_SCHED_SWITCH,
            body,
        });
        comm.insert(s.tid, s.name.clone());
        prev = Some((s.tid, s.name.clone()));
    }
    for w in &trace.wakes {
        let mut body = Msg::default();
        body.str(
            field::WAKING_COMM,
            comm.get(&w.tid).map_or("", String::as_str),
        )
        .int(field::WAKING_PID, i64::from(w.tid))
        .uint(field::WAKING_PRIO, PRIO)
        .uint(field::WAKING_SUCCESS, 1)
        .uint(field::WAKING_TARGET_CPU, 0);
        events.push(Ftrace {
            ns: w.ns,
            pid: w.waker,
            field: field::EVENT_SCHED_WAKING,
            body,
        });
    }
    for e in &trace.events {
        let ns = graph.time_at(e.step);
        let name = |tid: u32| comm.get(&tid).cloned().unwrap_or_default();
        let (event_field, body) = match &e.kind {
            EventKind::Fork { child, .. } => {
                let mut b = Msg::default();
                b.str(field::FORK_PARENT_COMM, &name(e.tid))
                    .int(field::FORK_PARENT_PID, i64::from(e.tid))
                    .str(field::FORK_CHILD_COMM, &name(e.tid))
                    .int(field::FORK_CHILD_PID, i64::from(*child));
                (field::EVENT_PROCESS_FORK, b)
            }
            EventKind::Exec {
                filename, old_pid, ..
            } => {
                let mut b = Msg::default();
                b.str(field::EXEC_FILENAME, filename)
                    .int(field::EXEC_PID, i64::from(e.tid))
                    .int(field::EXEC_OLD_PID, i64::from(*old_pid));
                (field::EVENT_PROCESS_EXEC, b)
            }
            EventKind::Exit { comm, .. } => {
                let mut b = Msg::default();
                b.str(field::EXIT_COMM, comm)
                    .int(field::EXIT_PID, i64::from(e.tid))
                    .int(field::EXIT_TGID, i64::from(e.pid));
                (field::EVENT_PROCESS_EXIT, b)
            }
            _ => continue,
        };
        events.push(Ftrace {
            ns,
            pid: e.tid,
            field: event_field,
            body,
        });
    }
    events.sort_by_key(|e| e.ns);
    for chunk in events.chunks(BUNDLE_EVENTS) {
        let mut bundle = Msg::default();
        bundle.uint(field::BUNDLE_CPU, 0);
        for e in chunk {
            let mut m = Msg::default();
            m.uint(field::EVENT_TIMESTAMP, e.ns)
                .uint(field::EVENT_PID, u64::from(e.pid))
                .msg(e.field, &e.body);
            bundle.msg(field::BUNDLE_EVENT, &m);
        }
        let mut packet = Msg::default();
        packet.msg(field::PACKET_FTRACE_EVENTS, &bundle);
        out.msg(field::TRACE_PACKET, &packet);
    }

    // rewind time's tracks: their descriptors, then their slices.
    let mut first = true;
    for (uuid, name) in [
        (CHAIN_TRACK, "rewind time: longest chain of work"),
        (PHASE_TRACK, "Nix phases"),
    ] {
        let mut d = Msg::default();
        d.uint(field::TRACK_UUID, uuid).str(field::TRACK_NAME, name);
        let mut packet = Msg::default();
        packet
            .uint(field::PACKET_TIMESTAMP, 0)
            .msg(field::PACKET_TRACK_DESCRIPTOR, &d)
            .uint(field::PACKET_SEQUENCE_ID, SEQUENCE);
        if first {
            packet.uint(field::PACKET_SEQUENCE_FLAGS, SEQ_INCREMENTAL_STATE_CLEARED);
            first = false;
        }
        out.msg(field::TRACE_PACKET, &packet);
    }
    let mut slices: Vec<(u64, u64, u64, String)> = Vec::new();
    for (from, to, label) in chain_on_the_timeline(timing) {
        slices.push((CHAIN_TRACK, from, to, label));
    }
    let report = timing.report();
    let end = graph.job_end.1;
    for (i, p) in report.phases.iter().enumerate() {
        let from = graph.time_at(p.step);
        let to = report
            .phases
            .get(i + 1)
            .map_or(end, |next| graph.time_at(next.step));
        slices.push((PHASE_TRACK, from, to, p.name.clone()));
    }
    for (track, from, to, label) in slices {
        let mut begin = Msg::default();
        begin
            .uint(field::TRACK_EVENT_TYPE, SLICE_BEGIN)
            .uint(field::TRACK_EVENT_TRACK_UUID, track)
            .str(field::TRACK_EVENT_NAME, &label);
        let mut end_event = Msg::default();
        end_event
            .uint(field::TRACK_EVENT_TYPE, SLICE_END)
            .uint(field::TRACK_EVENT_TRACK_UUID, track);
        for (ns, event) in [(from, &begin), (to.max(from), &end_event)] {
            let mut packet = Msg::default();
            packet
                .uint(field::PACKET_TIMESTAMP, ns)
                .msg(field::PACKET_TRACK_EVENT, event)
                .uint(field::PACKET_SEQUENCE_ID, SEQUENCE);
            out.msg(field::TRACE_PACKET, &packet);
        }
    }
    out.0
}

/// The chain's pieces where they happened on the one CPU: computing from
/// where it began in its run, a sleep from where its thread fell asleep.
/// Pieces of one process in one state merge only where one ends as the
/// next begins, so a slice never spans work another thread did between.
fn chain_on_the_timeline(timing: &Timing) -> Vec<(u64, u64, String)> {
    let mut out: Vec<(u64, u64, String)> = Vec::new();
    let mut last: Option<(Option<crate::Life>, bool)> = None;
    for piece in &timing.chain.pieces {
        let sleeping = matches!(piece.kind, PieceKind::Sleep { .. });
        let (from, to) = (piece.from_ns, piece.from_ns + piece.ns);
        let life = timing.lives[piece.run];
        if last == Some((life, sleeping))
            && let Some(prev) = out.last_mut()
            && prev.1 == from
        {
            prev.1 = to;
            continue;
        }
        let who = life.map_or_else(|| "the kernel".to_string(), |l| timing.name(l));
        let label = if sleeping {
            format!("sleeps: {who}")
        } else {
            who
        };
        out.push((from, to, label));
        last = Some((life, sleeping));
    }
    out
}

#[cfg(test)]
mod tests {
    // The wire encoding: varints and fields laid out as protobuf reads
    // them.
    use super::*;

    #[test]
    fn varints_and_fields_encode_as_protobuf_does() {
        let mut out = Vec::new();
        varint(&mut out, 300);
        assert_eq!(out, vec![0xac, 0x02]);
        let mut m = Msg::default();
        m.uint(1, 150).str(2, "hi");
        assert_eq!(m.0, vec![0x08, 0x96, 0x01, 0x12, 0x02, b'h', b'i']);
    }

    /// The chain's slices of `trace`, run to the clock at `end_ns`, as
    /// where each starts and ends.
    fn slices(trace: &Trace, end_ns: u64) -> Vec<(u64, u64)> {
        let timing = Timing::new(trace, &crate::io_records::IoLog::default(), end_ns).unwrap();
        chain_on_the_timeline(&timing)
            .into_iter()
            .map(|(from, to, _)| (from, to))
            .collect()
    }

    #[test]
    fn a_preempted_process_is_two_slices_with_the_other_work_between() {
        // Init computes from step 0 to 10, is preempted while process 2
        // runs to 40, and computes again to 60; the clock is one past the
        // step and the job runs from step 1 to 59. Init's two stretches
        // are two slices, 2 to 11 and 41 to 60, not one across process 2.
        use crate::critical::tests::{job_mark, switch};
        let trace = Trace {
            events: vec![job_mark(1, "rewind-start"), job_mark(59, "rewind-exit 0")],
            switches: vec![
                switch(0, 1, 1, None),
                switch(10, 2, 2, None),
                switch(40, 1, 1, Some(60)),
                switch(60, 0, 0, Some(60)),
            ],
            wakes: vec![],
        };
        assert_eq!(slices(&trace, 61), vec![(2, 11), (41, 60)]);
    }

    #[test]
    fn a_sleep_is_where_it_was_and_not_where_its_thread_next_ran() {
        // Init computes from step 0 to 10 and sleeps in clock_nanosleep;
        // a timer wakes it at step 20, but process 2 holds the CPU until
        // 50, and init computes from 50 to 60. The sleep is 11 to 21, from
        // where init fell asleep to the timer, not the 10 before step 50.
        use crate::critical::tests::{job_mark, switch};
        let trace = Trace {
            events: vec![job_mark(1, "rewind-start"), job_mark(59, "rewind-exit 0")],
            switches: vec![
                switch(0, 1, 1, None),
                switch(10, 0, 0, Some(230)),
                switch(20, 2, 2, None),
                switch(50, 1, 1, None),
                switch(60, 0, 0, Some(60)),
            ],
            wakes: vec![crate::switches::Wake {
                step: 20,
                ns: 21,
                tid: 1,
                waker: 0,
                how: crate::switches::WakeHow::Hardirq,
            }],
        };
        assert_eq!(slices(&trace, 61), vec![(2, 11), (11, 21), (51, 60)]);
    }
}
