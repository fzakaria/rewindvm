//! `rewind where`: where in a program's own code a thread was at a step,
//! and where a run that timed out computing in user space had stopped.
//!
//! The thread's stack comes from gdb on a fork at the step, set up as
//! `rewind gdb` sets it up but without the kernel's symbols, which a
//! thread's user-space frames do not need. gdb runs a Python script of
//! Rewind's that walks the thread's frames and prints them as JSON; the
//! innermost that is the program's own code, not the C library's, Rust's
//! standard library's or a dependency's, is the answer.

use std::collections::{BTreeMap, BTreeSet};
use std::net::TcpListener;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use rewind_core::threads::OnTheCpu;
use rewind_core::{Home, Run};
use rewind_trace::Trace;
use rewind_trace::located::{Extent, Frame, Located, SourceFile};
use serde::Deserialize;

use crate::gdb::{self, Kernel, Needs, Output, Say, Symbols};

/// The gdb script, written to the session's directory for gdb to source,
/// and the start of the line it answers on.
const SCRIPT: &str = include_str!("locate.py");
const SCRIPT_NAME: &str = "where.py";
const ANSWER_MARKER: &str = "rewind-where: ";

/// The source lines around the chosen frame's line the text answer shows
/// a person.
const TEXT_SOURCE_RADIUS: u32 = 2;

/// How much of each source file `--json` carries for a program, such as
/// the desktop app, to scroll through: a file up to a mebibyte whole, and
/// of a larger one the lines from a few hundred above its frames' lines to
/// a few hundred below.
const JSON_LIMITS: Limits = Limits {
    max_whole: 1 << 20,
    radius: 200,
};

/// How `rewind where` answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Text,
    Json,
}

/// What the gdb script prints for a thread.
#[derive(Deserialize)]
struct Walked {
    frames: Option<Vec<Frame>>,
    error: Option<String>,
}

