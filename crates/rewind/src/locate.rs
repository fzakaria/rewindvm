//! `rewind where`: where in a program's own code a thread was at a step,
//! and where a run that timed out computing in user space had stopped.
//!
//! The thread's stack comes from gdb on a fork at the step, set up as
//! `rewind gdb` sets it up but without the kernel's symbols, which a
//! thread's user-space frames do not need. gdb runs a Python script of
//! Rewind's that walks the thread's frames and prints them as JSON; the
//! innermost that is the program's own code, not the C library's, Rust's
//! standard library's or a dependency's, is the answer.

use std::collections::HashMap;
use std::net::TcpListener;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use rewind_core::{Home, Run};
use rewind_trace::Trace;
use serde::{Deserialize, Serialize};

use crate::gdb::{self, Kernel, Needs, Output, Say, Symbols};

/// The gdb script, written to the session's directory for gdb to source,
/// and the start of the line it answers on.
const SCRIPT: &str = include_str!("locate.py");
const SCRIPT_NAME: &str = "where.py";
const ANSWER_MARKER: &str = "rewind-where: ";

/// The source lines around a frame's line: a few around the chosen
/// frame's for a person to read, more around every frame's for a program,
/// such as the desktop app, to scroll through.
const TEXT_SOURCE_RADIUS: u32 = 2;
const JSON_SOURCE_RADIUS: u32 = 40;

/// How `rewind where` answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Text,
    Json,
}

/// What `rewind where` prints with `--json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    pub run: String,
    pub step: u64,
    pub pid: u32,
    pub tid: u32,
    pub process: String,
    /// The thread's frames, innermost first, and the one in the program's
    /// own code.
    pub frames: Vec<Frame>,
    pub chosen: Option<usize>,
    /// Each frame's source lines around its line, in the order of
    /// `frames`: null for a frame without a source file and line, or whose
    /// file was not found.
    pub sources: Vec<Option<Source>>,
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
    let (pid, tid) = thread_at(run.trace()?, step, pid, tid)?;
    let answer = walk(home, run, step, pid, tid)?;
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
                let source = answer.sources.get(i)?.as_ref()?;
                narrowed(source, answer.frames[i].line?, TEXT_SOURCE_RADIUS)
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

