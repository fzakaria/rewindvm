//! An inspection's answer, read from the records of the machine that
//! made it, for an engine with no machine of its own to watch: the QEMU
//! machine runs an inspection, its step log becomes records
//! ([`crate::step_log`]), and the answer is read from those, as
//! rewind-core's inspection reads it from a forked machine's records as
//! they arrive.
//!
//! The inspecting process marks where its answer starts and how it ended;
//! everything it writes between the marks to standard output is the
//! answer, and to standard error the reason for a failure.

use rewind_init::{InspectStatus, Mark};

use crate::{Event, EventKind};

const STDOUT_FD: u32 = 1;
const STDERR_FD: u32 = 2;

/// What an inspection answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inspected {
    pub status: InspectStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// The answer the inspection among `records` gave, or None when the
/// records end before its end mark.
pub fn answer(records: &[(u64, Vec<u8>)]) -> Option<Inspected> {
    let mut pid = None;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    for (step, record) in records {
        let Ok(event) = Event::decode(*step, record) else {
            continue;
        };
        match &event.kind {
            EventKind::Mark { text } => match Mark::parse(text) {
                Some(Mark::InspectBegin) => pid = Some(event.pid),
                Some(Mark::InspectEnd(status)) if pid == Some(event.pid) => {
                    return Some(Inspected {
                        status,
                        stdout,
                        stderr,
                    });
                }
                _ => {}
            },
            EventKind::Output { fd, bytes } if pid == Some(event.pid) => match *fd {
                STDOUT_FD => stdout.extend_from_slice(bytes),
                STDERR_FD => stderr.extend_from_slice(bytes),
                _ => {}
            },
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    // Answers read from hand-built records: only the inspecting
    // process's output between its marks counts.
    use super::*;
    use crate::HEADER_LEN;

    const KIND_OUTPUT: u16 = 2;
    const KIND_MARK: u16 = 10;
    const NS: u64 = 0;

    fn record(kind: u16, pid: u32, aux: u32, payload: &[u8]) -> Vec<u8> {
        let mut r = Vec::new();
        r.extend_from_slice(&((HEADER_LEN + payload.len()) as u32).to_le_bytes());
        r.extend_from_slice(&kind.to_le_bytes());
        r.extend_from_slice(&0u16.to_le_bytes());
        r.extend_from_slice(&pid.to_le_bytes());
        r.extend_from_slice(&pid.to_le_bytes());
        r.extend_from_slice(&aux.to_le_bytes());
        r.extend_from_slice(payload);
        crate::stamped(&r, NS)
    }

    fn mark(pid: u32, mark: Mark) -> Vec<u8> {
        record(KIND_MARK, pid, 0, mark.to_string().as_bytes())
    }

    /// The job's output before and during the answer stays out of it.
    #[test]
    fn only_the_inspecting_process_answers() {
        let records = vec![
            (1, record(KIND_OUTPUT, 7, 1, b"job\n")),
            (2, mark(40, Mark::InspectBegin)),
            (3, record(KIND_OUTPUT, 40, 1, b"hello ")),
            (4, record(KIND_OUTPUT, 7, 1, b"job again\n")),
            (5, record(KIND_OUTPUT, 40, 1, b"world")),
            (6, mark(40, Mark::InspectEnd(InspectStatus::Done))),
        ];
        assert_eq!(
            answer(&records),
            Some(Inspected {
                status: InspectStatus::Done,
                stdout: b"hello world".to_vec(),
                stderr: Vec::new(),
            })
        );
    }

    /// Records that end before the end mark hold no answer.
    #[test]
    fn no_end_mark_is_no_answer() {
        let records = vec![
            (2, mark(40, Mark::InspectBegin)),
            (3, record(KIND_OUTPUT, 40, 1, b"half")),
        ];
        assert_eq!(answer(&records), None);
    }
}
