//! Context switches: which task the CPU ran from each step on.
//!
//! The guest kernel writes a switch record at every context switch, so a
//! switch is an exit and a step of its own, and the record names the task
//! the CPU runs from that step: the thread of a process, one of the
//! kernel's own threads, or the idle task. The monitor does not store the
//! guest's record. It encodes each switch in a few bytes, against a table
//! of the tasks named so far, and stores many of them in one trace record
//! ([`Encoder`]), since a run makes a switch at most steps and the guest's
//! 36 bytes each would be most of a long trace. A batch is written at the
//! first step of the next window of [`WINDOW`] steps, when it is full, or
//! when the run ends, stamped with the step it is written at, so a trace
//! stays in step order and a batch holds only switches at or before its
//! step.
//!
//! The records are not events: they say nothing a program did, so
//! comparisons of what two runs did leave them out, and `rewind events`
//! does not list them. `rewind threads` and the app's thread lanes are
//! read from them. A machine restored at a keyframe goes on encoding from
//! the trace's state at that step ([`Encoder::resume`]), so its batches
//! come out as the run's did.

use std::collections::HashMap;

use crate::event::{DecodeError, HEADER_LEN, RECORD_MAX};

/// The record kind the guest numbers a switch, its REWIND_EV_SWITCH.
const KIND: u16 = 14;

/// The record kind the monitor stores a batch of encoded switches under,
/// which the guest never sends.
const BATCH_KIND: u16 = 15;

/// The format of a batch's payload, in the record's aux field.
pub const FORMAT: u32 = 1;

/// The steps a batch covers at most: a batch is written at the first step
/// of the window after its first switch's, so a reader of a trace still
/// being written is at most a window behind.
pub const WINDOW: u64 = 1 << 12;

/// The most payload a batch holds: a record's size less its header.
const PAYLOAD_MAX: usize = RECORD_MAX - HEADER_LEN;

/// The bit in an encoded task's flags for a task with memory of its own.
const TASK_MEMORY: u8 = 1;

/// Varints: seven bits a byte, low bits first, the high bit saying more
/// bytes follow.
const VARINT_BITS: u32 = 7;
const VARINT_LOW: u64 = 0x7f;
const VARINT_MORE: u8 = 0x80;

/// The flag on a switch record for a task with no memory of its own: one of
/// the kernel's threads, or a process's thread exiting, past letting its
/// memory go. The guest's REWIND_FLAG_NO_MEMORY.
const FLAG_NO_MEMORY: u16 = 1 << 1;

/// The idle task's pid: the CPU had nothing to run.
const IDLE_PID: u32 = 0;

/// One context switch: from `step` on, the CPU ran this task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Switch {
    pub step: u64,
    /// The task's thread group (process) id; 0 for the idle task.
    pub pid: u32,
    /// The task's thread id.
    pub tid: u32,
    /// Whether the task has memory of its own, and so a program.
    pub memory: bool,
    /// The kernel's name for the task, its comm.
    pub name: String,
}

impl Switch {
    /// Whether the switch is to the idle task: nothing was ready to run.
    pub fn is_idle(&self) -> bool {
        self.pid == IDLE_PID
    }
}

/// Whether a trace record is a switch record.
pub fn is_switch(record: &[u8]) -> bool {
    record.len() >= HEADER_LEN && u16::from_le_bytes([record[4], record[5]]) == KIND
}

/// Decodes a switch record as the guest wrote it: the header's pid and tid
/// name the task, its flags say whether the task has memory, and the
/// payload is the task's name.
pub fn decode(step: u64, record: &[u8]) -> Result<Switch, DecodeError> {
    if record.len() < HEADER_LEN {
        return Err(DecodeError::Short(record.len()));
    }
    let u32_at = |at: usize| u32::from_le_bytes(record[at..at + 4].try_into().expect("four bytes"));
    let claimed = u32_at(0) as usize;
    if claimed != record.len() {
        return Err(DecodeError::Length {
            claimed,
            actual: record.len(),
        });
    }
    let flags = u16::from_le_bytes([record[6], record[7]]);
    let name = &record[HEADER_LEN..];
    let end = name.iter().position(|b| *b == 0).unwrap_or(name.len());
    Ok(Switch {
        step,
        pid: u32_at(8),
        tid: u32_at(12),
        memory: flags & FLAG_NO_MEMORY == 0,
        name: String::from_utf8_lossy(&name[..end]).into_owned(),
    })
}