/// Thread `tid`'s frames at `step`, with process `pid`'s symbols, and the
/// one in the program's own code with its source.
pub fn walk(home: &Home, run: &Run, step: u64, pid: u32, tid: u32) -> Result<Answer> {
    // The fork, refused first for a run whose kernel lists no tasks, then
    // the symbols, which take inspections on forks of their own.
    let needs = Needs::Tasks(format!("`rewind where` cannot find thread {tid}"));
    let scope = rewind_core::debug::Scope::Process(pid);
    let mut debuggee = gdb::fork(home, run, step, scope, needs)?;
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

    // Each frame's source, read while the session's copies are here,
    // then every path as the VM had it.
    let chosen = chosen(&frames);
    let sources = frame_sources(&frames, JSON_SOURCE_RADIUS, |path| {
        std::fs::read_to_string(path).ok()
    });
    for frame in &mut frames {
        let in_vm = |path: &String| {
            gdb::path_in_vm(std::path::Path::new(path), symbols.dir())
                .display()
                .to_string()
        };
        frame.fullname = frame.fullname.as_ref().map(in_vm);
        frame.object = frame.object.as_ref().map(in_vm);
    }

    Ok(Answer {
        run: run.manifest.id.clone(),
        step,
        pid,
        tid,
        process: process_name(run.trace()?, pid),
        frames,
        chosen,
        sources,
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
    let stalled = run.stalled.as_ref()?;
    let trace = run.trace().ok()?;

    // The process: the thread on the CPU, else the last event's.
    let (pid, name, certainty) = match &stalled.thread {
        Some(thread) => (thread.pid, thread.name.clone(), Certainty::OnTheCpu),
        None => {
            let event = trace.events.iter().rev().find(|e| e.pid != 0)?;
            let name = process_name(trace, event.pid);
            (event.pid, name, Certainty::LastEvent)
        }
    };

    // The instruction, with the process's symbols as of the last step:
    // the address is reached after it, but the map rarely changes then.
    let place = place(home, run, trace.last_step(), pid, stalled.stall.rip).ok()?;
    let words = place_words(&place)?;
    Some(format!(
        "{}, {}",
        rewind_core::run::describe_user_stall(&stalled.stall, &words),
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

/// The process and thread `rewind where` looks at: those given, else those
/// of the event at `step`. A process given alone is looked at in the
/// event's thread when the event is that process's, else in its main
/// thread; a thread given alone is in the process the trace saw it in.
fn thread_at(trace: &Trace, step: u64, pid: Option<u32>, tid: Option<u32>) -> Result<(u32, u32)> {
    let event = trace.events.iter().find(|e| e.step == step);
    match (pid, tid) {
        (Some(pid), Some(tid)) => Ok((pid, tid)),
        (Some(pid), None) => {
            let tid = event.filter(|e| e.pid == pid).map_or(pid, |e| e.tid);
            Ok((pid, tid))
        }
        (None, Some(tid)) => {
            let Some(seen) = trace.events.iter().find(|e| e.tid == tid) else {
                bail!("the run has no thread {tid}; give --pid with --tid");
            };
            Ok((seen.pid, tid))
        }
        (None, None) => {
            let Some(event) = event else {
                bail!("no event at step {step}; give --pid, or a step `rewind events` lists");
            };
            if event.pid == 0 {
                bail!("step {step} ran in the kernel, in no process; give --pid");
            }
            Ok((event.pid, event.tid))
        }
    }
}

/// A process's name: the file name of the program it last ran.
fn process_name(trace: &Trace, pid: u32) -> String {
    let process = trace.processes().into_iter().rev().find(|p| p.pid == pid);
    let name = process.map_or_else(|| format!("pid {pid}"), |p| p.name());
    name.rsplit('/').next().unwrap_or(&name).to_string()
}

/// `source`'s lines within `radius` of `line`.
fn narrowed(source: &Source, line: u32, radius: u32) -> Option<Source> {
    let first = line.saturating_sub(radius).max(source.first);
    let skip = (first - source.first) as usize;
    let take = (line + radius + 1).saturating_sub(first) as usize;
    let lines: Vec<String> = source.lines.iter().skip(skip).take(take).cloned().collect();
    if lines.is_empty() {
        return None;
    }
    Some(Source { first, lines })
}

/// One frame of a thread's stack, as the gdb script lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Frame {
    pub level: u32,
    pub function: Option<String>,
    /// The source file as the program's DWARF names it, and where it was
    /// found here.
    pub file: Option<String>,
    pub fullname: Option<String>,
    pub line: Option<u32>,
    /// The instruction, in hex: a frame's pc, or a caller's return address.
    pub pc: String,
    /// The program or library the instruction is in, by its path in the VM.
    pub object: Option<String>,
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

impl Frame {
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

/// For each of `frames`, the lines within `radius` of its line in its
/// source file, as `read` gives the file by its path here: None for a
/// frame without both, or whose file `read` cannot give. Each file is
/// read once, however many frames are in it.
fn frame_sources(
    frames: &[Frame],
    radius: u32,
    mut read: impl FnMut(&str) -> Option<String>,
) -> Vec<Option<Source>> {
    let mut files: HashMap<&str, Option<String>> = HashMap::new();
    frames
        .iter()
        .map(|frame| {
            let path = frame.fullname.as_deref()?;
            let line = frame.line?;
            let text = files.entry(path).or_insert_with(|| read(path)).as_deref()?;
            source_around(text, line, radius)
        })
        .collect()
}

/// Lines of a source file around one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    /// The number of the first line.
    pub first: u32,
    pub lines: Vec<String>,
}

/// The lines of `text` within `radius` of line `line`, counted from 1.
/// None when the file has no such line.
pub fn source_around(text: &str, line: u32, radius: u32) -> Option<Source> {
    let count = u32::try_from(text.lines().count()).ok()?;
    if line == 0 || line > count {
        return None;
    }
    let first = line.saturating_sub(radius).max(1);
    let last = line.saturating_add(radius).min(count);
    let lines = text
        .lines()
        .skip((first - 1) as usize)
        .take((last - first + 1) as usize)
        .map(String::from)
        .collect();
    Some(Source { first, lines })
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
    /// the trace saw it in. A step with no event, or the kernel's, needs a
    /// process. Builds a trace of a few writes by hand.
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
        assert_eq!(thread_at(&trace, 10, None, None).unwrap(), (166, 168));
        assert_eq!(thread_at(&trace, 10, Some(166), None).unwrap(), (166, 168));
        assert_eq!(thread_at(&trace, 10, Some(170), None).unwrap(), (170, 170));
        assert_eq!(thread_at(&trace, 14, None, Some(168)).unwrap(), (166, 168));
        assert_eq!(
            thread_at(&trace, 11, Some(166), Some(167)).unwrap(),
            (166, 167)
        );
        assert!(thread_at(&trace, 11, None, None).is_err());
        assert!(thread_at(&trace, 12, None, None).is_err());
        assert!(thread_at(&trace, 10, None, Some(999)).is_err());
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
    /// file, and none for a line past the end.
    #[test]
    fn source_lines_are_cut_at_either_end_of_the_file() {
        let text = "a\nb\nc\nd\ne\nf\n";
        assert_eq!(
            source_around(text, 3, 2),
            Some(Source {
                first: 1,
                lines: ["a", "b", "c", "d", "e"].map(String::from).to_vec(),
            })
        );
        assert_eq!(
            source_around(text, 6, 2),
            Some(Source {
                first: 4,
                lines: ["d", "e", "f"].map(String::from).to_vec(),
            })
        );
        assert_eq!(source_around(text, 7, 2), None);
    }

    /// Every frame with a file and a line gets the lines around its own
    /// line, two frames in one file each their own, and the file is read
    /// once. A frame without a line, or whose file cannot be read, gets
    /// none. Frames as the gdb script lists them, over files held in a
    /// map.
    #[test]
    fn each_frame_with_a_line_gets_its_own_window() {
        let stack = frames(
            r#"[
            {"level":0,"function":"run_job","file":"src/pool.c","fullname":"/s/pool.c","line":2,"pc":"0x1","object":null},
            {"level":1,"function":"worker","file":"src/pool.c","fullname":"/s/pool.c","line":5,"pc":"0x2","object":null},
            {"level":2,"function":"start_thread","file":"pthread_create.c","fullname":"/gone.c","line":9,"pc":"0x3","object":null},
            {"level":3,"function":"__clone3","file":null,"fullname":null,"line":null,"pc":"0x4","object":"/lib/libc.so.6"}
        ]"#,
        );
        let mut reads = 0;
        let sources = frame_sources(&stack, 1, |path| {
            reads += 1;
            (path == "/s/pool.c").then(|| "a\nb\nc\nd\ne\nf\n".to_string())
        });
        let lines = |first: u32, lines: &[&str]| {
            Some(Source {
                first,
                lines: lines.iter().map(|l| l.to_string()).collect(),
            })
        };
        assert_eq!(
            sources,
            vec![
                lines(1, &["a", "b", "c"]),
                lines(4, &["d", "e", "f"]),
                None,
                None
            ]
        );
        assert_eq!(reads, 2);
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
