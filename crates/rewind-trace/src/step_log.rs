//! The step log of Rewind's machine on QEMU, turned into a trace.
//!
//! The QEMU machine (`qemu/`, GPL-2.0) does to the guest what
//! rewind-vmm does: it counts the steps, moves the clock, takes the
//! guest's buffered read and write records and sends the interrupt. It
//! does not encode the trace. It writes a step log of what rewind-vmm
//! hands its trace encoders, in the same order, and [`encode`] runs the
//! same encoders over it, so trace.bin is made by this crate's code for
//! both machines.
//!
//! The log starts with [`MAGIC`]. Each entry is a tag byte and its
//! fields, little endian:
//!
//! | Tag | Entry  | Fields                                   |
//! | --- | ------ | ---------------------------------------- |
//! | 1   | Step   | u64 step, u64 clock as the step before left it |
//! | 2   | Io     | u32 lost, u32 length, the guest's buffer |
//! | 3   | Setup  | none: the guest named its shared page    |
//! | 4   | Record | u64 clock once the step moved it, u32 length, the record |
//! | 5   | End    | u8 why, u64 step, u64 clock              |
//! | 6   | Resume | u64 step, u64 clock, u8 whether set up   |
//!
//! An Io entry after an End entry, the log's last, holds what the stop
//! drained. A machine started from a keyframe instead of from boot
//! writes a Resume entry first, with the keyframe's step and clock and
//! whether the guest had set up its shared page by then; the encoders
//! then go on from the recording's trace up to that step, as rewind-vmm's
//! do on a restore.

use std::fmt;

use crate::io_records;
use crate::switches;

/// The first bytes of a step log.
pub const MAGIC: &[u8; 8] = b"rwqlog1\0";

const TAG_STEP: u8 = 1;
const TAG_IO: u8 = 2;
const TAG_SETUP: u8 = 3;
const TAG_RECORD: u8 = 4;
const TAG_END: u8 = 5;
const TAG_RESUME: u8 = 6;

/// Why the machine stopped, as the End entry says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    PowerOff,
    Restart,
    Halt,
    TripleFault,
    /// The guest went idle with no timer armed.
    Stalled,
    /// The machine reached the step it was asked to stop at; its guest
    /// did not stop.
    Paused,
}

impl Stop {
    fn from_byte(byte: u8) -> Option<Stop> {
        Some(match byte {
            0 => Stop::PowerOff,
            1 => Stop::Restart,
            2 => Stop::Halt,
            3 => Stop::TripleFault,
            4 => Stop::Stalled,
            5 => Stop::Paused,
            _ => return None,
        })
    }
}

/// A trace made from a step log: its records, each with its step, and
/// how the machine stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Encoded {
    pub records: Vec<(u64, Vec<u8>)>,
    /// The last step, and why the machine stopped there; None for a log
    /// that ends without saying, as one of a machine that was killed.
    pub end: Option<(u64, Stop)>,
    /// The clock at the end.
    pub ns: u64,
}

/// A step log that does not read.
#[derive(Debug, PartialEq, Eq)]
pub enum LogError {
    Magic,
    Truncated {
        at: usize,
    },
    Tag {
        at: usize,
        tag: u8,
    },
    Stop {
        at: usize,
        why: u8,
    },
    AfterEnd {
        at: usize,
    },
    Switch {
        step: u64,
        message: String,
    },
    /// The recording's trace does not resume the encoders at the
    /// keyframe's step.
    Resume {
        step: u64,
        message: String,
    },
}

impl fmt::Display for LogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LogError::Magic => write!(f, "not a step log of Rewind's QEMU machine"),
            LogError::Truncated { at } => {
                write!(f, "the step log ends inside an entry at byte {at}")
            }
            LogError::Tag { at, tag } => {
                write!(f, "the step log has unknown tag {tag} at byte {at}")
            }
            LogError::Stop { at, why } => {
                write!(
                    f,
                    "the step log's end at byte {at} has unknown reason {why}"
                )
            }
            LogError::AfterEnd { at } => {
                write!(f, "the step log goes on past its end, at byte {at}")
            }
            LogError::Switch { step, message } => {
                write!(
                    f,
                    "the switch record at step {step} does not read: {message}"
                )
            }
            LogError::Resume { step, message } => {
                write!(f, "the trace does not resume at step {step}: {message}")
            }
        }
    }
}