/// Whether a trace record is a batch of encoded switches.
pub fn is_batch(record: &[u8]) -> bool {
    record.len() >= HEADER_LEN && u16::from_le_bytes([record[4], record[5]]) == BATCH_KIND
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum SwitchError {
    #[error("the switches at step {step} end in the middle of an entry")]
    Truncated { step: u64 },
    #[error("the switches at step {step} are in format {format}; this build reads {FORMAT}")]
    Format { step: u64, format: u32 },
    #[error("the switches at step {step} name task {index}, which none before defined")]
    UndefinedTask { step: u64, index: u64 },
    #[error("the switches at step {step} hold a {what} too large for its field")]
    OutOfRange { step: u64, what: &'static str },
    #[error("a switch record at step {step} does not read: {error}")]
    Guest { step: u64, error: DecodeError },
}

/// A task as the table keeps it: what tells one task from another.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Task {
    pid: u32,
    tid: u32,
    memory: bool,
    name: String,
}

impl Task {
    fn of(switch: &Switch) -> Task {
        Task {
            pid: switch.pid,
            tid: switch.tid,
            memory: switch.memory,
            name: switch.name.clone(),
        }
    }
}

/// What an entry is encoded against: the tasks defined so far, by the
/// order they were defined in, and the step of the switch before. An
/// encoder and a decoder that have seen the same switches hold the same.
#[derive(Clone, Debug, Default)]
struct Tables {
    tasks: HashMap<Task, u64>,
    last_step: u64,
}

impl Tables {
    /// Encodes one switch onto `out`, without changing the tables: its step
    /// as the distance from the switch before, then its task's index,
    /// which for a task not yet defined is the next index followed by the
    /// task. Returns the task to define, when the entry defines one, for
    /// [`Tables::define`] once the entry is kept.
    fn entry(&self, switch: &Switch, out: &mut Vec<u8>) -> Option<Task> {
        put(out, switch.step.wrapping_sub(self.last_step));
        let task = Task::of(switch);
        if let Some(index) = self.tasks.get(&task) {
            put(out, *index);
            return None;
        }
        put(out, self.tasks.len() as u64);
        put(out, u64::from(task.pid));
        put(out, u64::from(task.tid));
        out.push(if task.memory { TASK_MEMORY } else { 0 });
        put(out, task.name.len() as u64);
        out.extend_from_slice(task.name.as_bytes());
        Some(task)
    }

    /// Notes that an entry for `switch` was kept, and the task it defined.
    fn define(&mut self, switch: &Switch, defined: Option<Task>) {
        self.last_step = switch.step;
        if let Some(task) = defined {
            let next = self.tasks.len() as u64;
            self.tasks.insert(task, next);
        }
    }

    /// Encodes one switch onto `out` and keeps it.
    fn encode(&mut self, switch: &Switch, out: &mut Vec<u8>) {
        let defined = self.entry(switch, out);
        self.define(switch, defined);
    }
}

/// Turns the guest's switch records into trace records, a batch at a time.
#[derive(Clone, Debug, Default)]
pub struct Encoder {
    tables: Tables,
    /// The entries encoded since the last batch was written.
    pending: Vec<u8>,
    /// The step of the first pending entry's switch, which names the
    /// window the batch is for.
    first: Option<u64>,
}

impl Encoder {
    /// An encoder for a machine that has just booted.
    pub fn new() -> Encoder {
        Encoder::default()
    }

    /// The encoder as it was once the run had made the switches among
    /// `records`, the run's whole trace, up to `step`, the step a machine
    /// is restored at: the tables as the switches up to then left them,
    /// and pending the switches up to then that the run wrote in a batch
    /// after it.
    pub fn resume(records: &[(u64, Vec<u8>)], step: u64) -> Result<Encoder, SwitchError> {
        let mut decoder = Decoder::default();
        let mut encoder = Encoder::new();
        let mut switches = Vec::new();
        for (written_at, record) in records.iter().filter(|(_, r)| is_batch(r)) {
            switches.clear();
            decoder.decode(*written_at, record, &mut switches)?;
            for switch in switches.iter().filter(|s| s.step <= step) {
                if *written_at <= step {
                    // Written by then: in the tables, not pending.
                    let mut written = Vec::new();
                    encoder.tables.encode(switch, &mut written);
                    continue;
                }
                encoder.add_switch(switch);
            }
        }
        Ok(encoder)
    }

    /// The batch to write at `step`, before anything else the step
    /// records, when the pending switches are from an earlier window; None
    /// otherwise.
    pub fn at_step(&mut self, step: u64) -> Option<Vec<u8>> {
        let first = self.first?;
        if first / WINDOW == step / WINDOW {
            return None;
        }
        self.flush()
    }

    /// Takes the guest's switch record made at `step`, and gives the batch
    /// to write first, at this step, when the pending entries had no room
    /// for it.
    pub fn add(&mut self, step: u64, record: &[u8]) -> Result<Option<Vec<u8>>, SwitchError> {
        let switch = decode(step, record).map_err(|error| SwitchError::Guest { step, error })?;
        Ok(self.add_switch(&switch))
    }

    /// The pending batch at the run's end, if any.
    pub fn finish(&mut self) -> Option<Vec<u8>> {
        self.flush()
    }

    fn add_switch(&mut self, switch: &Switch) -> Option<Vec<u8>> {
        // The entry is made before the tables change, so a batch with no
        // room for it is written first and the entry still reads against
        // what the tables hold then.
        let mut entry = Vec::new();
        let defined = self.tables.entry(switch, &mut entry);
        let written = if self.pending.len() + entry.len() > PAYLOAD_MAX {
            self.flush()
        } else {
            None
        };
        self.tables.define(switch, defined);
        self.pending.extend_from_slice(&entry);
        self.first.get_or_insert(switch.step);
        written
    }

    fn flush(&mut self) -> Option<Vec<u8>> {
        self.first.take()?;
        let payload = std::mem::take(&mut self.pending);
        Some(batch_record(&payload))
    }
}

/// A trace record of [`BATCH_KIND`] around `payload`.
fn batch_record(payload: &[u8]) -> Vec<u8> {
    let len = HEADER_LEN + payload.len();
    let mut r = Vec::with_capacity(len);
    r.extend_from_slice(&(len as u32).to_le_bytes());
    r.extend_from_slice(&BATCH_KIND.to_le_bytes());
    // Flags, pid and tid: the entries carry their own tasks.
    r.extend_from_slice(&[0u8; 10]);
    r.extend_from_slice(&FORMAT.to_le_bytes());
    r.extend_from_slice(payload);
    r
}

/// Reads the switches out of a trace's batches, in order.
#[derive(Clone, Debug, Default)]
pub struct Decoder {
    tasks: Vec<Task>,
    last_step: u64,
}

impl Decoder {
    /// The decoder as it stands after the batches among `records` written
    /// at or before `step`: what reads the trace a machine restored at
    /// `step` writes from there on, whose first batch names tasks those
    /// batches defined.
    pub fn resume(records: &[(u64, Vec<u8>)], step: u64) -> Result<Decoder, SwitchError> {
        let mut decoder = Decoder::default();
        let mut switches = Vec::new();
        for (written_at, record) in records.iter().filter(|(s, r)| *s <= step && is_batch(r)) {
            decoder.decode(*written_at, record, &mut switches)?;
        }
        Ok(decoder)
    }

    /// Appends the switches of the batch `record`, written at `step`, to
    /// `out`.
    pub fn decode(
        &mut self,
        step: u64,
        record: &[u8],
        out: &mut Vec<Switch>,
    ) -> Result<(), SwitchError> {
        if record.len() < HEADER_LEN {
            return Err(SwitchError::Truncated { step });
        }
        let format = u32::from_le_bytes(record[16..20].try_into().expect("four bytes"));
        if format != FORMAT {
            return Err(SwitchError::Format { step, format });
        }
        let mut r = Reader {
            bytes: &record[HEADER_LEN..],
            step,
        };
        while !r.bytes.is_empty() {
            self.last_step = self.last_step.wrapping_add(r.varint()?);
            let index = r.varint()?;
            let task = match usize::try_from(index) {
                Ok(i) if i < self.tasks.len() => &self.tasks[i],
                Ok(i) if i == self.tasks.len() => {
                    let pid = r.u32("pid")?;
                    let tid = r.u32("tid")?;
                    let memory = r.byte()? & TASK_MEMORY != 0;
                    let len = r.u32("name length")? as usize;
                    let name = r.bytes(len)?;
                    self.tasks.push(Task {
                        pid,
                        tid,
                        memory,
                        name: String::from_utf8_lossy(name).into_owned(),
                    });
                    &self.tasks[i]
                }
                _ => return Err(SwitchError::UndefinedTask { step, index }),
            };
            out.push(Switch {
                step: self.last_step,
                pid: task.pid,
                tid: task.tid,
                memory: task.memory,
                name: task.name.clone(),
            });
        }
        Ok(())
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    step: u64,
}

impl<'a> Reader<'a> {
    fn byte(&mut self) -> Result<u8, SwitchError> {
        let (&b, rest) = self
            .bytes
            .split_first()
            .ok_or(SwitchError::Truncated { step: self.step })?;
        self.bytes = rest;
        Ok(b)
    }

    fn bytes(&mut self, len: usize) -> Result<&'a [u8], SwitchError> {
        if len > self.bytes.len() {
            return Err(SwitchError::Truncated { step: self.step });
        }
        let (taken, rest) = self.bytes.split_at(len);
        self.bytes = rest;
        Ok(taken)
    }

    fn varint(&mut self) -> Result<u64, SwitchError> {
        let mut value = 0u64;
        let mut shift = 0;
        loop {
            let b = self.byte()?;
            if shift < u64::BITS {
                value |= u64::from(b & !VARINT_MORE) << shift;
            }
            if b & VARINT_MORE == 0 {
                return Ok(value);
            }
            shift += VARINT_BITS;
        }
    }

    /// A varint that holds a u32, `what` it is naming the field when it
    /// holds more.
    fn u32(&mut self, what: &'static str) -> Result<u32, SwitchError> {
        let value = self.varint()?;
        u32::try_from(value).map_err(|_| SwitchError::OutOfRange {
            step: self.step,
            what,
        })
    }
}

fn put(out: &mut Vec<u8>, mut value: u64) {
    while value > VARINT_LOW {
        out.push((value & VARINT_LOW) as u8 | VARINT_MORE);
        value >>= VARINT_BITS;
    }
    out.push(value as u8);
}

#[cfg(test)]
pub(crate) mod tests {
    // Decoding: switch records laid out as the guest lays them out come
    // back as the switches they describe, and other records are not taken
    // for one.
    use super::*;

    /// A switch record as the guest writes it, with `flags` and the task's
    /// name as the payload.
    pub(crate) fn record(flags: u16, pid: u32, tid: u32, name: &[u8]) -> Vec<u8> {
        let mut r = Vec::new();
        r.extend_from_slice(&((HEADER_LEN + name.len()) as u32).to_le_bytes());
        r.extend_from_slice(&KIND.to_le_bytes());
        r.extend_from_slice(&flags.to_le_bytes());
        r.extend_from_slice(&pid.to_le_bytes());
        r.extend_from_slice(&tid.to_le_bytes());
        r.extend_from_slice(&0u32.to_le_bytes());
        r.extend_from_slice(name);
        r
    }

    #[test]
    fn a_switch_to_a_thread_names_its_process_thread_and_program() {
        let switch = decode(40, &record(0, 80, 82, b"shuffle")).unwrap();
        assert_eq!(
            switch,
            Switch {
                step: 40,
                pid: 80,
                tid: 82,
                memory: true,
                name: "shuffle".into(),
            }
        );
        assert!(!switch.is_idle());
    }

    #[test]
    fn a_switch_to_a_kernel_thread_has_no_memory() {
        let switch = decode(41, &record(FLAG_NO_MEMORY, 11, 11, b"ksoftirqd/0")).unwrap();
        assert!(!switch.memory);
        assert_eq!(switch.name, "ksoftirqd/0");
        assert!(!switch.is_idle());
    }

    #[test]
    fn a_switch_to_pid_zero_is_to_the_idle_task() {
        let switch = decode(42, &record(FLAG_NO_MEMORY, 0, 0, b"swapper/0\0junk")).unwrap();
        assert!(switch.is_idle());
        assert_eq!(switch.name, "swapper/0");
    }

    #[test]
    fn only_records_of_the_switch_kind_are_switches() {
        assert!(is_switch(&record(0, 1, 1, b"init")));
        let mut other = record(0, 1, 1, b"init");
        other[4] = 5;
        assert!(!is_switch(&other));
        assert!(!is_switch(&[0; 4]));
    }

    #[test]
    fn a_record_shorter_than_its_header_or_its_claim_is_refused() {
        assert_eq!(decode(1, &[0; 8]), Err(DecodeError::Short(8)));
        let mut lying = record(0, 1, 1, b"init");
        lying[0] = 99;
        assert_eq!(
            decode(1, &lying),
            Err(DecodeError::Length {
                claimed: 99,
                actual: HEADER_LEN + 4,
            })
        );
    }

    /// Runs a sequence of switches through an encoder the way the monitor
    /// does: the window check at each step, the guest record at each
    /// switch, and the end. Each switch is a (step, pid, tid, memory,
    /// name); the steps in between are those without a switch.
    fn run(
        encoder: &mut Encoder,
        switches: &[(u64, u32, u32, bool, &str)],
        from: u64,
        last: u64,
    ) -> Vec<(u64, Vec<u8>)> {
        let mut records = Vec::new();
        for step in from..=last {
            if let Some(batch) = encoder.at_step(step) {
                records.push((step, batch));
            }
            if let Some(&(_, pid, tid, memory, name)) = switches.iter().find(|(s, ..)| *s == step) {
                let flags = if memory { 0 } else { FLAG_NO_MEMORY };
                if let Some(batch) = encoder
                    .add(step, &record(flags, pid, tid, name.as_bytes()))
                    .unwrap()
                {
                    records.push((step, batch));
                }
            }
        }
        if let Some(batch) = encoder.finish() {
            records.push((last, batch));
        }
        records
    }

    fn switches_of(records: &[(u64, Vec<u8>)]) -> Vec<Switch> {
        let mut decoder = Decoder::default();
        let mut out = Vec::new();
        for (step, record) in records {
            assert!(is_batch(record));
            decoder.decode(*step, record, &mut out).unwrap();
        }
        out
    }

    const SWITCHES: &[(u64, u32, u32, bool, &str)] = &[
        (10, 1, 1, true, "init"),
        (20, 34, 34, true, "shuffle"),
        (25, 34, 35, true, "shuffle"),
        (31, 11, 11, false, "ksoftirqd/0"),
        (32, 0, 0, false, "swapper/0"),
        (40, 34, 34, true, "shuffle"),
        (4100, 34, 35, true, "shuffle"),
        (4101, 34, 34, true, "shuffle"),
        (9000, 0, 0, false, "swapper/0"),
    ];

    #[test]
    fn switches_batched_through_an_encoder_decode_back() {
        let records = run(&mut Encoder::new(), SWITCHES, 0, 9000);
        let decoded = switches_of(&records);
        let expected: Vec<Switch> = SWITCHES
            .iter()
            .map(|&(step, pid, tid, memory, name)| Switch {
                step,
                pid,
                tid,
                memory,
                name: name.into(),
            })
            .collect();
        assert_eq!(decoded, expected);
        // Three windows, each written at the first step of the next window
        // that the machine stepped at, and the last at the end.
        let steps: Vec<u64> = records.iter().map(|(s, _)| *s).collect();
        assert_eq!(steps, vec![4096, 8192, 9000]);
        // A switch to a task already defined costs two bytes.
        let total: usize = records.iter().map(|(_, r)| r.len() - HEADER_LEN).sum();
        assert!(total < 120, "{total} bytes for {} switches", SWITCHES.len());
    }

    #[test]
    fn a_full_batch_is_written_before_the_switch_that_overflows_it() {
        // Each switch names a new task, so each entry is about 30 bytes.
        let many: Vec<(u64, u32, u32, bool, String)> = (0..700u64)
            .map(|i| {
                (
                    i + 1,
                    100 + i as u32,
                    100 + i as u32,
                    true,
                    format!("worker-{i:03}"),
                )
            })
            .collect();
        let borrowed: Vec<(u64, u32, u32, bool, &str)> = many
            .iter()
            .map(|(s, p, t, m, n)| (*s, *p, *t, *m, n.as_str()))
            .collect();
        let records = run(&mut Encoder::new(), &borrowed, 0, 700);
        assert!(records.len() >= 2);
        assert!(records.iter().all(|(_, r)| r.len() <= RECORD_MAX));
        assert_eq!(switches_of(&records).len(), 700);
        // The first batch was written at the step of the switch it had no
        // room for, before that switch.
        let first_written_at = records[0].0;
        let first_batch = switches_of(&records[..1]);
        assert!(first_batch.last().unwrap().step < first_written_at);
    }

    /// An encoder resumed at a step makes the batches the run made after
    /// that step, whether the step is before, inside or at the end of a
    /// pending batch.
    #[test]
    fn a_resumed_encoder_goes_on_as_the_run_did() {
        let whole = run(&mut Encoder::new(), SWITCHES, 0, 9000);
        for at in [5u64, 10, 24, 25, 32, 4095, 4096, 4100, 8191, 8999] {
            let mut resumed = Encoder::resume(&whole, at).unwrap();
            let rest = run(&mut resumed, SWITCHES, at + 1, 9000);
            let expected: Vec<(u64, Vec<u8>)> =
                whole.iter().filter(|(s, _)| *s > at).cloned().collect();
            assert_eq!(rest, expected, "resumed at step {at}");
        }
    }

    /// The trace a machine restored at a step writes reads with a decoder
    /// resumed at that step, and holds every switch after it, and those
    /// before it that were still pending.
    #[test]
    fn a_resumed_decoder_reads_a_resumed_encoders_batches() {
        let whole = run(&mut Encoder::new(), SWITCHES, 0, 9000);
        let at = 4100;
        let mut resumed = Encoder::resume(&whole, at).unwrap();
        let rest = run(&mut resumed, SWITCHES, at + 1, 9000);
        let mut decoder = Decoder::resume(&whole, at).unwrap();
        let mut out = Vec::new();
        for (step, record) in &rest {
            decoder.decode(*step, record, &mut out).unwrap();
        }
        let steps: Vec<u64> = out.iter().map(|s| s.step).collect();
        assert_eq!(steps, vec![4100, 4101, 9000]);
        assert!(
            Decoder::default()
                .decode(rest[0].0, &rest[0].1, &mut out)
                .is_err()
        );
    }

    #[test]
    fn a_batch_naming_an_undefined_task_or_another_format_is_refused() {
        let mut decoder = Decoder::default();
        let mut out = Vec::new();
        let mut payload = Vec::new();
        put(&mut payload, 5);
        put(&mut payload, 3);
        assert_eq!(
            decoder.decode(7, &batch_record(&payload), &mut out),
            Err(SwitchError::UndefinedTask { step: 7, index: 3 })
        );
        let mut other = batch_record(&[]);
        other[16] = 9;
        assert_eq!(
            Decoder::default().decode(8, &other, &mut out),
            Err(SwitchError::Format { step: 8, format: 9 })
        );
        let cut = batch_record(&[0x05]);
        assert_eq!(
            Decoder::default().decode(9, &cut, &mut out),
            Err(SwitchError::Truncated { step: 9 })
        );
    }
}
