//! Looking inside a run at a step.
//!
//! A recording holds what the VM reported, not what it held. To show a
//! file as it was at step N, Rewind forks the run there: it restores the
//! nearest keyframe, runs forward to N, and asks the VM's kernel to start
//! `/init --inspect` with the request. That process reads the file and
//! writes it to the Rewind devices, between two marks, and the fork is then
//! thrown away. The recording itself never changes.

use anyhow::{Result, bail};
use rewind_init::{INSPECT_BEGIN_MARK, INSPECT_CAT, INSPECT_END_MARK, InspectStatus};
use rewind_trace::{Event, EventKind};
use rewind_vmm::{Ignore, Observer, Outcome};

use crate::home::Home;
use crate::run::Run;

/// How far a fork runs between checks on the answer, in steps.
const CHUNK_STEPS: u64 = 20_000;

/// How long a fork may run without the inspection starting, in steps. A
/// kernel that predates inspections ignores the request, and the fork would
/// otherwise run to the end of the job.
const START_WITHIN_STEPS: u64 = 2_000_000;

/// Standard output and error, as the Rewind devices tag them.
const STDOUT_FD: u32 = 1;
const STDERR_FD: u32 = 2;

/// What an inspection found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Inspection {
    /// The file's bytes.
    Contents(Vec<u8>),
    /// No such file at that step, with the VM's message.
    NotFound(String),
    /// Anything else that went wrong inside the VM, with its message.
    Failed(String),
}

/// A file's contents at `step`, as process `pid` would have opened it: in
/// its root and working directory, or the job's when `pid` is None or the
/// process is gone by then.
pub fn cat(home: &Home, run: &Run, step: u64, pid: Option<u32>, path: &str) -> Result<Inspection> {
    let mut machine = run.machine_at(home, step, &mut Ignore)?;
    let pid = pid.unwrap_or(0).to_string();
    machine.request_inspection(&[INSPECT_CAT, &pid, path])?;

    // Run in chunks until the answer's end mark arrives.
    let mut answer = Answer::default();
    loop {
        let until = machine.step() + CHUNK_STEPS;
        let outcome = machine.run(Some(until), &mut answer)?;
        if let Some(status) = answer.status {
            return Ok(answer.into_inspection(status));
        }
        if let Outcome::Stopped(stop) = outcome {
            bail!("the VM stopped ({stop:?}) before answering");
        }
        if answer.pid.is_none() && machine.step() >= step + START_WITHIN_STEPS {
            bail!(
                "the VM did not start the inspection within {START_WITHIN_STEPS} steps; \
                 the run's kernel may predate inspections, so record it again"
            );
        }
    }
}

/// The inspection's records, picked out of everything else the fork does.
#[derive(Default)]
struct Answer {
    /// The inspecting process, once its begin mark arrives.
    pid: Option<u32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    status: Option<InspectStatus>,
}

impl Answer {
    fn into_inspection(self, status: InspectStatus) -> Inspection {
        let message = String::from_utf8_lossy(&self.stderr).trim().to_string();
        match status {
            InspectStatus::Done => Inspection::Contents(self.stdout),
            InspectStatus::NotFound => Inspection::NotFound(message),
            InspectStatus::Failed => Inspection::Failed(message),
        }
    }
}

impl Observer for Answer {
    fn record(&mut self, step: u64, record: &[u8]) {
        let Ok(event) = Event::decode(step, record) else {
            return;
        };

        // The begin mark names the process; nothing before it is ours.
        if let EventKind::Mark { text } = &event.kind {
            let text = text.trim();
            if text == INSPECT_BEGIN_MARK {
                self.pid = Some(event.pid);
                return;
            }
            if let Some(code) = text.strip_prefix(INSPECT_END_MARK) {
                if self.pid == Some(event.pid) {
                    self.status = Some(InspectStatus::from_code(code.parse().unwrap_or(1)));
                }
                return;
            }
        }
        if self.pid != Some(event.pid) {
            return;
        }

        // Its output is the answer; its errors explain a failure.
        if let EventKind::Output { fd, bytes } = &event.kind {
            match *fd {
                STDOUT_FD => self.stdout.extend_from_slice(bytes),
                STDERR_FD => self.stderr.extend_from_slice(bytes),
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // The answer collector on hand-built records: only the inspecting
    // process's output between its marks counts, and the end mark's code
    // decides the result.
    use super::*;

    const HEADER: usize = 20;
    const KIND_OUTPUT: u16 = 2;
    const KIND_MARK: u16 = 10;

    fn record(kind: u16, pid: u32, aux: u32, payload: &[u8]) -> Vec<u8> {
        let mut r = Vec::new();
        r.extend_from_slice(&((HEADER + payload.len()) as u32).to_le_bytes());
        r.extend_from_slice(&kind.to_le_bytes());
        r.extend_from_slice(&0u16.to_le_bytes());
        r.extend_from_slice(&pid.to_le_bytes());
        r.extend_from_slice(&pid.to_le_bytes());
        r.extend_from_slice(&aux.to_le_bytes());
        r.extend_from_slice(payload);
        r
    }

    #[test]
    fn collects_only_the_inspecting_process() {
        // A job keeps printing while pid 40 answers; the job's bytes stay out.
        let mut a = Answer::default();
        a.record(1, &record(KIND_OUTPUT, 7, 1, b"job before\n"));
        a.record(2, &record(KIND_MARK, 40, 0, b"rewind-inspect-begin\n"));
        a.record(3, &record(KIND_OUTPUT, 40, 1, b"hello "));
        a.record(4, &record(KIND_OUTPUT, 7, 1, b"job during\n"));
        a.record(5, &record(KIND_OUTPUT, 40, 1, b"world\n"));
        a.record(6, &record(KIND_MARK, 40, 0, b"rewind-inspect-end 0\n"));
        let status = a.status.unwrap();
        assert_eq!(
            a.into_inspection(status),
            Inspection::Contents(b"hello world\n".to_vec())
        );
    }

    #[test]
    fn a_missing_file_reports_the_message() {
        // Code 2 is a missing file, with the VM's message from stderr.
        let mut a = Answer::default();
        a.record(1, &record(KIND_MARK, 40, 0, b"rewind-inspect-begin\n"));
        a.record(
            2,
            &record(KIND_OUTPUT, 40, 2, b"/x: no such file at this step\n"),
        );
        a.record(3, &record(KIND_MARK, 40, 0, b"rewind-inspect-end 2\n"));
        let status = a.status.unwrap();
        assert_eq!(
            a.into_inspection(status),
            Inspection::NotFound("/x: no such file at this step".into())
        );
    }
}