impl std::error::Error for LogError {}

/// Reads a step log's fields in order.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], LogError> {
        let end = self
            .at
            .checked_add(n)
            .filter(|end| *end <= self.bytes.len())
            .ok_or(LogError::Truncated { at: self.at })?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    fn u8(&mut self) -> Result<u8, LogError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, LogError> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("four bytes"),
        ))
    }

    fn u64(&mut self) -> Result<u64, LogError> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("eight bytes"),
        ))
    }

    fn sized(&mut self) -> Result<&'a [u8], LogError> {
        let len = self.u32()? as usize;
        self.take(len)
    }

    fn done(&self) -> bool {
        self.at == self.bytes.len()
    }
}

/// The encoders rewind-vmm keeps once the guest has named its shared
/// page, and the trace they have made.
#[derive(Default)]
struct Encoders {
    io: Option<io_records::Encoder>,
    switches: Option<switches::Encoder>,
    records: Vec<(u64, Vec<u8>)>,
}

impl Encoders {
    /// rewind-vmm's flush_switches: the batches due at `step`, given the
    /// clock as the step before left it.
    fn flush(&mut self, step: u64, ns: u64) {
        if let Some(encoder) = self.switches.as_mut() {
            for batch in encoder.at_step(step, ns) {
                self.records.push((step, batch));
            }
        }
    }

    /// The rest of rewind-vmm's drain_io: the buffered records at `step`,
    /// and their wakes, stamped `ns`, among the switches.
    fn io(&mut self, step: u64, ns: u64, lost: u32, buffered: &[u8]) {
        let Some(encoder) = self.io.as_mut() else {
            return;
        };
        let mut wakes = Vec::new();
        for record in encoder.encode(step, buffered, lost, &mut wakes) {
            self.records.push((step, record));
        }
        let Some(switches) = self.switches.as_mut() else {
            return;
        };
        for wake in &mut wakes {
            wake.ns = ns;
            if let Some(batch) = switches.add_wake(wake) {
                self.records.push((step, batch));
            }
        }
    }

    /// rewind-vmm's io_out for the record port, then write_event: a
    /// switch joins the pending batch, and any other record is an event
    /// stamped with the clock its step left.
    fn record(&mut self, step: u64, ns: u64, record: &[u8]) -> Result<(), LogError> {
        if switches::is_switch(record)
            && let Some(encoder) = self.switches.as_mut()
        {
            let batch = encoder.add(step, record).map_err(|e| LogError::Switch {
                step,
                message: e.to_string(),
            })?;
            if let Some(batch) = batch {
                self.records.push((step, batch));
            }
            return Ok(());
        }
        let mut stamped = Vec::new();
        crate::stamp_into(record, ns, &mut stamped);
        self.records.push((step, stamped));
        Ok(())
    }

    /// rewind-vmm's finish_switches.
    fn finish(&mut self, step: u64, ns: u64) {
        if let Some(encoder) = self.switches.as_mut() {
            for batch in encoder.finish(ns) {
                self.records.push((step, batch));
            }
        }
    }
}