/// Prints where thread `tid` of process `pid` was at `step` of `run`:
/// by default the thread of the step's own event. `callers` is how many
/// frames that called the chosen one the text lists.
pub fn locate(
    home: &Home,
    run: &Run,
    step: u64,
    (pid, tid): (Option<u32>, Option<u32>),
    callers: usize,
    format: Format,
) -> Result<ExitCode> {
    let pick = thread_at(run.trace()?, step, pid, tid)?;
    let needs = Needs::Tasks("`rewind where` cannot find threads".into());
    let machine = gdb::fork(home, run, step, needs)?;
    let (pid, tid) = match pick {
        Pick::Thread { pid, tid } => (pid, tid),
        Pick::OnTheCpu => match gdb::on_the_cpu(&machine)? {
            Some(OnTheCpu::Thread { pid, tid }) => (pid, tid),
            Some(OnTheCpu::WithoutMemory { tid, name }) => bail!(
                "at step {step} the CPU ran {name} ({tid}) with no memory of its own: a kernel \
                 thread, or a process exiting; give --pid"
            ),
            Some(OnTheCpu::Idle) | None => {
                bail!("at step {step} the CPU was idle, in no process; give --pid")
            }
        },
    };
    let answer = walk(home, run, step, machine, pid, tid)?;
    match format {
        Format::Json => println!("{}", serde_json::to_string(&answer)?),
        Format::Text => {
            let whose = Whose {
                pid,
                process: &answer.process,
                tid,
                step,
            };
            let text_source = answer.chosen.and_then(|i| {
                let frame = &answer.frames[i];
                let file = answer.files.get(frame.fullname.as_ref()?)?;
                file.around(frame.line?, TEXT_SOURCE_RADIUS)
            });
            print!(
                "{}",
                render(
                    &whose,
                    &answer.frames,
                    answer.chosen,
                    text_source.as_ref(),
                    callers
                )
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Thread `tid`'s frames on `machine`, a fork of `run` at `step`, with
/// process `pid`'s symbols, and the one in the program's own code with its
/// source.
fn walk(
    home: &Home,
    run: &Run,
    step: u64,
    machine: rewind_vmm::Machine,
    pid: u32,
    tid: u32,
) -> Result<Located> {
    // The fork as gdb sees it, then the symbols, which take inspections on
    // forks of their own.
    let scope = rewind_core::debug::Scope::Process(pid);
    let mut debuggee = gdb::debuggee(run, step, machine, scope)?;
    let symbols = Symbols::load(home, run, step, Some(pid), Kernel::Skip, Say::Aloud)?;

    // gdb with the script, against the fork.
    let script = symbols.dir().join(SCRIPT_NAME);
    std::fs::write(&script, SCRIPT).context("writing the gdb script")?;
    let listener = TcpListener::bind(gdb::GDB_LOCAL).context("listening for gdb")?;
    let mut args = symbols.arguments(Some(listener.local_addr()?));
    args.extend([
        "-batch".to_string(),
        "-ex".to_string(),
        format!("set $rewind_tid = {tid}"),
        "-x".to_string(),
        script.display().to_string(),
    ]);
    eprintln!("rewind: walking thread {tid}'s stack in gdb");
    let names = symbols.file_names();
    let output = Output::CapturedSayingDownloads(&names);
    let (_, printed) = gdb::run_gdb(&args, Some((&mut debuggee, &listener)), output)?;
    let walked: Walked = script_answer(&printed)?;
    if let Some(error) = walked.error {
        bail!("gdb could not walk thread {tid}'s stack: {error}");
    }
    let mut frames = walked.frames.unwrap_or_default();

    // Each source file, read while the session's copies are here, then
    // every path, the files' own too, as the VM had it.
    let chosen = chosen(&frames);
    let files = source_files(&frames, &JSON_LIMITS, |path| {
        std::fs::read_to_string(path).ok()
    });
    let in_vm = |path: &String| {
        gdb::path_in_vm(std::path::Path::new(path), symbols.dir())
            .display()
            .to_string()
    };
    for frame in &mut frames {
        frame.fullname = frame.fullname.as_ref().map(in_vm);
        frame.object = frame.object.as_ref().map(in_vm);
    }
    let files = files
        .into_iter()
        .map(|(path, file)| (in_vm(&path), file))
        .collect();

    Ok(Located {
        run: run.manifest.id.to_string(),
        step,
        pid,
        tid,
        process: process_name(run.trace()?, pid, step),
        frames,
        chosen,
        files,
    })
}

/// What the gdb script prints for an address: the function it is in and
/// how far into it, its source line, and its program.
#[derive(Debug, Deserialize)]
struct Place {
    function: Option<String>,
    offset: Option<u64>,
    file: Option<String>,
    line: Option<u32>,
    object: Option<String>,
    error: Option<String>,
}

/// Where `address` is in the code process `pid` had mapped at `step` of
/// `run`. gdb reads the symbol files alone, with no fork to connect to.
fn place(home: &Home, run: &Run, step: u64, pid: u32, address: u64) -> Result<Place> {
    let symbols = Symbols::load(home, run, step, Some(pid), Kernel::Skip, Say::Nothing)?;
    let script = symbols.dir().join(SCRIPT_NAME);
    std::fs::write(&script, SCRIPT).context("writing the gdb script")?;
    let mut args = symbols.arguments(None);
    args.extend([
        "-batch".to_string(),
        "-ex".to_string(),
        format!("set $rewind_address = {address:#x}"),
        "-x".to_string(),
        script.display().to_string(),
    ]);
    let (_, printed) = gdb::run_gdb(&args, None, Output::Captured)?;
    let place: Place = script_answer(&printed)?;
    if let Some(error) = place.error {
        bail!("gdb could not place {address:#x}: {error}");
    }
    Ok(place)
}

/// How sure a timeout's message is of the process it names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Certainty {
    /// The VM's kernel had its thread on the CPU when the run stopped.
    OnTheCpu,
    /// It made the run's last event; the kernel did not say.
    LastEvent,
}

/// How a run that timed out computing in user space stopped, with the
/// instruction named by its function, offset and source line, and the
/// process it was in. None when the run did not stop that way or the
/// instruction has no symbol, and the run's own words stand.
pub fn describe_stall(home: &Home, run: &Run) -> Option<String> {
    let timeout = run.manifest.outcome.as_ref()?.stop.timeout()?;
    let rewind_trace::stop::Doing::User { rip, thread } = &timeout.doing else {
        return None;
    };
    let trace = run.trace().ok()?;

    // The process: the thread on the CPU, else the last event's.
    let (pid, name, certainty) = match thread {
        Some(thread) => (thread.pid, thread.name.clone(), Certainty::OnTheCpu),
        None => {
            let event = trace.events.iter().rev().find(|e| e.pid != 0)?;
            let name = process_name(trace, event.pid, event.step);
            (event.pid, name, Certainty::LastEvent)
        }
    };

    // The instruction, with the process's symbols as of the last step:
    // the address is reached after it, but the map rarely changes then.
    let place = place(home, run, trace.last_step(), pid, *rip).ok()?;
    let words = place_words(&place)?;
    Some(format!(
        "{}, {}",
        timeout.in_user_space(&words),
        process_words(pid, &name, certainty)
    ))
}

/// Where store paths are, and what ends the hash that starts a store
/// path's name.
const NIX_STORE: &str = "/nix/store/";
const STORE_HASH_END: char = '-';

/// A file's name for a one-line message: the last part of its path, and
/// for a file that is a store path itself, its name without the hash.
fn file_name(path: &str) -> &str {
    if let Some(name) = path.strip_prefix(NIX_STORE).filter(|n| !n.contains('/')) {
        return name.split_once(STORE_HASH_END).map_or(name, |(_, n)| n);
    }
    path.rsplit('/').next().unwrap_or(path)
}

/// An instruction's place in words: its function and offset, then its
/// source file's name and line, else its program's file name. None
/// without a function.
fn place_words(place: &Place) -> Option<String> {
    let function = place.function.as_ref()?;
    let at = match place.offset {
        Some(offset) if offset > 0 => format!("{function}+{offset}"),
        _ => function.clone(),
    };
    let detail = match (&place.file, place.line, &place.object) {
        (Some(file), Some(line), _) => Some(format!("{}:{line}", file_name(file))),
        (_, _, Some(object)) => Some(file_name(object).to_string()),
        _ => None,
    };
    Some(match detail {
        Some(detail) => format!("in {at} ({detail})"),
        None => format!("in {at}"),
    })
}

/// The process a run stalled in, in words.
fn process_words(pid: u32, name: &str, certainty: Certainty) -> String {
    match certainty {
        Certainty::OnTheCpu => format!("process {pid} ({name})"),
        Certainty::LastEvent => {
            format!("probably process {pid} ({name}), which made the run's last event")
        }
    }
}

/// What a gdb built without Python says when asked to run Python, as
/// sourcing the script or loading the process's files does.
const NO_PYTHON: &str = "Python scripting is not supported in this copy of GDB";

/// The JSON the gdb script printed on its marked line. Without one, what
/// gdb said on its standard error explains why: most plainly, that it
/// cannot run the script at all.
fn script_answer<T: serde::de::DeserializeOwned>(printed: &gdb::Printed) -> Result<T> {
    let Some(line) = printed
        .stdout
        .lines()
        .find_map(|l| l.strip_prefix(ANSWER_MARKER))
    else {
        if printed.stderr.contains(NO_PYTHON) {
            bail!("the gdb on PATH cannot run Python scripts; use a gdb built with Python");
        }
        bail!("gdb's script gave no answer: {}", printed.stderr.trim());
    };
    serde_json::from_str(line).context("reading the gdb script's answer")
}

/// The thread a question about a step is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pick {
    Thread {
        pid: u32,
        tid: u32,
    },
    /// Whichever thread the CPU ran, which only a fork at the step can
    /// read.
    OnTheCpu,
}

/// The process and thread `rewind where` and `rewind gdb` look at: those
/// given, else those of the event at `step`. A process given alone is
/// looked at in the event's thread when the event is that process's, else
/// in its main thread; a thread given alone is in the process the trace
/// saw it in. At a step with no event, or with the kernel's own, such as a
/// console line, it is the thread on the CPU.
pub fn thread_at(trace: &Trace, step: u64, pid: Option<u32>, tid: Option<u32>) -> Result<Pick> {
    let event = trace.events.iter().find(|e| e.step == step);
    match (pid, tid) {
        (Some(pid), Some(tid)) => Ok(Pick::Thread { pid, tid }),
        (Some(pid), None) => {
            let tid = event.filter(|e| e.pid == pid).map_or(pid, |e| e.tid);
            Ok(Pick::Thread { pid, tid })
        }
        (None, Some(tid)) => {
            // The thread's latest event by the step, else its first after:
            // a thread id is given out again once its thread has ended.
            let by_step = trace.until(step).iter().rev().find(|e| e.tid == tid);
            let seen = by_step.or_else(|| trace.events.iter().find(|e| e.tid == tid));
            let Some(seen) = seen else {
                bail!("the run has no thread {tid}; give --pid with --tid");
            };
            Ok(Pick::Thread { pid: seen.pid, tid })
        }
        (None, None) => match event {
            Some(event) if event.pid != 0 => Ok(Pick::Thread {
                pid: event.pid,
                tid: event.tid,
            }),
            _ => Ok(Pick::OnTheCpu),
        },
    }
}

/// The name of the process with id `pid` at `step`: the file name of the
/// program it last ran.
fn process_name(trace: &Trace, pid: u32, step: u64) -> String {
    let process = trace.process_at(pid, step);
    let name = process.map_or_else(|| format!("pid {pid}"), |p| p.name());
    name.rsplit('/').next().unwrap_or(&name).to_string()
}

/// Where x86-64's lower half, user space, ends: an instruction at or
/// above it is the kernel's.
const USER_END: u64 = 0x0000_8000_0000_0000;

/// The start of the file names of the dynamic loader and of the C
/// library's, whose frames are not the program's own code.
const SYSTEM_LIBRARIES: &[&str] = &["ld-linux", "ld-musl", "libc.so", "libc-", "libpthread"];

/// Where the DWARF of code that is not the program's own puts its
/// sources: Rust's standard library, and crates that Nix vendors or Cargo
/// fetches.
const LIBRARY_SOURCES: &[&str] = &[
    "/rustc/",
    "/cargo-vendor-dir/",
    "/.cargo/registry/",
    "/.cargo/git/",
];

/// What frame choice asks of a frame.
trait Choice {
    fn address(&self) -> Option<u64>;
    fn in_user_space(&self) -> bool;
    fn in_system_library(&self) -> bool;
    fn in_library_source(&self) -> bool;
    fn own_code(&self) -> bool;
}

impl Choice for Frame {
    /// The instruction's address.
    fn address(&self) -> Option<u64> {
        u64::from_str_radix(self.pc.trim_start_matches("0x"), 16).ok()
    }

    fn in_user_space(&self) -> bool {
        self.address().is_some_and(|a| a < USER_END)
    }

    /// Whether the frame is in the dynamic loader or the C library.
    fn in_system_library(&self) -> bool {
        let Some(object) = &self.object else {
            return false;
        };
        let name = object.rsplit('/').next().unwrap_or(object);
        SYSTEM_LIBRARIES.iter().any(|lib| name.starts_with(lib))
    }

    /// Whether the frame's source is a library's, by the path its DWARF or
    /// this machine gives it.
    fn in_library_source(&self) -> bool {
        [&self.file, &self.fullname]
            .into_iter()
            .flatten()
            .any(|path| LIBRARY_SOURCES.iter().any(|dir| path.contains(dir)))
    }

    /// Whether the frame is the program's own code: user space, with a
    /// source line, outside the system's libraries and other code's
    /// sources.
    fn own_code(&self) -> bool {
        self.in_user_space()
            && self.file.is_some()
            && self.line.is_some()
            && !self.in_system_library()
            && !self.in_library_source()
    }
}

/// The innermost frame in the program's own code, else the innermost in
/// user space with a name.
pub fn chosen(frames: &[Frame]) -> Option<usize> {
    frames.iter().position(Frame::own_code).or_else(|| {
        frames
            .iter()
            .position(|f| f.in_user_space() && f.function.is_some())
    })
}

/// How much of a source file `--json` carries: files up to `max_whole`
/// bytes whole, larger ones as the lines within `radius` of their
/// frames' lines.
struct Limits {
    max_whole: usize,
    radius: u32,
}

/// The source files `frames` are in, by their paths here, as `read` gives
/// them: whole, or a window around the frames' lines for a file over the
/// limit. A frame without a path and a line adds none, nor does one whose
/// file `read` cannot give. Each file is read once, however many frames
/// are in it.
fn source_files(
    frames: &[Frame],
    limits: &Limits,
    mut read: impl FnMut(&str) -> Option<String>,
) -> BTreeMap<String, SourceFile> {
    // The lines each file's frames are on.
    let mut lines: BTreeMap<&str, BTreeSet<u32>> = BTreeMap::new();
    for frame in frames {
        let (Some(path), Some(line)) = (frame.fullname.as_deref(), frame.line) else {
            continue;
        };
        lines.entry(path).or_default().insert(line);
    }

    // Each file whole when it is small enough, else the lines around its
    // frames' lines.
    let mut files = BTreeMap::new();
    for (path, on) in lines {
        let Some(text) = read(path) else {
            continue;
        };
        let file = if text.len() <= limits.max_whole {
            Some(SourceFile::whole(&text))
        } else {
            SourceFile::window(&text, &on, limits.radius)
        };
        if let Some(file) = file {
            files.insert(path.to_string(), file);
        }
    }
    files
}

/// What the text answer and the JSON one take of a source file.
trait Carry {
    fn window(text: &str, on: &BTreeSet<u32>, radius: u32) -> Option<SourceFile>;
    fn around(&self, line: u32, radius: u32) -> Option<Source>;
}

impl Carry for SourceFile {
    /// The lines of `text` from `radius` above the first of `on` to
    /// `radius` below the last, cut at the file's ends. None when every
    /// line of `on` is past the file's end.
    fn window(text: &str, on: &BTreeSet<u32>, radius: u32) -> Option<SourceFile> {
        let count = u32::try_from(text.lines().count()).ok()?;
        let lowest = *on.iter().find(|&&line| line >= 1 && line <= count)?;
        let highest = *on.range(..=count).next_back()?;
        let first = lowest.saturating_sub(radius).max(1);
        let last = highest.saturating_add(radius).min(count);
        let lines: Vec<&str> = text
            .lines()
            .skip((first - 1) as usize)
            .take((last - first + 1) as usize)
            .collect();
        Some(SourceFile {
            extent: Extent::Window,
            first,
            text: lines.join("\n"),
        })
    }

    /// The lines within `radius` of line `line`, counted from 1, cut to
    /// what the file carries. None when it does not carry that line.
    fn around(&self, line: u32, radius: u32) -> Option<Source> {
        let count = u32::try_from(self.text.lines().count()).ok()?;
        let end = self.first.checked_add(count)?;
        if line < self.first || line >= end {
            return None;
        }
        let first = line.saturating_sub(radius).max(self.first);
        let last = line.saturating_add(radius).min(end - 1);
        let lines = self
            .text
            .lines()
            .skip((first - self.first) as usize)
            .take((last - first + 1) as usize)
            .map(String::from)
            .collect();
        Some(Source { first, lines })
    }
}

/// Lines of a source file around one, as the text answer shows them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    /// The number of the first line.
    pub first: u32,
    pub lines: Vec<String>,
}

/// Whose stack it was, for the header line.
pub struct Whose<'a> {
    pub pid: u32,
    pub process: &'a str,
    pub tid: u32,
    pub step: u64,
}

