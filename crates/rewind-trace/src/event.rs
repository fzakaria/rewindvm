//! Guest records, decoded.
//!
//! The guest kernel writes one record per event (see rewind_emit in
//! guest/linux/rewind-guest.patch): a 20 byte header of length, kind, flags,
//! pid, tid and a kind-specific `aux` word, then a payload.
//!
//! The guest has no clock of its own to put in a record: the VM's clock
//! is the monitor's. The monitor stamps each event's record with the
//! clock before the record goes into the trace ([`stamped`]), so an event
//! in a trace says when it happened as well as on which step. A thread
//! can compute for a long time between two steps, so the step alone does
//! not say when.

use serde::{Deserialize, Serialize};

/// The header the guest writes at the start of every record.
pub const HEADER_LEN: usize = 20;

/// The bytes of the clock the monitor stamps into an event's record,
/// between the guest's header and the payload.
pub const CLOCK_LEN: usize = 8;

/// The header of an event's record in a trace: the guest's, then the
/// clock.
pub const EVENT_HEADER_LEN: usize = HEADER_LEN + CLOCK_LEN;

/// Where a record's length is, and how many bytes the length takes.
const LENGTH_AT: usize = 0;
const LENGTH_LEN: usize = 4;

/// The record of an event as a trace holds it: `record`, which the guest
/// wrote, with `ns`, the VM's clock when the event happened, after its
/// header, and its length the longer for it.
///
/// `record` is at least a header, as the monitor has checked of every
/// record it takes from the guest.
pub fn stamped(record: &[u8], ns: u64) -> Vec<u8> {
    let mut out = Vec::new();
    stamp_into(record, ns, &mut out);
    out
}

/// Makes `out` the record [`stamped`] gives of `record` and `ns`, in the
/// memory `out` already has when that is enough: the monitor stamps a
/// record at every event and keeps one buffer for them all.
pub fn stamp_into(record: &[u8], ns: u64, out: &mut Vec<u8>) {
    let (header, payload) = record.split_at(HEADER_LEN);
    let len = (record.len() + CLOCK_LEN) as u32;
    out.clear();
    out.extend_from_slice(header);
    out.extend_from_slice(&ns.to_le_bytes());
    out.extend_from_slice(payload);
    out[LENGTH_AT..LENGTH_AT + LENGTH_LEN].copy_from_slice(&len.to_le_bytes());
}

/// The longest record the guest writes, header included: its
/// REWIND_RECORD_MAX, which the monitor holds it to.
pub const RECORD_MAX: usize = 8192;

/// The longest record a trace holds: the guest's longest, with the clock
/// the monitor stamps into an event's.
pub const TRACE_RECORD_MAX: usize = RECORD_MAX + CLOCK_LEN;

/// Record kinds, as numbered by the guest.
mod kind {
    pub(super) const CONSOLE: u16 = 1;
    pub(super) const OUTPUT: u16 = 2;
    pub(super) const EXEC: u16 = 3;
    pub(super) const FORK: u16 = 4;
    pub(super) const EXIT: u16 = 5;
    pub(super) const SIGNAL: u16 = 6;
    pub(super) const OPEN: u16 = 7;
    pub(super) const UNLINK: u16 = 8;
    pub(super) const RENAME: u16 = 9;
    pub(super) const MARK: u16 = 10;
    pub(super) const READ_OPEN: u16 = 11;
    pub(super) const CLOSE: u16 = 12;
}

/// The flag on fork and exit records that marks a thread rather than a
/// process.
const FLAG_THREAD: u16 = 1;