/// The trace a step log makes: what rewind-vmm's observer would have
/// been handed, step for step, by a machine that did the same. A log of
/// a machine started from a keyframe goes on from `trace`, the
/// recording's whole trace, at the keyframe's step; a log from boot
/// reads none of it.
pub fn encode(log: &[u8], trace: &[(u64, Vec<u8>)]) -> Result<Encoded, LogError> {
    if !log.starts_with(MAGIC) {
        return Err(LogError::Magic);
    }
    let mut r = Reader {
        bytes: log,
        at: MAGIC.len(),
    };
    let mut enc = Encoders::default();
    let mut step = 0;
    let mut ns = 0;
    let mut end = None;

    while !r.done() {
        let at = r.at;
        let tag = r.u8()?;
        if end.is_some() && tag != TAG_IO {
            return Err(LogError::AfterEnd { at });
        }
        match tag {
            TAG_STEP => {
                step = r.u64()?;
                ns = r.u64()?;
                enc.flush(step, ns);
            }
            TAG_IO => {
                let lost = r.u32()?;
                let buffered = r.sized()?;
                enc.io(step, ns, lost, buffered);
            }
            TAG_RESUME => {
                step = r.u64()?;
                ns = r.u64()?;
                if r.u8()? != 0 {
                    let resume = |message: String| LogError::Resume { step, message };
                    let through = trace.partition_point(|(s, _)| *s <= step);
                    enc.io = Some(
                        io_records::Encoder::resume(&trace[..through])
                            .map_err(|e| resume(e.to_string()))?,
                    );
                    enc.switches = Some(
                        switches::Encoder::resume(trace, step)
                            .map_err(|e| resume(e.to_string()))?,
                    );
                }
            }
            TAG_SETUP => {
                enc.io = Some(io_records::Encoder::new());
                enc.switches = Some(switches::Encoder::new());
            }
            TAG_RECORD => {
                let stamp = r.u64()?;
                let record = r.sized()?;
                enc.record(step, stamp, record)?;
                ns = stamp;
            }
            TAG_END => {
                let why = r.u8()?;
                let why = Stop::from_byte(why).ok_or(LogError::Stop { at, why })?;
                step = r.u64()?;
                ns = r.u64()?;
                // A machine paused at a step writes only its pending
                // switches; one that stopped drains first (rewind-vmm's
                // end_trace, rewind-core's end_replay).
                if why != Stop::Paused {
                    enc.flush(step, ns);
                }
                end = Some((step, why));
            }
            tag => return Err(LogError::Tag { at, tag }),
        }
    }
    if end.is_some() {
        enc.finish(step, ns);
    }
    Ok(Encoded {
        records: enc.records,
        end,
        ns,
    })
}

#[cfg(test)]
mod tests {
    // Step logs written by hand, as the QEMU machine writes them, turned
    // into the records rewind-vmm's monitor would have handed its trace.
    use super::*;
    use crate::HEADER_LEN;

    /// A guest event record: the header with `kind`, then `payload`.
    fn guest_record(kind: u16, payload: &[u8]) -> Vec<u8> {
        let mut r = vec![0u8; HEADER_LEN];
        r[0..4].copy_from_slice(&((HEADER_LEN + payload.len()) as u32).to_le_bytes());
        r[4..6].copy_from_slice(&kind.to_le_bytes());
        r.extend_from_slice(payload);
        r
    }

    /// A step log made of entries.
    struct Log(Vec<u8>);

    impl Log {
        fn new() -> Log {
            Log(MAGIC.to_vec())
        }
        fn step(mut self, step: u64, ns: u64) -> Log {
            self.0.push(TAG_STEP);
            self.0.extend_from_slice(&step.to_le_bytes());
            self.0.extend_from_slice(&ns.to_le_bytes());
            self
        }
        fn setup(mut self) -> Log {
            self.0.push(TAG_SETUP);
            self
        }
        fn record(mut self, ns: u64, record: &[u8]) -> Log {
            self.0.push(TAG_RECORD);
            self.0.extend_from_slice(&ns.to_le_bytes());
            self.0
                .extend_from_slice(&(record.len() as u32).to_le_bytes());
            self.0.extend_from_slice(record);
            self
        }
        fn resume(mut self, step: u64, ns: u64, set_up: bool) -> Log {
            self.0.push(TAG_RESUME);
            self.0.extend_from_slice(&step.to_le_bytes());
            self.0.extend_from_slice(&ns.to_le_bytes());
            self.0.push(u8::from(set_up));
            self
        }
        fn end(mut self, why: u8, step: u64, ns: u64) -> Log {
            self.0.push(TAG_END);
            self.0.push(why);
            self.0.extend_from_slice(&step.to_le_bytes());
            self.0.extend_from_slice(&ns.to_le_bytes());
            self
        }
    }

