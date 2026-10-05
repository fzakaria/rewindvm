//! Looking inside a run at a step.
//!
//! A recording holds what the VM reported, not what it held. To show a
//! file as it was at step N, Rewind forks the run there: it restores the
//! nearest keyframe, runs forward to N, and asks the VM's kernel to start
//! `/init --inspect` with the request. That process reads the file and
//! writes it to the Rewind devices, between two marks, and the fork is then
//! thrown away. The recording itself never changes.
//!
//! A shell is the same, with a person attached: the request starts a shell
//! on a pty inside the VM, what it prints comes back through the console
//! device, and what the person types goes in through it.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use anyhow::{Result, bail};
use rewind_init::{CONSOLE_FD, InspectRequest, InspectStatus, Mark};
use rewind_trace::{Event, EventKind};
use rewind_vmm::pv::GuestExit;
use rewind_vmm::{Ignore, Input, Machine, Observer, Outcome, Stop};

use crate::home::Home;
use crate::run::{Keep, Run};

/// How far a fork runs between checks on the answer, in steps.
const CHUNK_STEPS: u64 = 20_000;

/// How long a fork may run without the inspection starting, in steps. A
/// kernel that never starts it would otherwise run the fork to the end of
/// the job.
const START_WITHIN_STEPS: u64 = 2_000_000;

/// Standard output and error, as the Rewind devices tag them.
const STDOUT_FD: u32 = 1;
const STDERR_FD: u32 = 2;

/// Where a shell's output goes as it arrives: the person's terminal.
pub type Console = Box<dyn FnMut(&[u8])>;

/// What a shell starts with: the process whose view it takes (the job's
/// when None), its terminal's size as (columns, rows), and any extra
/// packages.
pub struct Session {
    pub pid: Option<u32>,
    pub size: (u16, u16),
    pub extras: Option<Extras>,
}

/// More Nix packages for a shell: an image of their closure, mapped into the
/// fork's extras slot, and the bin directories to put first on its PATH.
pub struct Extras {
    pub image: std::path::PathBuf,
    pub bins: Vec<String>,
}

impl Extras {
    /// The extras for `installables`: built or substituted, their closure
    /// packed into an erofs image kept in the home, named by the closure
    /// so a second shell with the same packages reuses it.
    pub fn build(home: &Home, installables: &[String]) -> Result<Extras> {
        let (outputs, closure) = crate::nix::packages(installables)?;
        let key = blake3::hash(
            closure
                .iter()
                .map(|p| p.to_string_lossy())
                .collect::<Vec<_>>()
                .join("\n")
                .as_bytes(),
        )
        .to_hex();
        let image = home.images().join(format!("extras-{}.erofs", &key[..32]));
        if !image.exists() {
            eprintln!("rewind: packing {} store paths for --with", closure.len());
            let tmp = crate::image::temp_beside(&image);
            crate::image::from_store_paths(&closure, &tmp)?;
            crate::image::place(&tmp, &image)?;
        }
        let bins = outputs
            .iter()
            .map(|o| o.join("bin"))
            .filter(|b| b.is_dir())
            .map(|b| b.display().to_string())
            .collect();
        Ok(Extras { image, bins })
    }
}

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
    let (mut machine, step) = fork_at(home, run, step)?;
    let path = path.to_string();
    machine.request_inspection(&InspectRequest::Cat { pid, path }.args())?;

    let mut answer = Answer::default();
    let status = finish(&mut machine, step, &mut answer, "the file could be read")?;
    Ok(answer.into_inspection(status))
}

/// Process `pid` at `step`, or when None the process that was running
/// there, and its memory map: its pid on a line, then its /proc/<pid>/maps
/// as the VM's root sees it (see [`crate::maps`]). Not found when the
/// kernel was running, or when there is no process `pid`.
pub fn running(home: &Home, run: &Run, step: u64, pid: Option<u32>) -> Result<Inspection> {
    let (mut machine, step) = fork_at(home, run, step)?;
    machine.request_inspection(&InspectRequest::Running { pid }.args())?;

    let mut answer = Answer::default();
    let status = finish(
        &mut machine,
        step,
        &mut answer,
        "the memory map could be read",
    )?;
    Ok(answer.into_inspection(status))
}

/// Several files' contents at `step`, as process `pid` would have opened
/// them, in sections named by their paths (see
/// [`rewind_init::sections`]). Files that are not there are left out.
pub fn files(
    home: &Home,
    run: &Run,
    step: u64,
    pid: Option<u32>,
    paths: &[String],
) -> Result<Inspection> {
    let (mut machine, step) = fork_at(home, run, step)?;
    machine.request_inspection(&InspectRequest::Files { pid }.args())?;

    // The list goes in as typing: one path a line, then an empty line.
    let mut list = paths.join("\n");
    list.push_str("\n\n");
    machine.set_input(Box::new(Typed(Some(list.into_bytes()))));

    let mut answer = Answer::default();
    let status = finish(&mut machine, step, &mut answer, "the files could be read")?;
    Ok(answer.into_inspection(status))
}

/// Input typed all at once.
struct Typed(Option<Vec<u8>>);

impl Input for Typed {
    fn wait(&mut self, _timeout: Option<Duration>) -> Vec<u8> {
        self.0.take().unwrap_or_default()
    }
}