/// `rewind where`'s answer in words: a header, the chosen frame with its
/// source, and up to `callers` frames it was called from.
pub fn render(
    whose: &Whose,
    frames: &[Frame],
    chosen: Option<usize>,
    source: Option<&Source>,
    callers: usize,
) -> String {
    let mut out = format!(
        "process {} ({}), thread {}, at step {}\n",
        whose.pid, whose.process, whose.tid, whose.step
    );

    // Without a frame to show, every frame there was.
    let Some(at) = chosen.filter(|&i| i < frames.len()) else {
        out.push_str("no frame names a function; the stack was:\n");
        for frame in frames {
            out.push_str(&format!("{}\n", frame_line(frame)));
        }
        return out;
    };

    // The frame, then its source with the frame's line marked.
    let frame = &frames[at];
    out.push_str(&format!("{}\n", frame_line(frame)));
    if let Some(source) = source {
        for (number, text) in (source.first..).zip(&source.lines) {
            let marker = if Some(number) == frame.line {
                LINE_MARKER
            } else {
                NO_MARKER
            };
            let shown = format!("{marker} {number:>6}  {text}");
            out.push_str(shown.trim_end());
            out.push('\n');
        }
    }

    // The frames that called it.
    for caller in frames.iter().skip(at + 1).take(callers) {
        out.push_str(&format!("called from {}\n", frame_line(caller)));
    }
    out
}