/// One thing the guest did, and the step it happened on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub step: u64,
    /// The VM's clock, in nanoseconds since boot, when the event
    /// happened: the clock as the event's step left it, so the work its
    /// thread did up to the event is before it.
    pub ns: u64,
    /// The thread group (process) id.
    pub pid: u32,
    /// The thread id.
    pub tid: u32,
    pub kind: EventKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    /// Kernel log text.
    Console {
        text: String,
    },
    /// Bytes a process wrote to its standard output (fd 1) or error (fd 2).
    Output {
        fd: u32,
        bytes: Vec<u8>,
    },
    /// A process replaced its image.
    Exec {
        filename: String,
        argv: Vec<String>,
        old_pid: u32,
    },
    /// A process or thread was created. `pid` and `tid` are the parent's.
    Fork {
        child: u32,
        thread: bool,
    },
    /// A process or thread ended. `status` is the kernel's exit_code: the
    /// exit status in bits 8 to 15, or the terminating signal in bits 0 to 6.
    Exit {
        status: u32,
        comm: String,
        thread: bool,
    },
    /// A signal was delivered. `addr` is the faulting address for SIGSEGV,
    /// SIGBUS, SIGILL and SIGFPE.
    Signal {
        signo: u32,
        code: i32,
        addr: u64,
    },
    /// A file was opened for writing, created or truncated.
    Open {
        path: String,
        flags: u32,
    },
    Unlink {
        path: String,
    },
    Rename {
        from: String,
        to: String,
    },
    /// A line written to /dev/rewind: a label on the timeline.
    Mark {
        text: String,
    },
    /// A regular file was opened for reading while some process had it
    /// open for writing, so the reader may see it half written. Opens of
    /// files nobody is writing are not recorded.
    ReadOpen {
        path: String,
        flags: u32,
    },
    /// The last reference to a regular file opened for writing was
    /// closed: the writes through it are over.
    Close {
        path: String,
    },
    /// A record this version does not know.
    Unknown {
        kind: u16,
        data: Vec<u8>,
    },
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("record is {0} bytes, shorter than its header")]
    Short(usize),
    #[error("record says it is {claimed} bytes but is {actual}")]
    Length { claimed: usize, actual: usize },
}

impl EventKind {
    /// Whether two events did the same thing, apart from the ids the
    /// kernel handed out, which a rescheduled run may hand out in another
    /// order: the child a fork names and the pid an exec reports.
    pub fn alike(&self, other: &EventKind) -> bool {
        match (self, other) {
            (EventKind::Fork { thread: a, .. }, EventKind::Fork { thread: b, .. }) => a == b,
            (
                EventKind::Exec {
                    filename: f,
                    argv: a,
                    ..
                },
                EventKind::Exec {
                    filename: g,
                    argv: b,
                    ..
                },
            ) => f == g && a == b,
            _ => self == other,
        }
    }
}

impl EventKind {
    /// What the event did, in words, as `rewind events` and `rewind
    /// summary` print it: `write(1, "done\n")`, `exit_group(cc) exited:2`.
    pub fn words(&self) -> String {
        match self {
            EventKind::Console { text } => format!("console {:?}", text.trim_end()),
            EventKind::Output { fd, bytes } => {
                format!("write({fd}, {:?})", String::from_utf8_lossy(bytes))
            }
            EventKind::Exec { filename, argv, .. } => {
                format!("execve({filename:?}, {argv:?})")
            }
            EventKind::Fork { child, thread } => {
                let how = if *thread {
                    "clone(CLONE_THREAD)"
                } else {
                    "fork()"
                };
                format!("{how} = {child}")
            }
            EventKind::Exit {
                status,
                comm,
                thread,
            } => {
                let who = if *thread { "thread exit" } else { "exit_group" };
                format!(
                    "{who}({comm}) {}",
                    crate::ending::ExitStatus::from_raw(*status)
                )
            }
            EventKind::Signal { signo, code, addr } => {
                format!("{} code={code} addr={addr:#x}", signal_name(*signo))
            }
            EventKind::Open { path, flags } => format!("open({path:?}, {flags:#o})"),
            EventKind::Unlink { path } => format!("unlink({path:?})"),
            EventKind::Rename { from, to } => format!("rename({from:?}, {to:?})"),
            EventKind::Mark { text } => format!("mark {text:?}"),
            EventKind::ReadOpen { path, flags } => {
                format!("open({path:?}, {flags:#o}) while it is being written")
            }
            EventKind::Close { path } => format!("close({path:?})"),
            EventKind::Unknown { kind, data } => {
                format!("record kind {kind}, {} bytes", data.len())
            }
        }
    }
}

impl Event {
    /// The mark this event is when init wrote it: a mark from init's pid
    /// that reads as one of init's. The job can write the same words, and
    /// they stay plain marks.
    pub fn init_mark(&self) -> Option<rewind_init::Mark> {
        let EventKind::Mark { text } = &self.kind else {
            return None;
        };
        if self.pid != rewind_init::INIT_PID {
            return None;
        }
        rewind_init::Mark::parse(text)
    }