/// An interactive shell inside a fork of `run` at `step`, as `session`
/// describes: in a process's root and working directory, with its
/// environment, on a terminal of the session's size, with any extra
/// packages mounted. What the person types comes from `input`, and what
/// the shell prints goes to `console`. The rest of the VM is stopped where
/// it was. Returns when the shell exits.
pub fn shell(
    home: &Home,
    run: &Run,
    step: u64,
    session: Session,
    input: Box<dyn Input>,
    console: Console,
) -> Result<()> {
    let Session {
        pid,
        size: (cols, rows),
        extras,
    } = session;
    let (mut machine, step) = fork_at(home, run, step)?;
    if let Some(extras) = &extras {
        machine.attach_extras(&extras.image)?;
    }
    let request = InspectRequest::Shell {
        pid,
        size: (cols, rows),
        with: extras.map(|e| e.bins),
    };
    machine.request_inspection(&request.args())?;

    let mut answer = Answer {
        console: Some(console),
        ..Answer::default()
    };
    machine.set_input(Box::new(UntilDone {
        input,
        done: answer.done.clone(),
    }));
    match finish(&mut machine, step, &mut answer, "the shell started")? {
        InspectStatus::Done => Ok(()),
        _ => bail!("{}", String::from_utf8_lossy(&answer.stderr).trim()),
    }
}

/// A machine at `step` of `run`, ready for an inspection. After init
/// reports the job's exit, it only syncs and powers off, so nothing
/// changes, and once power off has begun nothing more can run in the VM:
/// a later step is moved back to that report.
fn fork_at(home: &Home, run: &Run, step: u64) -> Result<(Machine, u64)> {
    let exited_at = run.trace()?.job_exit().map(|exit| exit.step);
    let step = exited_at.map_or(step, |exited| step.min(exited));
    Ok((
        run.machine_at(home, step, Keep::Keyframe, &mut Ignore)?,
        step,
    ))
}

/// Runs the machine in chunks until the inspection's end mark arrives.
/// `what` finishes the sentence for when the VM stops first.
fn finish(
    machine: &mut Machine,
    step: u64,
    answer: &mut Answer,
    what: &str,
) -> Result<InspectStatus> {
    loop {
        let until = machine.step() + CHUNK_STEPS;
        let outcome = machine.run(Some(until), answer)?;
        if let Some(status) = answer.status {
            return Ok(status);
        }
        if let Outcome::Stopped(stop) = outcome {
            bail!("the VM {} before {what}", stopped(stop));
        }
        if answer.pid.is_none() && machine.step() >= step + START_WITHIN_STEPS {
            bail!("the VM did not start the inspection within {START_WITHIN_STEPS} steps");
        }
    }
}

/// A person's input that stops being waited for once the shell has exited,
/// so the machine can stop instead of waiting for keys nobody will press.
struct UntilDone {
    input: Box<dyn Input>,
    done: Rc<Cell<bool>>,
}

impl Input for UntilDone {
    fn wait(&mut self, timeout: Option<Duration>) -> Vec<u8> {
        if self.done.get() {
            return Vec::new();
        }
        self.input.wait(timeout)
    }
}

/// How a machine stopped, in words.
fn stopped(stop: Stop) -> &'static str {
    match stop {
        Stop::Guest(GuestExit::PowerOff) => "powered off",
        Stop::Guest(GuestExit::Restart) => "restarted",
        Stop::Guest(GuestExit::Halt) => "halted",
        Stop::TripleFault => "crashed",
        Stop::Stalled => "went idle for good",
        Stop::TimedOut(_) => "timed out",
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
    /// Where a shell's output goes, as it arrives.
    console: Option<Console>,
    /// Set with the status, for the input to stop waiting.
    done: Rc<Cell<bool>>,
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
            match Mark::parse(text) {
                Some(Mark::InspectBegin) => {
                    self.pid = Some(event.pid);
                    return;
                }
                Some(Mark::InspectEnd(status)) => {
                    if self.pid == Some(event.pid) {
                        self.status = Some(status);
                        self.done.set(true);
                    }
                    return;
                }
                _ => {}
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
                CONSOLE_FD => {
                    if let Some(console) = &mut self.console {
                        console(bytes);
                    }
                }
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

    fn mark(pid: u32, mark: Mark) -> Vec<u8> {
        record(KIND_MARK, pid, 0, mark.to_string().as_bytes())
    }

    #[test]
    fn collects_only_the_inspecting_process() {
        // A job keeps printing while pid 40 answers; the job's bytes stay out.
        let mut a = Answer::default();
        a.record(1, &record(KIND_OUTPUT, 7, 1, b"job before\n"));
        a.record(2, &mark(40, Mark::InspectBegin));
        a.record(3, &record(KIND_OUTPUT, 40, 1, b"hello "));
        a.record(4, &record(KIND_OUTPUT, 7, 1, b"job during\n"));
        a.record(5, &record(KIND_OUTPUT, 40, 1, b"world\n"));
        a.record(6, &mark(40, Mark::InspectEnd(InspectStatus::Done)));
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
        a.record(1, &mark(40, Mark::InspectBegin));
        a.record(
            2,
            &record(KIND_OUTPUT, 40, 2, b"/x: no such file at this step\n"),
        );
        a.record(3, &mark(40, Mark::InspectEnd(InspectStatus::NotFound)));
        let status = a.status.unwrap();
        assert_eq!(
            a.into_inspection(status),
            Inspection::NotFound("/x: no such file at this step".into())
        );
    }
}