/// What marks the frame's line among its source lines, and the others.
const LINE_MARKER: &str = ">";
const NO_MARKER: &str = " ";

/// A frame by its level and function, and its source line, else its
/// program, else its address alone.
fn frame_line(frame: &Frame) -> String {
    let place = match (&frame.file, frame.line, &frame.object) {
        (Some(file), Some(line), _) => Some(format!("{file}:{line}")),
        (_, _, Some(object)) => Some(object.clone()),
        _ => None,
    };
    match (&frame.function, place) {
        (Some(function), Some(place)) => format!("#{} {function} ({place})", frame.level),
        (Some(function), None) => format!("#{} {function} at {}", frame.level, frame.pc),
        (None, Some(place)) => format!("#{} {} ({place})", frame.level, frame.pc),
        (None, None) => format!("#{} {}", frame.level, frame.pc),
    }
}

#[cfg(test)]
mod tests {
    // Frame choice and the text answer, from frame lists as the gdb script
    // prints them for a C program in glibc, a Rust program with its
    // dependencies, and a stack with nothing of the program's own.
    use super::*;

    /// A frame list as JSON, the way the gdb script prints it.
    fn frames(json: &str) -> Vec<Frame> {
        serde_json::from_str(json).unwrap()
    }

    const IN_GLIBC: &str = r#"[
        {"level":0,"function":"__futex_abstimed_wait_common64","file":"./nptl/futex-internal.c","fullname":"/build/glibc-2.40/nptl/futex-internal.c","line":57,"pc":"0x7f61584f2b26","object":"/nix/store/bbb-glibc-2.40/lib/libc.so.6"},
        {"level":1,"function":"pthread_cond_wait","file":"./nptl/pthread_cond_wait.c","fullname":null,"line":618,"pc":"0x7f61584f53d0","object":"/nix/store/bbb-glibc-2.40/lib/libc.so.6"},
        {"level":2,"function":"worker","file":"src/pool.c","fullname":"/home/me/.local/share/rewind/gdb/42/build/mylib/src/pool.c","line":70,"pc":"0x55bfaf4373e1","object":"/build/mylib/tests/test_pool_shutdown"},
        {"level":3,"function":"start_thread","file":"./nptl/pthread_create.c","fullname":null,"line":448,"pc":"0x7f61584f61d3","object":"/nix/store/bbb-glibc-2.40/lib/libc.so.6"},
        {"level":4,"function":"__clone3","file":null,"fullname":null,"line":null,"pc":"0x7f615857e5bc","object":"/nix/store/bbb-glibc-2.40/lib/libc.so.6"}
    ]"#;

    /// In a C program, frames in glibc are passed over for the program's
    /// own function that called into it.
    #[test]
    fn glibc_frames_are_passed_over() {
        assert_eq!(chosen(&frames(IN_GLIBC)), Some(2));
    }

    /// In a Rust program, the standard library, which DWARF places under
    /// /rustc/, and vendored or registry crates are passed over.
    #[test]
    fn rust_s_library_and_dependencies_are_passed_over() {
        let stack = frames(
            r#"[
            {"level":0,"function":"std::sys::pal::unix::futex::futex_wait","file":"/rustc/48a229ce/library/std/src/sys/pal/unix/futex.rs","fullname":null,"line":72,"pc":"0x55e0000010a0","object":"/build/source/target/release/casita"},
            {"level":1,"function":"tokio::runtime::park::Inner::park","file":"/build/cargo-vendor-dir/tokio-1.47.0/src/runtime/park.rs","fullname":null,"line":120,"pc":"0x55e0000020b0","object":"/build/source/target/release/casita"},
            {"level":2,"function":"serde_json::de::from_str","file":"/home/me/.cargo/registry/src/index.crates.io-6f17d22bba15001f/serde_json-1.0.140/src/de.rs","fullname":null,"line":2,"pc":"0x55e0000030c0","object":"/build/source/target/release/casita"},
            {"level":3,"function":"casita::main","file":"src/main.rs","fullname":"/build/source/src/main.rs","line":31,"pc":"0x55e0000040d0","object":"/build/source/target/release/casita"}
        ]"#,
        );
        assert_eq!(chosen(&stack), Some(3));
    }

    /// With none of the program's own code on the stack, the innermost
    /// user-space frame with a name is chosen; kernel frames never are,
    /// and an empty stack has none.
    #[test]
    fn without_the_program_s_code_the_innermost_named_user_frame_is_chosen() {
        let stack = frames(
            r#"[
            {"level":0,"function":"rewind_emit","file":"arch/x86/kernel/cpu/rewind.c","fullname":null,"line":230,"pc":"0xffffffff81285085","object":"/nix/store/aaa-kernel/vmlinux"},
            {"level":1,"function":null,"file":null,"fullname":null,"line":null,"pc":"0x401913","object":null},
            {"level":2,"function":"__syscall_cp_c","file":null,"fullname":null,"line":null,"pc":"0x401920","object":"/newroot/src/big"},
            {"level":3,"function":"write","file":null,"fullname":null,"line":null,"pc":"0x40155c","object":"/newroot/src/big"}
        ]"#,
        );
        assert_eq!(chosen(&stack), Some(2));
        assert_eq!(chosen(&[]), None);
    }

    /// The thread looked at: the step's event's by default, a process
    /// given alone in the event's thread when the event is its own and in
    /// its main thread otherwise, and a thread given alone in the process
    /// the trace saw it in last by the step, as thread ids are given out
    /// again. At a step with no event, or the kernel's, it is the thread on
    /// the CPU. Builds a trace of a few writes by hand.
    #[test]
    fn the_thread_is_the_event_s_unless_given() {
        let write = |step, pid, tid| rewind_trace::Event {
            step,
            pid,
            tid,
            kind: rewind_trace::EventKind::Output {
                fd: 1,
                bytes: b"x".to_vec(),
            },
        };
        let trace = Trace {
            events: vec![write(10, 166, 168), write(12, 0, 0), write(14, 170, 170)],
        };
        let thread = |pid, tid| Pick::Thread { pid, tid };
        assert_eq!(thread_at(&trace, 10, None, None).unwrap(), thread(166, 168));
        assert_eq!(
            thread_at(&trace, 10, Some(166), None).unwrap(),
            thread(166, 168)
        );
        assert_eq!(
            thread_at(&trace, 10, Some(170), None).unwrap(),
            thread(170, 170)
        );
        assert_eq!(
            thread_at(&trace, 14, None, Some(168)).unwrap(),
            thread(166, 168)
        );
        assert_eq!(
            thread_at(&trace, 11, Some(166), Some(167)).unwrap(),
            thread(166, 167)
        );
        assert_eq!(thread_at(&trace, 11, None, None).unwrap(), Pick::OnTheCpu);
        assert_eq!(thread_at(&trace, 12, None, None).unwrap(), Pick::OnTheCpu);
        assert!(thread_at(&trace, 10, None, Some(999)).is_err());

        // Thread 168 again later, in process 300, after 166's thread 168
        // had ended: a step after it means process 300's.
        let reused = Trace {
            events: vec![write(10, 166, 168), write(20, 300, 168)],
        };
        assert_eq!(
            thread_at(&reused, 15, None, Some(168)).unwrap(),
            thread(166, 168)
        );
        assert_eq!(
            thread_at(&reused, 25, None, Some(168)).unwrap(),
            thread(300, 168)
        );
    }

    /// A stopped instruction in words: its function and offset, then its
    /// source file's name and line, else its program's file name; without
    /// a function, no words. A source file that is a store path loses its
    /// hash. Places as the gdb script prints them for an address.
    #[test]
    fn a_place_names_the_function_and_the_line() {
        let place = |json: &str| -> Place { serde_json::from_str(json).unwrap() };
        let spin = place(
            r#"{"address":"0x558577205154","function":"spin","offset":11,"file":"/nix/store/lm3yyssrgs19ph1hc8fsh6lhq0473x8r-spin.c","fullname":null,"line":5,"object":"/nix/store/aaa-spin-bin/bin/spin"}"#,
        );
        assert_eq!(place_words(&spin).as_deref(), Some("in spin+11 (spin.c:5)"));
        let pool = place(
            r#"{"address":"0x55bfaf437437","function":"worker","offset":0,"file":"src/pool.c","fullname":null,"line":77,"object":null}"#,
        );
        assert_eq!(place_words(&pool).as_deref(), Some("in worker (pool.c:77)"));
        let stripped = place(
            r#"{"address":"0x401913","function":"__syscall_cp_c","offset":0,"file":null,"fullname":null,"line":null,"object":"/newroot/src/big"}"#,
        );
        assert_eq!(
            place_words(&stripped).as_deref(),
            Some("in __syscall_cp_c (big)")
        );
        let unknown = place(
            r#"{"address":"0x1000","function":null,"offset":null,"file":null,"fullname":null,"line":null,"object":null}"#,
        );
        assert_eq!(place_words(&unknown), None);
    }

    /// A gdb built without Python cannot run the script, and says so on
    /// its standard error; the answer names the cause and the fix. Any
    /// other failure to answer passes gdb's words on.
    #[test]
    fn a_gdb_without_python_is_named_as_the_cause() {
        let printed = |stderr: &str| gdb::Printed {
            stdout: String::new(),
            stderr: stderr.into(),
        };
        let without = printed(
            "/tmp/gdb/42/where.py:1: Error in sourced command file:\n\
             Python scripting is not supported in this copy of GDB.\n",
        );
        let err = script_answer::<Place>(&without).unwrap_err().to_string();
        assert_eq!(
            err,
            "the gdb on PATH cannot run Python scripts; use a gdb built with Python"
        );
        let other = script_answer::<Place>(&printed("Remote connection closed")).unwrap_err();
        assert_eq!(
            other.to_string(),
            "gdb's script gave no answer: Remote connection closed"
        );
    }

    /// The process a run stalled in: the one the VM's kernel had on the
    /// CPU, by its name there, or a guess from the run's last event.
    #[test]
    fn the_stalled_process_is_named_or_guessed() {
        assert_eq!(
            process_words(38, "spin", Certainty::OnTheCpu),
            "process 38 (spin)"
        );
        assert_eq!(
            process_words(38, "spin", Certainty::LastEvent),
            "probably process 38 (spin), which made the run's last event"
        );
    }

    /// Source lines around a line, cut at the start and the end of the
    /// file, and none for a line past the end. A window of a file counts
    /// its lines from its first, and has none outside it. Builds whole
    /// files and a window by hand.
    #[test]
    fn source_lines_are_cut_at_either_end_of_the_file() {
        let text = "a\nb\nc\nd\ne\nf\n";
        let whole = SourceFile::whole(text);
        let lines = |first: u32, lines: &[&str]| {
            Some(Source {
                first,
                lines: lines.iter().map(|l| l.to_string()).collect(),
            })
        };
        assert_eq!(whole.around(3, 2), lines(1, &["a", "b", "c", "d", "e"]));
        assert_eq!(whole.around(6, 2), lines(4, &["d", "e", "f"]));
        assert_eq!(whole.around(7, 2), None);
        assert_eq!(whole.around(0, 2), None);

        let window = SourceFile {
            extent: Extent::Window,
            first: 10,
            text: "j\nk\nl".to_string(),
        };
        assert_eq!(window.around(11, 1), lines(10, &["j", "k", "l"]));
        assert_eq!(window.around(12, 5), lines(10, &["j", "k", "l"]));
        assert_eq!(window.around(9, 1), None);
        assert_eq!(window.around(13, 1), None);
    }

    /// Each source file the frames are in is carried once, whole, keyed
    /// by the path the frames name it by, and read once however many
    /// frames are in it. A frame without a line, or whose file cannot be
    /// read, adds none. Frames as the gdb script lists them, over files
    /// held in a map.
    #[test]
    fn each_file_is_carried_once_and_whole() {
        let stack = frames(
            r#"[
            {"level":0,"function":"run_job","file":"src/pool.c","fullname":"/s/pool.c","line":2,"pc":"0x1","object":null},
            {"level":1,"function":"worker","file":"src/pool.c","fullname":"/s/pool.c","line":5,"pc":"0x2","object":null},
            {"level":2,"function":"start_thread","file":"pthread_create.c","fullname":"/gone.c","line":9,"pc":"0x3","object":null},
            {"level":3,"function":"__clone3","file":null,"fullname":null,"line":null,"pc":"0x4","object":"/lib/libc.so.6"}
        ]"#,
        );
        let mut reads = 0;
        let files = source_files(&stack, &JSON_LIMITS, |path| {
            reads += 1;
            (path == "/s/pool.c").then(|| "a\nb\nc\nd\ne\nf\n".to_string())
        });
        let expected = BTreeMap::from([(
            "/s/pool.c".to_string(),
            SourceFile {
                extent: Extent::Whole,
                first: 1,
                text: "a\nb\nc\nd\ne\nf\n".to_string(),
            },
        )]);
        assert_eq!(files, expected);
        assert_eq!(reads, 2);
    }

    /// A file larger than the cap is carried as the lines from a radius
    /// above its first frame's line to a radius below its last, cut at
    /// the file's ends; a file whose frames' lines are all past its end
    /// is not carried. Twenty numbered lines under a ten-byte cap.
    #[test]
    fn a_large_file_is_carried_as_a_window_around_its_frames_lines() {
        let stack = frames(
            r#"[
            {"level":0,"function":"inner","file":"big.c","fullname":"/s/big.c","line":10,"pc":"0x1","object":null},
            {"level":1,"function":"outer","file":"big.c","fullname":"/s/big.c","line":8,"pc":"0x2","object":null},
            {"level":2,"function":"main","file":"edge.c","fullname":"/s/edge.c","line":19,"pc":"0x3","object":null},
            {"level":3,"function":"past","file":"short.c","fullname":"/s/short.c","line":40,"pc":"0x4","object":null}
        ]"#,
        );
        let numbered: String = (1..=20).map(|n| format!("{n}\n")).collect();
        let limits = Limits {
            max_whole: 10,
            radius: 2,
        };
        let files = source_files(&stack, &limits, |_| Some(numbered.clone()));
        let window = |first: u32, text: &str| SourceFile {
            extent: Extent::Window,
            first,
            text: text.to_string(),
        };
        assert_eq!(files["/s/big.c"], window(6, "6\n7\n8\n9\n10\n11\n12"));
        assert_eq!(files["/s/edge.c"], window(17, "17\n18\n19\n20"));
        assert!(!files.contains_key("/s/short.c"));
    }

    /// A carried file reads as its extent, its first line and its text,
    /// the extent in lower case, so the JSON says which files are whole.
    #[test]
    fn a_carried_file_says_whether_it_is_whole() {
        let whole = serde_json::to_string(&SourceFile::whole("x\n")).unwrap();
        assert_eq!(whole, r#"{"extent":"whole","first":1,"text":"x\n"}"#);
        let window = SourceFile {
            extent: Extent::Window,
            first: 7,
            text: "y".to_string(),
        };
        assert_eq!(
            serde_json::to_string(&window).unwrap(),
            r#"{"extent":"window","first":7,"text":"y"}"#
        );
    }

    /// The text answer: who and when, the chosen frame by its level with
    /// its source and the line marked, then its callers up to the limit,
    /// each by its source line or, without one, its program.
    #[test]
    fn the_answer_names_the_frame_its_source_and_its_callers() {
        let stack = frames(IN_GLIBC);
        let whose = Whose {
            pid: 166,
            process: "test_pool_shutdown",
            tid: 168,
            step: 5017,
        };
        let source = Source {
            first: 68,
            lines: [
                "\tfor (;;) {",
                "\t\twhile (p->pending == 0 && !p->stopping)",
                "\t\t\tpthread_cond_wait(&p->cond, &p->lock);",
                "\t\tif (!p->stopping) {",
                "\t\t\tjob = take(p->queue);",
            ]
            .map(String::from)
            .to_vec(),
        };
        let text = render(&whose, &stack, Some(2), Some(&source), 2);
        assert_eq!(
            text,
            "process 166 (test_pool_shutdown), thread 168, at step 5017\n\
             #2 worker (src/pool.c:70)\n\
             \x20     68  \tfor (;;) {\n\
             \x20     69  \t\twhile (p->pending == 0 && !p->stopping)\n\
             >     70  \t\t\tpthread_cond_wait(&p->cond, &p->lock);\n\
             \x20     71  \t\tif (!p->stopping) {\n\
             \x20     72  \t\t\tjob = take(p->queue);\n\
             called from #3 start_thread (./nptl/pthread_create.c:448)\n\
             called from #4 __clone3 (/nix/store/bbb-glibc-2.40/lib/libc.so.6)\n"
        );
    }

    /// Without a chosen frame, the answer says so and lists the frames it
    /// had.
    #[test]
    fn an_answer_without_a_frame_lists_what_there_was() {
        let whose = Whose {
            pid: 34,
            process: "big",
            tid: 34,
            step: 310,
        };
        let stack = frames(
            r#"[{"level":0,"function":null,"file":null,"fullname":null,"line":null,"pc":"0x401913","object":null}]"#,
        );
        assert_eq!(
            render(&whose, &stack, None, None, 2),
            "process 34 (big), thread 34, at step 310\n\
             no frame names a function; the stack was:\n\
             #0 0x401913\n"
        );
    }
}