    /// A console record at a step is an event, stamped with the clock
    /// the step left and framed at its step, as rewind-vmm writes it.
    #[test]
    fn an_event_is_stamped_with_its_steps_clock() {
        const KIND_CONSOLE: u16 = 1;
        let console = guest_record(KIND_CONSOLE, b"hello\n");
        let log = Log::new()
            .step(1, 0)
            .record(5000, &console)
            .step(2, 5000)
            .end(0, 2, 5000);
        let encoded = encode(&log.0, &[]).unwrap();
        assert_eq!(encoded.records, vec![(1, crate::stamped(&console, 5000))]);
        assert_eq!(encoded.end, Some((2, Stop::PowerOff)));
        assert_eq!(encoded.ns, 5000);
    }

    /// A switch record before setup has no encoder to join, so it is an
    /// event like any other; after setup it joins the batch the end
    /// writes, as rewind-vmm's encoders do.
    #[test]
    fn switches_batch_only_after_setup() {
        const KIND_SWITCH: u16 = 14;
        // The system call the task left the CPU in, none, then its name.
        let mut payload = (-1i32).to_le_bytes().to_vec();
        payload.extend_from_slice(b"worker\0");
        let switch = guest_record(KIND_SWITCH, &payload);
        let before = encode(
            &Log::new()
                .step(1, 0)
                .record(5000, &switch)
                .end(0, 1, 5000)
                .0,
            &[],
        )
        .unwrap();
        assert_eq!(before.records, vec![(1, crate::stamped(&switch, 5000))]);

        let mut expected = switches::Encoder::new();
        expected.add(2, &switch).unwrap();
        let batches = expected.finish(10_000);
        let after = encode(
            &Log::new()
                .step(1, 0)
                .setup()
                .step(2, 5000)
                .record(10_000, &switch)
                .end(0, 2, 10_000)
                .0,
            &[],
        )
        .unwrap();
        assert_eq!(
            after.records,
            batches.into_iter().map(|b| (2, b)).collect::<Vec<_>>()
        );
    }

    /// A machine resumed after setup batches a switch, as its encoders go
    /// on from the trace; one resumed before setup writes the switch as
    /// an event, as a machine from boot would.
    #[test]
    fn a_resumed_machine_goes_on_from_the_trace() {
        const KIND_SWITCH: u16 = 14;
        let mut payload = (-1i32).to_le_bytes().to_vec();
        payload.extend_from_slice(b"worker\0");
        let switch = guest_record(KIND_SWITCH, &payload);
        let log = |set_up| {
            Log::new()
                .resume(100, 500_000, set_up)
                .step(101, 500_000)
                .record(505_000, &switch)
                .end(0, 101, 505_000)
                .0
        };

        let mut expected = switches::Encoder::resume(&[], 100).unwrap();
        expected.add(101, &switch).unwrap();
        let batches: Vec<_> = expected
            .finish(505_000)
            .into_iter()
            .map(|b| (101, b))
            .collect();
        assert_eq!(encode(&log(true), &[]).unwrap().records, batches);
        assert_eq!(
            encode(&log(false), &[]).unwrap().records,
            vec![(101, crate::stamped(&switch, 505_000))]
        );
    }

    /// A log that is not one, one cut inside an entry, and one that goes
    /// on past its end are refused, each saying where.
    #[test]
    fn broken_logs_are_refused() {
        assert_eq!(encode(b"nonsense", &[]), Err(LogError::Magic));
        let cut = Log::new().step(1, 0).0;
        assert_eq!(
            encode(&cut[..cut.len() - 1], &[]),
            Err(LogError::Truncated {
                at: MAGIC.len() + 9
            })
        );
        let past = Log::new().end(0, 0, 0).step(1, 0).0;
        assert_eq!(
            encode(&past, &[]),
            Err(LogError::AfterEnd {
                at: MAGIC.len() + 18
            })
        );
    }
}