    /// Decodes one event's record as a trace holds it: as the guest
    /// wrote it, with the clock the monitor stamped in.
    pub fn decode(step: u64, record: &[u8]) -> Result<Event, DecodeError> {
        if record.len() < EVENT_HEADER_LEN {
            return Err(DecodeError::Short(record.len()));
        }
        let u16_at = |at: usize| u16::from_le_bytes([record[at], record[at + 1]]);
        let u32_at =
            |at: usize| u32::from_le_bytes(record[at..at + 4].try_into().expect("four bytes"));

        let claimed = u32_at(0) as usize;
        if claimed != record.len() {
            return Err(DecodeError::Length {
                claimed,
                actual: record.len(),
            });
        }
        let kind = u16_at(4);
        let flags = u16_at(6);
        let pid = u32_at(8);
        let tid = u32_at(12);
        let aux = u32_at(16);
        let ns = u64::from_le_bytes(
            record[HEADER_LEN..EVENT_HEADER_LEN]
                .try_into()
                .expect("eight bytes"),
        );
        let data = &record[EVENT_HEADER_LEN..];
        let thread = flags & FLAG_THREAD != 0;

        let kind = match kind {
            kind::CONSOLE => EventKind::Console { text: text(data) },
            kind::OUTPUT => EventKind::Output {
                fd: aux,
                bytes: data.to_vec(),
            },
            kind::EXEC => {
                let mut parts = data.splitn(2, |b| *b == 0);
                let filename = text(parts.next().unwrap_or_default());
                let argv = parts
                    .next()
                    .unwrap_or_default()
                    .split(|b| *b == 0)
                    .filter(|a| !a.is_empty())
                    .map(text)
                    .collect();
                EventKind::Exec {
                    filename,
                    argv,
                    old_pid: aux,
                }
            }
            kind::FORK => EventKind::Fork { child: aux, thread },
            kind::EXIT => EventKind::Exit {
                status: aux,
                comm: text(data),
                thread,
            },
            kind::SIGNAL => {
                let code = data
                    .get(0..4)
                    .map_or(0, |b| i32::from_le_bytes(b.try_into().expect("four bytes")));
                let addr = data.get(4..12).map_or(0, |b| {
                    u64::from_le_bytes(b.try_into().expect("eight bytes"))
                });
                EventKind::Signal {
                    signo: aux,
                    code,
                    addr,
                }
            }
            kind::OPEN => EventKind::Open {
                path: text(data),
                flags: aux,
            },
            kind::UNLINK => EventKind::Unlink { path: text(data) },
            kind::RENAME => {
                let mut parts = data.splitn(2, |b| *b == 0);
                EventKind::Rename {
                    from: text(parts.next().unwrap_or_default()),
                    to: text(parts.next().unwrap_or_default()),
                }
            }
            kind::MARK => EventKind::Mark {
                text: text(data).trim_end().to_string(),
            },
            kind::READ_OPEN => EventKind::ReadOpen {
                path: text(data),
                flags: aux,
            },
            kind::CLOSE => EventKind::Close { path: text(data) },
            other => EventKind::Unknown {
                kind: other,
                data: data.to_vec(),
            },
        };
        Ok(Event {
            step,
            ns,
            pid,
            tid,
            kind,
        })
    }
}

fn text(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// The name of a signal, for display.
pub fn signal_name(signo: u32) -> &'static str {
    match signo {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        4 => "SIGILL",
        5 => "SIGTRAP",
        6 => "SIGABRT",
        7 => "SIGBUS",
        8 => "SIGFPE",
        9 => "SIGKILL",
        10 => "SIGUSR1",
        11 => "SIGSEGV",
        12 => "SIGUSR2",
        13 => "SIGPIPE",
        14 => "SIGALRM",
        15 => "SIGTERM",
        17 => "SIGCHLD",
        _ => "signal",
    }
}

#[cfg(test)]
mod tests {
    // Decoding: records built the way the guest lays them out come back as
    // the events they describe, and malformed ones are refused.
    use super::*;

    /// The clock the records of these tests are stamped with.
    const NS: u64 = 5_000_000_123;

