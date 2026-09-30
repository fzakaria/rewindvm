//! Guest records, decoded.
//!
//! The guest kernel writes one record per event (see rewind_emit in
//! guest/linux/rewind-guest.patch): a 20 byte header of length, kind, flags,
//! pid, tid and a kind-specific `aux` word, then a payload.

use serde::{Deserialize, Serialize};

pub const HEADER_LEN: usize = 20;

/// Record kinds, as numbered by the guest.
mod kind {
    pub const CONSOLE: u16 = 1;
    pub const OUTPUT: u16 = 2;
    pub const EXEC: u16 = 3;
    pub const FORK: u16 = 4;
    pub const EXIT: u16 = 5;
    pub const SIGNAL: u16 = 6;
    pub const OPEN: u16 = 7;
    pub const UNLINK: u16 = 8;
    pub const RENAME: u16 = 9;
    pub const MARK: u16 = 10;
}

/// The flag on fork and exit records that marks a thread rather than a
/// process.
const FLAG_THREAD: u16 = 1;

/// One thing the guest did, and the step it happened on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub step: u64,
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
    /// A record this version does not know.
    Unknown {
        kind: u16,
        data: Vec<u8>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("record is {0} bytes, shorter than its header")]
    Short(usize),
    #[error("record says it is {claimed} bytes but is {actual}")]
    Length { claimed: usize, actual: usize },
}

impl Event {
    /// Decodes one record as the guest wrote it.
    pub fn decode(step: u64, record: &[u8]) -> Result<Event, DecodeError> {
        if record.len() < HEADER_LEN {
            return Err(DecodeError::Short(record.len()));
        }
        let u16_at = |at: usize| u16::from_le_bytes([record[at], record[at + 1]]);
        let u32_at = |at: usize| u32::from_le_bytes(record[at..at + 4].try_into().unwrap());

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
        let data = &record[HEADER_LEN..];
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
                    .map_or(0, |b| i32::from_le_bytes(b.try_into().unwrap()));
                let addr = data
                    .get(4..12)
                    .map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap()));
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
            other => EventKind::Unknown {
                kind: other,
                data: data.to_vec(),
            },
        };
        Ok(Event {
            step,
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

    fn record(kind: u16, flags: u16, pid: u32, tid: u32, aux: u32, data: &[u8]) -> Vec<u8> {
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