    /// A record as the guest lays one out.
    fn guest_record(kind: u16, flags: u16, pid: u32, tid: u32, aux: u32, data: &[u8]) -> Vec<u8> {
        let mut r = Vec::new();
        r.extend_from_slice(&((HEADER_LEN + data.len()) as u32).to_le_bytes());
        r.extend_from_slice(&kind.to_le_bytes());
        r.extend_from_slice(&flags.to_le_bytes());
        r.extend_from_slice(&pid.to_le_bytes());
        r.extend_from_slice(&tid.to_le_bytes());
        r.extend_from_slice(&aux.to_le_bytes());
        r.extend_from_slice(data);
        r
    }

    /// A record as the trace holds one: the guest's, stamped with [`NS`].
    fn record(kind: u16, flags: u16, pid: u32, tid: u32, aux: u32, data: &[u8]) -> Vec<u8> {
        stamped(&guest_record(kind, flags, pid, tid, aux, data), NS)
    }

    // Tests that an event has the clock the monitor stamped its record
    // with, and that the stamp leaves the rest of the record as the guest
    // wrote it. A write of three bytes is stamped: the record is eight
    // bytes longer and says so, and decodes to the write, at the clock.
    #[test]
    fn an_event_has_the_clock_its_record_was_stamped_with() {
        const OUTPUT: u16 = 2;
        const STDERR: u32 = 2;
        let guest = guest_record(OUTPUT, 0, 7, 8, STDERR, b"abc");
        let stamped = stamped(&guest, NS);
        assert_eq!(stamped.len(), guest.len() + CLOCK_LEN);

        let event = Event::decode(42, &stamped).unwrap();
        assert_eq!((event.step, event.ns, event.pid, event.tid), (42, NS, 7, 8));
        assert_eq!(
            event.kind,
            EventKind::Output {
                fd: STDERR,
                bytes: b"abc".to_vec()
            }
        );
    }

    // Tests that a record the guest wrote, with no clock stamped in, is
    // refused when it is too short to hold one: a mark with no text is a
    // header alone.
    #[test]
    fn a_record_without_a_clock_is_refused() {
        const MARK: u16 = 10;
        let guest = guest_record(MARK, 0, 1, 1, 0, b"");
        assert_eq!(
            Event::decode(1, &guest),
            Err(DecodeError::Short(guest.len()))
        );
    }

    #[test]
    fn exec_splits_filename_and_argv() {
        let r = record(3, 0, 7, 7, 7, b"/bin/cc\0cc\0-c\0pool.c\0");
        let e = Event::decode(42, &r).unwrap();
        assert_eq!(e.step, 42);
        assert_eq!(
            e.kind,
            EventKind::Exec {
                filename: "/bin/cc".into(),
                argv: vec!["cc".into(), "-c".into(), "pool.c".into()],
                old_pid: 7
            }
        );
    }

    #[test]
    fn fork_and_exit_carry_the_thread_flag() {
        let e = Event::decode(1, &record(4, 1, 10, 10, 11, b"")).unwrap();
        assert_eq!(
            e.kind,
            EventKind::Fork {
                child: 11,
                thread: true
            }
        );
        let e = Event::decode(2, &record(5, 0, 10, 10, 256, b"make")).unwrap();
        assert_eq!(
            e.kind,
            EventKind::Exit {
                status: 256,
                comm: "make".into(),
                thread: false
            }
        );
    }

    #[test]
    fn signal_carries_code_and_address() {
        let mut data = Vec::new();
        data.extend_from_slice(&1i32.to_le_bytes());
        data.extend_from_slice(&0xdead_beefu64.to_le_bytes());
        let e = Event::decode(3, &record(6, 0, 5, 6, 11, &data)).unwrap();
        assert_eq!(
            e.kind,
            EventKind::Signal {
                signo: 11,
                code: 1,
                addr: 0xdead_beef
            }
        );
    }

    #[test]
    fn rejects_a_record_whose_length_lies() {
        let mut r = record(1, 0, 0, 0, 0, b"hello");
        r.pop();
        assert!(matches!(
            Event::decode(0, &r),
            Err(DecodeError::Length { .. })
        ));
        assert!(matches!(
            Event::decode(0, &r[..4]),
            Err(DecodeError::Short(4))
        ));
    }
}
