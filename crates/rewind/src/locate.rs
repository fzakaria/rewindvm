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
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use rewind_core::accesses::Sought;
use rewind_core::threads::OnTheCpu;
use rewind_core::{Home, Run};
use rewind_trace::Trace;
use rewind_trace::ending::ExitStatus;
use rewind_trace::located::{Extent, Frame, Located, SourceFile};
use serde::Deserialize;

use crate::gdb::{self, KeptGdb, Kernel, Needs, Output, Say, Server, Symbols};

/// The gdb script, written to the session's directory for gdb to source,
/// and the start of the line it answers on.
const SCRIPT: &str = include_str!("locate.py");
const SCRIPT_NAME: &str = "where.py";

/// The files the script reads what it is asked from: expressions to
/// evaluate in each frame, and threads to walk with addresses to place.
const EXPRESSIONS_NAME: &str = "where-expressions.json";
const THREADS_NAME: &str = "where-threads.json";
const ANSWER_MARKER: &str = "rewind-where: ";

/// The convenience variables the script reads what it is asked from,
/// cleared at the start of each session so a kept gdb's last session's
/// do not carry over.
const SCRIPT_VARIABLES: [&str; 5] = [
    "rewind_tid",
    "rewind_after_access",
    "rewind_expressions",
    "rewind_threads",
    "rewind_address",
];

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
pub(crate) enum Format {
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
pub(crate) fn locate(
    home: &Home,
    run: &Run,
    step: u64,
    (pid, tid): (Option<u32>, Option<u32>),
    after: Option<Sought>,
    callers: usize,
    format: Format,
) -> Result<ExitCode> {
    let answer = located_asking(home, run, step, (pid, tid), &[], after)?;
    let (pid, tid, step) = (answer.pid, answer.tid, answer.step);
    match format {
        Format::Json => println!("{}", serde_json::to_string(&answer)?),
        Format::Text => {
            let whose = Whose {
                pid,
                process: &answer.process,
                tid,
                step,
                after,
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

/// Where thread `tid` of process `pid` was at `step` of `run`, by default
/// the thread of the step's own event, else the one on the CPU: its frames,
/// the one in the program's own code, and their source.
pub(crate) fn located(
    home: &Home,
    run: &Run,
    step: u64,
    (pid, tid): (Option<u32>, Option<u32>),
) -> Result<Located> {
    located_asking(home, run, step, (pid, tid), &[], None)
}

/// Where a thread was, as [`located`] says, and what the expressions in
/// `asked` evaluate to in each of its frames: at the step's start, or
/// just after the access `after` names, made in the step.
pub(crate) fn located_asking(
    home: &Home,
    run: &Run,
    step: u64,
    (pid, tid): (Option<u32>, Option<u32>),
    asked: &[String],
    after: Option<Sought>,
) -> Result<Located> {
    located_keeping(home, run, step, (pid, tid), asked, after).map(|(located, _)| located)
}

/// Where a thread was, as [`located_asking`] says, and the gdb that
/// walked it, still running with the process's symbols, for another
/// session at another step.
pub(crate) fn located_keeping(
    home: &Home,
    run: &Run,
    step: u64,
    (pid, tid): (Option<u32>, Option<u32>),
    asked: &[String],
    after: Option<Sought>,
) -> Result<(Located, KeptGdb)> {
    let pick = thread_at(run.trace()?, step, pid, tid)?;

    // A process at its own exit has no memory left to name its frames
    // with, so it is looked at an earlier step: the one before, as it
    // called exit, or its last event before a signal killed it.
    let step = match pick {
        Pick::Thread { pid, .. } => {
            let alive = alive_step(run.trace()?, step, pid);
            if alive != step {
                eprintln!(
                    "{} process {pid} exits at step {step}, its memory gone; \
                     looking at step {alive}, while it still had it",
                    crate::color::stderr().prefix()
                );
            }
            alive
        }
        Pick::OnTheCpu => step,
    };
    let needs = Needs::Tasks("`rewind where` cannot find threads".into());
    if let Pick::Thread { pid, tid } = pick {
        let (machine, symbols, moment) = fork_and_symbols(home, run, (pid, step), needs, after)?;
        return walk(run, step, machine, symbols, (pid, tid), (asked, moment));
    }
    let mut machine = gdb::fork(home, run, step, needs)?;
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
    // The process's files as they were mapped where the thread is looked
    // at: a library loaded earlier in the step is mapped by the access.
    let moment = run_to(&mut machine, pid, step, after)?;
    let symbols = Symbols::load_on(
        home,
        run,
        (step, &mut machine),
        pid,
        Say::Aloud,
        Server::Own,
    )?;
    walk(run, step, machine, symbols, (pid, tid), (asked, moment))
}

/// When in a step a look is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Moment {
    /// At the step's start, where a fork there is.
    StepStart,
    /// Just after an access the step makes, where the CPU trapped: the
    /// innermost frame's line is the accessing instruction's.
    AfterAccess,
}

/// Runs `machine`, at the start of `step`, to just after process `pid`
/// makes the access `after` names, when one is named.
fn run_to(
    machine: &mut rewind_vmm::Machine,
    pid: u32,
    step: u64,
    after: Option<Sought>,
) -> Result<Moment> {
    let Some(sought) = after else {
        return Ok(Moment::StepStart);
    };
    rewind_core::accesses::run_to_access(machine, pid, step, sought)?;
    Ok(Moment::AfterAccess)
}

/// A fork of `run` at `step` for gdb, run to just after process `pid`
/// makes the access `after` names when one is named, and the process's
/// symbols there, with when in the step the fork is. The symbols take an
/// inspection on a fork of its own, so the two forks are made side by
/// side: each restores a keyframe and replays to the step, which on a
/// long run takes seconds. The inspection's fork runs to the access too:
/// a library the process loads earlier in the step, such as the one that
/// makes the access, is mapped only by then.
pub(crate) fn fork_and_symbols(
    home: &Home,
    run: &Run,
    (pid, step): (u32, u64),
    needs: Needs,
    after: Option<Sought>,
) -> Result<(rewind_vmm::Machine, Symbols, Moment)> {
    std::thread::scope(|scope| {
        let symbols = scope.spawn(|| match after {
            None => Symbols::load(
                home,
                run,
                step,
                Some(pid),
                Kernel::Skip,
                Say::Aloud,
                Server::Own,
            ),
            Some(_) => {
                let keep = rewind_core::run::Keep::Keyframe;
                let mut machine = run.machine_at(home, step, keep, &mut rewind_vmm::Ignore)?;
                run_to(&mut machine, pid, step, after)?;
                Symbols::load_on(
                    home,
                    run,
                    (step, &mut machine),
                    pid,
                    Say::Aloud,
                    Server::Own,
                )
            }
        });
        let machine = gdb::fork(home, run, step, needs).and_then(|mut machine| {
            let moment = run_to(&mut machine, pid, step, after)?;
            Ok((machine, moment))
        });
        let symbols = symbols.join().expect("loading the symbols panicked");
        let (machine, moment) = machine?;
        Ok((machine, symbols?, moment))
    })
}

/// Which thread of a failure `rewind check --where` names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Involved {
    /// The thread on the CPU at the step that decides how the run ends.
    AtTheDecidingStep,
    /// The thread of the failing run's first event that differs.
    AtTheFirstDifference,
}

/// A line of `rewind check --where`: the thread `answer` describes, by the
/// frame in the program's own code it was in, else its innermost one, and
/// why it is named.
pub(crate) fn thread_line(answer: &Located, involved: Involved) -> String {
    let frame = answer
        .chosen
        .or((!answer.frames.is_empty()).then_some(0))
        .map(|i| &answer.frames[i]);
    let place = match frame {
        None => "no frame names a function".to_string(),
        Some(f) if f.place_label().is_empty() => f.function_label(),
        Some(f) => format!("{} ({})", f.function_label(), f.place_label()),
    };
    let why = match involved {
        Involved::AtTheDecidingStep => "on the CPU at the deciding step",
        Involved::AtTheFirstDifference => "at the first event that differs",
    };
    format!(
        "  {:>10}   {}/{}   {place}, {why}",
        answer.step, answer.pid, answer.tid
    )
}

/// Thread `tid`'s frames on `machine`, a fork of `run` at `step`, with
/// process `pid`'s symbols, and the one in the program's own code with its
/// source; and the gdb that walked them, kept running.
fn walk(
    run: &Run,
    step: u64,
    machine: rewind_vmm::Machine,
    symbols: Symbols,
    (pid, tid): (u32, u32),
    (asked, moment): (&[String], Moment),
) -> Result<(Located, KeptGdb)> {
    // The fork as gdb sees it, and gdb with the process's symbols there.
    let scope = rewind_core::debug::Scope::Process(pid);
    let mut debuggee = gdb::debuggee(run, step, machine, scope)?;
    let mut gdb = KeptGdb::start(symbols, &debuggee, Say::Aloud)?;
    let symbols = gdb.symbols();

    // What the script is asked: the thread, and the expressions.
    let mut commands = vec![format!("set $rewind_tid = {tid}")];
    if moment == Moment::AfterAccess {
        commands.push("set $rewind_after_access = 1".to_string());
    }
    if !asked.is_empty() {
        let file = symbols.dir().join(EXPRESSIONS_NAME);
        std::fs::write(&file, serde_json::to_string(asked)?)
            .context("writing the expressions for gdb")?;
        commands.push(gdb_string("rewind_expressions", &file));
    }
    eprintln!(
        "{} walking thread {tid}'s stack in gdb",
        crate::color::stderr().prefix()
    );
    let printed = script_session(&mut gdb, &mut debuggee, commands)?;
    let walked: Walked = script_answer(&printed)?;
    if let Some(error) = walked.error {
        bail!("gdb could not walk thread {tid}'s stack: {error}");
    }
    let frames = walked.frames.unwrap_or_default();
    let located = located_from(run, step, (pid, tid), frames, gdb.symbols())?;
    Ok((located, gdb))
}

/// Where thread `tid` of process `pid` was at `step` of `run`, from the
/// frames gdb walked with `symbols`: the frames, the one in the program's
/// own code, and their source.
fn located_from(
    run: &Run,
    step: u64,
    (pid, tid): (u32, u32),
    mut frames: Vec<Frame>,
    symbols: &Symbols,
) -> Result<Located> {
    // Each source file, read while the session's copies are here, then
    // every path, the files' own too, as the VM had it or the DWARF names
    // it.
    let chosen = chosen(&frames);
    let files = source_files(&frames, &JSON_LIMITS, |path| {
        std::fs::read_to_string(path).ok()
    });
    let renamed = rename_in_vm(&mut frames, symbols);
    let files = files
        .into_iter()
        .map(|(path, file)| {
            let name = renamed
                .get(&path)
                .cloned()
                .unwrap_or_else(|| symbols.name_in_vm(&path, None));
            (name, file)
        })
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

/// Where each of threads `tids` of process `pid` was at `step` of `run`,
/// as [`located`] says for each, from one fork and one gdb session that
/// walks them all: a thread gdb could not walk says why. A process whose
/// exit is at the step is looked at an earlier step, as [`located`] looks
/// at it. gdb asks `server` for DWARF and sources.
pub(crate) fn located_threads(
    home: &Home,
    run: &Run,
    step: u64,
    (pid, tids): (u32, &[u32]),
    server: Server,
) -> Result<Vec<(u32, Result<Located>)>> {
    let alive = alive_step(run.trace()?, step, pid);
    if alive != step {
        eprintln!(
            "{} process {pid} exits at step {step}, its memory gone; \
             looking at step {alive}, while it still had it",
            crate::color::stderr().prefix()
        );
    }
    let step = alive;

    // One fork, which the symbols are read on and gdb connects to.
    let needs = Needs::Tasks("`rewind where` cannot find threads".into());
    let mut machine = gdb::fork(home, run, step, needs)?;
    let symbols = Symbols::load_on(home, run, (step, &mut machine), pid, Say::Aloud, server)?;
    let scope = rewind_core::debug::Scope::Process(pid);
    let mut debuggee = gdb::debuggee(run, step, machine, scope)?;
    let mut gdb = KeptGdb::start(symbols, &debuggee, Say::Aloud)?;

    // Every thread's frames, with their variables, from one session.
    eprintln!(
        "{} walking the stacks of {} threads of process {pid} at step {step} in gdb",
        crate::color::stderr().prefix(),
        tids.len()
    );
    let (walked, _) = walk_threads(&mut debuggee, &mut gdb, (tids, &[]), Walk::WithVariables)?;
    let located = walked
        .into_iter()
        .map(|t| {
            let tid = t.tid;
            let located = match (t.frames, t.error) {
                (_, Some(error)) => Err(anyhow::anyhow!(
                    "gdb could not walk thread {tid}'s stack: {error}"
                )),
                (frames, None) => located_from(
                    run,
                    step,
                    (pid, tid),
                    frames.unwrap_or_default(),
                    gdb.symbols(),
                ),
            };
            (tid, located)
        })
        .collect();
    Ok(located)
}

/// Gives each of `frames`' source files and programs the name it has for a
/// reader with no access to this machine, as [`Symbols::name_in_vm`] names
/// it, and returns each source file's name by its path here.
fn rename_in_vm(frames: &mut [Frame], symbols: &Symbols) -> BTreeMap<String, String> {
    let mut renamed = BTreeMap::new();
    for frame in frames {
        if let Some(path) = frame.fullname.take() {
            let name = symbols.name_in_vm(&path, frame.file.as_deref());
            renamed.insert(path, name.clone());
            frame.fullname = Some(name);
        }
        frame.object = frame
            .object
            .as_ref()
            .map(|path| symbols.name_in_vm(path, None));
    }
    renamed
}

/// What `gdb` printed in a session on `debuggee` that sources the script
/// after `commands`, the variables it reads cleared before them.
fn script_session(
    gdb: &mut KeptGdb,
    debuggee: &mut rewind_core::debug::Debuggee<'_>,
    commands: Vec<String>,
) -> Result<gdb::Printed> {
    let script = gdb.symbols().dir().join(SCRIPT_NAME);
    std::fs::write(&script, SCRIPT).context("writing the gdb script")?;
    let cleared = SCRIPT_VARIABLES
        .iter()
        .map(|name| format!("'{name}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut session = vec![format!(
        "python [gdb.set_convenience_variable(v, None) for v in ({cleared})]"
    )];
    session.extend(commands);
    session.push(format!("source {}", script.display()));
    gdb.session(debuggee, &session)
}

/// gdb's command setting convenience variable `name` to `path`, as a
/// string the script reads a file from.
fn gdb_string(name: &str, path: &std::path::Path) -> String {
    let quoted = path
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    format!("set ${name} = \"{quoted}\"")
}

/// What the gdb script prints for threads and addresses asked about
/// together.
#[derive(Deserialize)]
struct ThreadsAndPlaces {
    threads: Option<Vec<WalkedThread>>,
    places: Option<Vec<Place>>,
    error: Option<String>,
}

/// One thread's frames, as the script walks it with others.
#[derive(Deserialize)]
struct WalkedThread {
    tid: u32,
    frames: Option<Vec<Frame>>,
    error: Option<String>,
}

/// Where threads of a process were at a step, and where addresses are in
/// its code, from one gdb session on a fork there.
pub(crate) struct ThreadsAt {
    /// Each thread's frames, innermost first, and the index of the one in
    /// the program's own code; an error for one gdb could not walk.
    pub(crate) threads: Vec<(u32, ThreadFrames)>,
    /// Each address's place, with its source line's text when gdb found
    /// the file.
    pub(crate) places: Vec<PlaceAt>,
}

/// A thread's frames, innermost first, and the index of the one in the
/// program's own code; or why gdb could not walk them.
pub(crate) type ThreadFrames = Result<(Vec<Frame>, Option<usize>), String>;

/// An address's function and source line, and the line's text.
#[derive(Debug, Default)]
pub(crate) struct PlaceAt {
    pub(crate) function: Option<String>,
    pub(crate) file: Option<String>,
    pub(crate) line: Option<u32>,
    pub(crate) text: Option<String>,
}

/// How much of each frame a walk of threads lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Walk {
    /// Where the frame is, without its variables.
    Bare,
    /// Where the frame is, its arguments and locals, and the expressions
    /// on the lines around its line, as `rewind where` lists them.
    WithVariables,
}

/// Threads `tids` of the debugged process, and the places of `addresses`
/// in its code, from a session of `gdb`, which has the process's symbols,
/// on `debuggee`, a fork at a step, each frame listed as `walk` says.
/// Paths are as the VM had them.
pub(crate) fn threads_at(
    debuggee: &mut rewind_core::debug::Debuggee<'_>,
    gdb: &mut KeptGdb,
    (tids, addresses): (&[u32], &[u64]),
    walk: Walk,
) -> Result<ThreadsAt> {
    let (walked, places) = walk_threads(debuggee, gdb, (tids, addresses), walk)?;
    let symbols = gdb.symbols();

    // Each path as the VM had it or the DWARF names it, a line's text read
    // while the session's copy of its file is here.
    let threads = walked
        .into_iter()
        .map(|t| {
            let walked = match (t.frames, t.error) {
                (_, Some(error)) => Err(error),
                (frames, None) => {
                    let mut frames = frames.unwrap_or_default();
                    rename_in_vm(&mut frames, symbols);
                    let chosen = chosen(&frames);
                    Ok((frames, chosen))
                }
            };
            (t.tid, walked)
        })
        .collect();
    let places = places
        .into_iter()
        .map(|p| {
            let text = match (&p.fullname, p.line) {
                (Some(path), Some(line)) => std::fs::read_to_string(path)
                    .ok()
                    .and_then(|t| t.lines().nth(line as usize - 1).map(str::to_string)),
                _ => None,
            };
            PlaceAt {
                function: p.function,
                file: p.file,
                line: p.line,
                text,
            }
        })
        .collect();
    Ok(ThreadsAt { threads, places })
}

/// What the script prints for threads `tids` and `addresses`, asked in a
/// session of `gdb` on `debuggee`, each frame listed as `walk` says: each
/// thread's frames, with paths as this machine has them, and each
/// address's place.
fn walk_threads(
    debuggee: &mut rewind_core::debug::Debuggee<'_>,
    gdb: &mut KeptGdb,
    (tids, addresses): (&[u32], &[u64]),
    walk: Walk,
) -> Result<(Vec<WalkedThread>, Vec<Place>)> {
    // What the script is asked, written for it to read.
    let asked = gdb.symbols().dir().join(THREADS_NAME);
    let addresses: Vec<String> = addresses.iter().map(|a| format!("{a:#x}")).collect();
    let variables = walk == Walk::WithVariables;
    std::fs::write(
        &asked,
        serde_json::to_string(&serde_json::json!({
            "tids": tids,
            "addresses": addresses,
            "variables": variables,
        }))?,
    )
    .context("writing the threads for gdb")?;
    let commands = vec![gdb_string("rewind_threads", &asked)];
    let printed = script_session(gdb, debuggee, commands)?;
    let answer: ThreadsAndPlaces = script_answer(&printed)?;
    if let Some(error) = answer.error {
        bail!("gdb could not walk the threads: {error}");
    }
    Ok((
        answer.threads.unwrap_or_default(),
        answer.places.unwrap_or_default(),
    ))
}

/// What the gdb script prints for an address: the function it is in and
/// how far into it, its source line, and its program.
#[derive(Debug, Deserialize)]
struct Place {
    function: Option<String>,
    offset: Option<u64>,
    file: Option<String>,
    fullname: Option<String>,
    line: Option<u32>,
    object: Option<String>,
    /// Whether the address is in a variable, `function` naming it and
    /// `line` its definition, rather than in code.
    #[serde(default)]
    variable: bool,
    error: Option<String>,
}

/// Where `address` is in the code process `pid` had mapped at `step` of
/// `run`. gdb reads the symbol files alone, with no fork to connect to.
fn place(home: &Home, run: &Run, step: u64, pid: u32, address: u64) -> Result<Place> {
    let symbols = Symbols::load(
        home,
        run,
        step,
        Some(pid),
        Kernel::Skip,
        Say::Nothing,
        Server::Own,
    )?;
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

/// Where `address` is in process `pid`'s memory at `step` of `run`, in
/// words: the variable and the line defining it, or the function and its
/// source line. None when no symbol has it.
pub(crate) fn address_words(
    home: &Home,
    run: &Run,
    step: u64,
    pid: u32,
    address: u64,
) -> Option<String> {
    let place = place(home, run, step, pid, address).ok()?;
    if !place.variable {
        return place_words(&place);
    }
    let name = place.function.as_ref()?;
    let at = match place.offset {
        Some(offset) if offset > 0 => format!("{name}+{offset}"),
        _ => name.clone(),
    };
    Some(match (&place.file, place.line) {
        (Some(file), Some(line)) => format!("{at}, a variable ({}:{line})", file_name(file)),
        _ => format!("{at}, a variable"),
    })
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
pub(crate) fn describe_stall(home: &Home, run: &Run) -> Option<String> {
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
pub(crate) enum Pick {
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
pub(crate) fn thread_at(
    trace: &Trace,
    step: u64,
    pid: Option<u32>,
    tid: Option<u32>,
) -> Result<Pick> {
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

/// The step `rewind where` looks at process `pid` at, asked about `step`:
/// the step itself, unless the step's event is the process's own exit,
/// since by then the kernel has taken its memory and, with it, every file
/// that names its frames. A process that called exit is looked at the
/// step before. A process killed by a signal is looked at its last event
/// before the exit: the kernel tears it down from the kill on, and can
/// drop it from /proc steps before its exit is recorded.
fn alive_step(trace: &Trace, step: u64, pid: u32) -> u64 {
    let status = trace.events.iter().find_map(|e| match e.kind {
        rewind_trace::EventKind::Exit {
            status,
            thread: false,
            ..
        } if e.step == step && e.pid == pid => Some(status),
        _ => None,
    });
    let Some(status) = status else {
        return step;
    };

    // A killed process's last event of its own, else the step before.
    let before = step.saturating_sub(1);
    match ExitStatus::from_raw(status) {
        ExitStatus::Code(_) => before,
        ExitStatus::Signal { .. } => trace
            .events
            .iter()
            .rev()
            .find(|e| e.pid == pid && e.step < step)
            .map_or(before, |e| e.step),
    }
}

/// The name of the process with id `pid` at `step`: the file name of the
/// program it ran then, as the desktop app names it too, else its pid.
fn process_name(trace: &Trace, pid: u32, step: u64) -> String {
    trace
        .process_name_at(pid, step)
        .unwrap_or_else(|| format!("pid {pid}"))
}

/// Where x86-64's lower half, user space, ends: an instruction at or
/// above it is the kernel's.
const USER_END: u64 = 0x0000_8000_0000_0000;

/// The start of the file names of the dynamic loader and of the C
/// library's, whose frames are not the program's own code.
const SYSTEM_LIBRARIES: &[&str] = &["ld-linux", "ld-musl", "libc.so", "libc-", "libpthread"];

/// Where the DWARF of code that is not the program's own puts its
/// sources: Rust's standard library, crates that Nix vendors or Cargo
/// fetches, Go's standard library under a Nix GOROOT, and Go modules in
/// the module cache or a vendor directory.
const LIBRARY_SOURCES: &[&str] = &[
    "/rustc/",
    "/cargo-vendor-dir/",
    "/.cargo/registry/",
    "/.cargo/git/",
    "/share/go/src/",
    "/pkg/mod/",
    "/vendor/",
];

/// The first elements of the Go standard library's package paths, which
/// -trimpath names its files by. A program's own module may also lack a
/// domain, as `module myapp` does, so the roots are listed.
const GO_STANDARD_ROOTS: &[&str] = &[
    "archive",
    "bufio",
    "bytes",
    "cmp",
    "compress",
    "container",
    "context",
    "crypto",
    "database",
    "debug",
    "embed",
    "encoding",
    "errors",
    "expvar",
    "flag",
    "fmt",
    "go",
    "hash",
    "html",
    "image",
    "index",
    "internal",
    "io",
    "iter",
    "log",
    "maps",
    "math",
    "mime",
    "net",
    "os",
    "path",
    "plugin",
    "reflect",
    "regexp",
    "runtime",
    "slices",
    "sort",
    "strconv",
    "strings",
    "structs",
    "sync",
    "syscall",
    "testing",
    "text",
    "time",
    "unicode",
    "unique",
    "unsafe",
    "vendor",
    "weak",
];

/// The extensions of Go's sources: its code and its assembly.
const GO_SOURCES: &[&str] = &[".go", ".s"];

/// What -trimpath puts between a Go module's path and its version in the
/// names of the module's files.
const GO_MODULE_VERSION: &str = "@v";

/// The name the Go compiler gives the source of the wrappers it writes.
const GO_AUTOGENERATED: &str = "<autogenerated>";

/// Where the system's headers are, whose inline functions, such as the C
/// library's fortified `write`, are compiled into the program without
/// being its code: a Nix package's top-level include directory, or the
/// system's. A project's own headers sit deeper in its sources.
const SYSTEM_HEADERS: &str = "/usr/include/";
const STORE: &str = "/nix/store/";
const PACKAGE_HEADERS: &str = "include";

/// What frame choice asks of a frame.
trait Choice {
    fn address(&self) -> Option<u64>;
    fn in_user_space(&self) -> bool;
    fn in_system_library(&self) -> bool;
    fn in_library_source(&self) -> bool;
    fn in_system_header(&self) -> bool;
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
        let by_path = [&self.file, &self.fullname]
            .into_iter()
            .flatten()
            .any(|path| LIBRARY_SOURCES.iter().any(|dir| path.contains(dir)));
        by_path || self.file.as_deref().is_some_and(trimmed_go_library)
    }

    /// Whether the frame's source is a system header: in the system's
    /// include directory, or in a store package's own.
    fn in_system_header(&self) -> bool {
        [&self.file, &self.fullname]
            .into_iter()
            .flatten()
            .any(|path| {
                if path.starts_with(SYSTEM_HEADERS) {
                    return true;
                }
                let Some(in_store) = path.strip_prefix(STORE) else {
                    return false;
                };
                in_store.split('/').nth(1) == Some(PACKAGE_HEADERS)
            })
    }

    /// Whether the frame is the program's own code: user space, with a
    /// source line, outside the system's libraries and headers and other
    /// code's sources.
    fn own_code(&self) -> bool {
        self.in_user_space()
            && self.file.is_some()
            && self.line.is_some()
            && !self.in_system_library()
            && !self.in_library_source()
            && !self.in_system_header()
    }
}

/// Whether `file`, a source's name in a Go program's DWARF, is a file of
/// code the program does not own as -trimpath names it: Go's standard
/// library by its package's path, a dependency by its module's path and
/// version, or a wrapper the compiler wrote.
fn trimmed_go_library(file: &str) -> bool {
    if file.ends_with(GO_AUTOGENERATED) {
        return true;
    }

    // A path within the build, not one on the build machine, of Go code.
    let relative = !file.starts_with('/');
    let go = GO_SOURCES.iter().any(|ext| file.ends_with(ext));
    if !relative || !go {
        return false;
    }

    // The standard library's packages, or a module at a version.
    let standard = file
        .split_once('/')
        .is_some_and(|(first, _)| GO_STANDARD_ROOTS.contains(&first));
    standard || file.contains(GO_MODULE_VERSION)
}

/// The innermost frame in the program's own code, else the innermost in
/// user space with a name and a source line outside the C library, such as
/// a language runtime's, else the innermost in user space with a name.
pub(crate) fn chosen(frames: &[Frame]) -> Option<usize> {
    let named = |f: &Frame| f.in_user_space() && f.function.is_some();
    let with_line =
        |f: &Frame| named(f) && f.file.is_some() && f.line.is_some() && !f.in_system_library();
    frames
        .iter()
        .position(Frame::own_code)
        .or_else(|| frames.iter().position(with_line))
        .or_else(|| frames.iter().position(named))
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
pub(crate) struct Source {
    /// The number of the first line.
    pub first: u32,
    pub lines: Vec<String>,
}

/// Whose stack it was, for the header line.
pub(crate) struct Whose<'a> {
    pub pid: u32,
    pub process: &'a str,
    pub tid: u32,
    pub step: u64,
    /// The access in the step the look is just after, if any.
    pub after: Option<Sought>,
}

/// What a header says of a look just after `after`, nothing for one at
/// the step's start: ", just after its write of the 8 bytes at 0x…", with
/// which of the step's when it is not the first.
fn moment_words(after: Option<Sought>) -> String {
    let Some(sought) = after else {
        return String::new();
    };
    let which = match sought.hit {
        1 => String::new(),
        n => format!("{} ", ordinal(n)),
    };
    let (what, to) = match sought.kind {
        rewind_core::accesses::Kind::Write => ("write", "of"),
        rewind_core::accesses::Kind::Read => ("access", "to"),
    };
    let in_step = if sought.hit == 1 { "" } else { " in the step" };
    format!(
        ", just after its {which}{what} {to} the {} bytes at {:#x}{in_step}",
        sought.value.len, sought.value.address
    )
}

/// `n` as an English ordinal: 2nd, 3rd, 11th, 21st.
fn ordinal(n: u32) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// `rewind where`'s answer in words: a header, the chosen frame with its
/// source, arguments and locals, and up to `callers` frames it was called
/// from.
pub(crate) fn render(
    whose: &Whose,
    frames: &[Frame],
    chosen: Option<usize>,
    source: Option<&Source>,
    callers: usize,
) -> String {
    let mut out = format!(
        "process {} ({}), thread {}, at step {}{}\n",
        whose.pid,
        whose.process,
        whose.tid,
        whose.step,
        moment_words(whose.after)
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

    // The frame's arguments and locals, under a heading each, as gdb's
    // `info args` and `info locals` name them.
    for (heading, variables) in [
        (ARGUMENTS_HEADING, &frame.arguments),
        (LOCALS_HEADING, &frame.locals),
    ] {
        if variables.is_empty() {
            continue;
        }
        out.push_str(&format!("{heading}:\n"));
        for variable in variables {
            out.push_str(&format!("  {} = {}\n", variable.name, variable.value));
        }
    }

    // The frames that called it.
    for caller in frames.iter().skip(at + 1).take(callers) {
        out.push_str(&format!("called from {}\n", frame_line(caller)));
    }
    out
}

/// The headings over the chosen frame's arguments and its locals.
const ARGUMENTS_HEADING: &str = "arguments";
const LOCALS_HEADING: &str = "locals";

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
    use rewind_trace::located::{Location, Variable};

    #[test]
    fn a_thread_line_names_its_function_line_and_why() {
        // The chosen frame, the program's own, is named with its source
        // line, past the C library's frames inside it; a thread whose stack
        // gave no frames says so.
        let answer = Located {
            run: "5c910df9774b38f2".into(),
            step: 5139,
            pid: 166,
            tid: 166,
            process: "test_pool_shutdown".into(),
            frames: frames(
                r#"[
                {"level": 0, "function": "__GI___clock_nanosleep", "file": "clock_nanosleep.c",
                 "fullname": null, "line": 48, "pc": "0x7f0", "object": "/lib/libc.so.6",
                 "arguments": [], "locals": [], "expressions": [], "evaluated": []},
                {"level": 5, "function": "main", "file": "tests/test_pool_shutdown.c",
                 "fullname": null, "line": 23, "pc": "0x5a0", "object": "/build/t",
                 "arguments": [], "locals": [], "expressions": [], "evaluated": []}
            ]"#,
            ),
            chosen: Some(1),
            files: BTreeMap::new(),
        };
        assert_eq!(
            thread_line(&answer, Involved::AtTheDecidingStep),
            "        5139   166/166   main (tests/test_pool_shutdown.c:23), on the CPU at the deciding step"
        );
        let bare = Located {
            frames: Vec::new(),
            chosen: None,
            ..answer
        };
        assert_eq!(
            thread_line(&bare, Involved::AtTheFirstDifference),
            "        5139   166/166   no frame names a function, at the first event that differs"
        );
    }

    /// A frame list as JSON, the way the gdb script prints it.
    fn frames(json: &str) -> Vec<Frame> {
        serde_json::from_str(json).unwrap()
    }

    const IN_GLIBC: &str = r#"[
        {"level":0,"function":"__futex_abstimed_wait_common64","file":"./nptl/futex-internal.c","fullname":"/build/glibc-2.40/nptl/futex-internal.c","line":57,"pc":"0x7f61584f2b26","object":"/nix/store/bbb-glibc-2.40/lib/libc.so.6","arguments":[],"locals":[],"expressions":[],"evaluated":[]},
        {"level":1,"function":"pthread_cond_wait","file":"./nptl/pthread_cond_wait.c","fullname":null,"line":618,"pc":"0x7f61584f53d0","object":"/nix/store/bbb-glibc-2.40/lib/libc.so.6","arguments":[],"locals":[],"expressions":[],"evaluated":[]},
        {"level":2,"function":"worker","file":"src/pool.c","fullname":"/home/me/.local/share/rewind/gdb/42/build/mylib/src/pool.c","line":70,"pc":"0x55bfaf4373e1","object":"/build/mylib/tests/test_pool_shutdown","arguments":[],"locals":[],"expressions":[],"evaluated":[]},
        {"level":3,"function":"start_thread","file":"./nptl/pthread_create.c","fullname":null,"line":448,"pc":"0x7f61584f61d3","object":"/nix/store/bbb-glibc-2.40/lib/libc.so.6","arguments":[],"locals":[],"expressions":[],"evaluated":[]},
        {"level":4,"function":"__clone3","file":null,"fullname":null,"line":null,"pc":"0x7f615857e5bc","object":"/nix/store/bbb-glibc-2.40/lib/libc.so.6","arguments":[],"locals":[],"expressions":[],"evaluated":[]}
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
            {"level":0,"function":"std::sys::pal::unix::futex::futex_wait","file":"/rustc/48a229ce/library/std/src/sys/pal/unix/futex.rs","fullname":null,"line":72,"pc":"0x55e0000010a0","object":"/build/source/target/release/casita","arguments":[],"locals":[],"expressions":[],"evaluated":[]},
            {"level":1,"function":"tokio::runtime::park::Inner::park","file":"/build/cargo-vendor-dir/tokio-1.47.0/src/runtime/park.rs","fullname":null,"line":120,"pc":"0x55e0000020b0","object":"/build/source/target/release/casita","arguments":[],"locals":[],"expressions":[],"evaluated":[]},
            {"level":2,"function":"serde_json::de::from_str","file":"/home/me/.cargo/registry/src/index.crates.io-6f17d22bba15001f/serde_json-1.0.140/src/de.rs","fullname":null,"line":2,"pc":"0x55e0000030c0","object":"/build/source/target/release/casita","arguments":[],"locals":[],"expressions":[],"evaluated":[]},
            {"level":3,"function":"casita::main","file":"src/main.rs","fullname":"/build/source/src/main.rs","line":31,"pc":"0x55e0000040d0","object":"/build/source/target/release/casita","arguments":[],"locals":[],"expressions":[],"evaluated":[]}
        ]"#,
        );
        assert_eq!(chosen(&stack), Some(3));
    }

    /// In a Go program, the runtime, the standard library and modules the
    /// program depends on are passed over for the program's own package.
    /// Built with -trimpath, as nixpkgs builds Go, the standard library's
    /// files are named by their package's path, which has no domain, and
    /// a dependency's by its module path and version; without it, by their
    /// paths under GOROOT and the module cache or the vendor directory.
    /// The trimmed stack is direnv's at a close in `direnv deny`.
    #[test]
    fn go_s_runtime_library_and_modules_are_passed_over() {
        let trimmed = frames(
            r#"[
            {"level":0,"function":"internal/runtime/syscall/linux.Syscall6","file":"internal/runtime/syscall/linux/asm_linux_amd64.s","fullname":"./internal/runtime/syscall/linux/asm_linux_amd64.s","line":36,"pc":"0x40db2e","object":"/nix/store/aaa-direnv/bin/direnv","arguments":[],"locals":[],"expressions":[],"evaluated":[]},
            {"level":1,"function":"syscall.Close","file":"syscall/zsyscall_linux_amd64.go","fullname":"./syscall/zsyscall_linux_amd64.go","line":335,"pc":"0x40e000","object":"/nix/store/aaa-direnv/bin/direnv","arguments":[],"locals":[],"expressions":[],"evaluated":[]},
            {"level":2,"function":"os.WriteFile","file":"os/file.go","fullname":"./os/file.go","line":938,"pc":"0x40f000","object":"/nix/store/aaa-direnv/bin/direnv","arguments":[],"locals":[],"expressions":[],"evaluated":[]},
            {"level":3,"function":"github.com/BurntSushi/toml.Decode","file":"github.com/BurntSushi/toml@v1.5.0/decode.go","fullname":"./github.com/BurntSushi/toml@v1.5.0/decode.go","line":120,"pc":"0x410000","object":"/nix/store/aaa-direnv/bin/direnv","arguments":[],"locals":[],"expressions":[],"evaluated":[]},
            {"level":4,"function":"github.com/direnv/direnv/v2/internal/cmd.(*RC).Deny","file":"github.com/direnv/direnv/v2/internal/cmd/rc.go","fullname":"./github.com/direnv/direnv/v2/internal/cmd/rc.go","line":130,"pc":"0x411000","object":"/nix/store/aaa-direnv/bin/direnv","arguments":[],"locals":[],"expressions":[],"evaluated":[]},
            {"level":5,"function":"main.main","file":"github.com/direnv/direnv/v2/main.go","fullname":"./github.com/direnv/direnv/v2/main.go","line":25,"pc":"0x412000","object":"/nix/store/aaa-direnv/bin/direnv","arguments":[],"locals":[],"expressions":[],"evaluated":[]},
            {"level":6,"function":"runtime.main","file":"runtime/proc.go","fullname":"./runtime/proc.go","line":290,"pc":"0x413000","object":"/nix/store/aaa-direnv/bin/direnv","arguments":[],"locals":[],"expressions":[],"evaluated":[]}
        ]"#,
        );
        assert_eq!(chosen(&trimmed), Some(4));
        let untrimmed = frames(
            r#"[
            {"level":0,"function":"runtime.usleep","file":"/nix/store/bbb-go-1.25.3/share/go/src/runtime/sys_linux_amd64.s","fullname":null,"line":135,"pc":"0x492b17","object":"/build/app","arguments":[],"locals":[],"expressions":[],"evaluated":[]},
            {"level":1,"function":"github.com/urfave/cli.(*App).Run","file":"/build/go/pkg/mod/github.com/urfave/cli@v1.22.5/app.go","fullname":null,"line":40,"pc":"0x493000","object":"/build/app","arguments":[],"locals":[],"expressions":[],"evaluated":[]},
            {"level":2,"function":"github.com/spf13/pflag.Parse","file":"/build/source/vendor/github.com/spf13/pflag/flag.go","fullname":null,"line":12,"pc":"0x494000","object":"/build/app","arguments":[],"locals":[],"expressions":[],"evaluated":[]},
            {"level":3,"function":"main.main","file":"/build/source/main.go","fullname":null,"line":9,"pc":"0x495000","object":"/build/app","arguments":[],"locals":[],"expressions":[],"evaluated":[]}
        ]"#,
        );
        assert_eq!(chosen(&untrimmed), Some(3));

        // A module without a domain is still the program's own.
        let local = frames(
            r#"[
            {"level":0,"function":"os.WriteFile","file":"os/file.go","fullname":null,"line":938,"pc":"0x40f000","object":"/bin/myapp","arguments":[],"locals":[],"expressions":[],"evaluated":[]},
            {"level":1,"function":"myapp/store.Save","file":"myapp/store/save.go","fullname":null,"line":17,"pc":"0x411000","object":"/bin/myapp","arguments":[],"locals":[],"expressions":[],"evaluated":[]}
        ]"#,
        );
        assert_eq!(chosen(&local), Some(1));
    }

    /// A system header's inline function, such as the C library's
    /// fortified `write`, is compiled into the program but is not its
    /// code: the program's function that called it is chosen. A project's
    /// own headers, under an include directory deeper in its sources, are
    /// still its code.
    #[test]
    fn system_headers_inlined_into_the_program_are_passed_over() {
        let stack = frames(
            r#"[
            {"level":0,"function":"__syscall_cp_c","file":null,"fullname":null,"line":null,"pc":"0x401920","object":"/newroot/bin/race","arguments":[],"locals":[],"expressions":[],"evaluated":[]},
            {"level":1,"function":"write","file":"/nix/store/gs38hik78n8wz9ylhhrbp7210zcr1dzp-fortify-headers-3.0.1/include/unistd.h","fullname":null,"line":185,"pc":"0x401155","object":"/newroot/bin/race","arguments":[],"locals":[],"expressions":[],"evaluated":[]},
            {"level":2,"function":"add","file":"race.c","fullname":"/newroot/src/race.c","line":15,"pc":"0x401155","object":"/newroot/bin/race","arguments":[],"locals":[],"expressions":[],"evaluated":[]}
        ]"#,
        );
        assert_eq!(chosen(&stack), Some(2));
        let glibc = frames(
            r#"[
            {"level":0,"function":"strlen","file":"/usr/include/x86_64-linux-gnu/bits/string_fortified.h","fullname":null,"line":40,"pc":"0x401000","object":"/src/app","arguments":[],"locals":[],"expressions":[],"evaluated":[]},
            {"level":1,"function":"run","file":"app.c","fullname":null,"line":9,"pc":"0x401010","object":"/src/app","arguments":[],"locals":[],"expressions":[],"evaluated":[]}
        ]"#,
        );
        assert_eq!(chosen(&glibc), Some(1));
        let own_header = frames(
            r#"[
            {"level":0,"function":"nix::Store::parseStorePath","file":"/nix/store/aaa-source/src/libstore/include/nix/store/store-api.hh","fullname":null,"line":300,"pc":"0x401000","object":"/bin/nix","arguments":[],"locals":[],"expressions":[],"evaluated":[]}
        ]"#,
        );
        assert_eq!(chosen(&own_header), Some(0));
    }

    /// With none of the program's own code on the stack, the innermost
    /// user-space frame with a name is chosen; kernel frames never are,
    /// and an empty stack has none.
    #[test]
    fn without_the_program_s_code_the_innermost_named_user_frame_is_chosen() {
        let stack = frames(
            r#"[
            {"level":0,"function":"rewind_emit","file":"arch/x86/kernel/cpu/rewind.c","fullname":null,"line":230,"pc":"0xffffffff81285085","object":"/nix/store/aaa-kernel/vmlinux","arguments":[],"locals":[],"expressions":[],"evaluated":[]},
            {"level":1,"function":null,"file":null,"fullname":null,"line":null,"pc":"0x401913","object":null,"arguments":[],"locals":[],"expressions":[],"evaluated":[]},
            {"level":2,"function":"__syscall_cp_c","file":null,"fullname":null,"line":null,"pc":"0x401920","object":"/newroot/src/big","arguments":[],"locals":[],"expressions":[],"evaluated":[]},
            {"level":3,"function":"write","file":null,"fullname":null,"line":null,"pc":"0x40155c","object":"/newroot/src/big","arguments":[],"locals":[],"expressions":[],"evaluated":[]}
        ]"#,
        );
        assert_eq!(chosen(&stack), Some(2));
        assert_eq!(chosen(&[]), None);
    }

    /// With none of the program's own code on the stack but a library's
    /// source lines, the innermost frame with a line outside the C library
    /// is chosen over the C library's: overmind's main thread starting an
    /// OS thread for Go's runtime, on a stack that does not lead back to a
    /// goroutine.
    #[test]
    fn without_the_program_s_code_a_library_line_beats_the_c_library() {
        let stack = frames(
            r#"[
            {"level":0,"function":"__GI___clone3","file":"../sysdeps/unix/sysv/linux/x86_64/clone3.S","fullname":null,"line":62,"pc":"0x7f0000001000","object":"/nix/store/ccc-glibc-2.44/lib/libc.so.6","arguments":[],"locals":[],"expressions":[],"evaluated":[]},
            {"level":1,"function":"__pthread_create_2_1","file":"pthread_create.c","fullname":null,"line":858,"pc":"0x7f0000002000","object":"/nix/store/ccc-glibc-2.44/lib/libc.so.6","arguments":[],"locals":[],"expressions":[],"evaluated":[]},
            {"level":2,"function":"_cgo_try_pthread_create","file":null,"fullname":null,"line":null,"pc":"0x500000","object":"/nix/store/ddd-overmind/bin/overmind","arguments":[],"locals":[],"expressions":[],"evaluated":[]},
            {"level":3,"function":"runtime.asmcgocall","file":"runtime/asm_amd64.s","fullname":null,"line":995,"pc":"0x470000","object":"/nix/store/ddd-overmind/bin/overmind","arguments":[],"locals":[],"expressions":[],"evaluated":[]},
            {"level":4,"function":"runtime.newproc","file":"./<autogenerated>","fullname":null,"line":1,"pc":"0x440000","object":"/nix/store/ddd-overmind/bin/overmind","arguments":[],"locals":[],"expressions":[],"evaluated":[]}
        ]"#,
        );
        assert_eq!(chosen(&stack), Some(3));
    }

    /// The thread looked at: the step's event's by default, a process
    /// given alone in the event's thread when the event is its own and in
    /// its main thread otherwise, and a thread given alone in the process
    /// the trace saw it in last by the step, as thread ids are given out
    /// again. At a step with no event, or the kernel's, it is the thread on
    /// the CPU. Builds a trace of a few writes by hand.
    #[test]
    fn a_process_s_own_exit_is_looked_at_a_step_before() {
        // At its own exit a process's memory is gone, so its frames are
        // looked at the step before, the last it had it; a thread's exit
        // leaves the process its memory, and any other step stays.
        let event = |step, pid, tid, kind| rewind_trace::Event {
            step,
            pid,
            tid,
            kind,
        };
        let exit = |thread| rewind_trace::EventKind::Exit {
            status: 15,
            comm: "waiter".into(),
            thread,
        };
        let trace = Trace {
            events: vec![
                event(
                    6299,
                    140,
                    140,
                    rewind_trace::EventKind::Signal {
                        signo: 15,
                        code: 0,
                        addr: 0,
                    },
                ),
                event(6300, 140, 140, exit(false)),
                event(6310, 150, 151, exit(true)),
            ],
            ..Default::default()
        };
        assert_eq!(alive_step(&trace, 6300, 140), 6299);
        assert_eq!(alive_step(&trace, 6299, 140), 6299);
        assert_eq!(alive_step(&trace, 6310, 150), 6310);
        assert_eq!(alive_step(&trace, 6300, 141), 6300);
    }

    /// A process killed by a signal is looked at its last event before the
    /// kill: the kernel tears it down before its exit is recorded, and in a
    /// sandbox whose last process exited it is gone from /proc steps
    /// earlier, as tmux's server was when bash ended a job at step 2098 and
    /// the server's SIGKILL exit came at 2102. A process that called exit
    /// is still looked at the step before. Builds the trace by hand.
    #[test]
    fn a_killed_process_is_looked_at_its_last_event() {
        let event = |step, pid, kind| rewind_trace::Event {
            step,
            pid,
            tid: pid,
            kind,
        };
        let exit = |status| rewind_trace::EventKind::Exit {
            status,
            comm: "tmux: server".into(),
            thread: false,
        };
        let sigchld = rewind_trace::EventKind::Signal {
            signo: 17,
            code: 2,
            addr: 0,
        };
        let killed_by_sigkill = 9;
        let exited_0 = 0;
        let trace = Trace {
            events: vec![
                event(2056, 40, sigchld.clone()),
                event(2098, 34, exit(exited_0)),
                event(2102, 40, exit(killed_by_sigkill)),
                event(2200, 50, sigchld),
                event(2300, 50, exit(exited_0)),
                event(2400, 60, exit(killed_by_sigkill)),
            ],
            ..Default::default()
        };
        assert_eq!(alive_step(&trace, 2102, 40), 2056);
        assert_eq!(alive_step(&trace, 2098, 34), 2097);
        assert_eq!(alive_step(&trace, 2300, 50), 2299);

        // Killed with no event of its own before, the step before it is.
        assert_eq!(alive_step(&trace, 2400, 60), 2399);
    }

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
            ..Default::default()
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
            ..Default::default()
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
            {"level":0,"function":"run_job","file":"src/pool.c","fullname":"/s/pool.c","line":2,"pc":"0x1","object":null,"arguments":[],"locals":[],"expressions":[],"evaluated":[]},
            {"level":1,"function":"worker","file":"src/pool.c","fullname":"/s/pool.c","line":5,"pc":"0x2","object":null,"arguments":[],"locals":[],"expressions":[],"evaluated":[]},
            {"level":2,"function":"start_thread","file":"pthread_create.c","fullname":"/gone.c","line":9,"pc":"0x3","object":null,"arguments":[],"locals":[],"expressions":[],"evaluated":[]},
            {"level":3,"function":"__clone3","file":null,"fullname":null,"line":null,"pc":"0x4","object":"/lib/libc.so.6","arguments":[],"locals":[],"expressions":[],"evaluated":[]}
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
            {"level":0,"function":"inner","file":"big.c","fullname":"/s/big.c","line":10,"pc":"0x1","object":null,"arguments":[],"locals":[],"expressions":[],"evaluated":[]},
            {"level":1,"function":"outer","file":"big.c","fullname":"/s/big.c","line":8,"pc":"0x2","object":null,"arguments":[],"locals":[],"expressions":[],"evaluated":[]},
            {"level":2,"function":"main","file":"edge.c","fullname":"/s/edge.c","line":19,"pc":"0x3","object":null,"arguments":[],"locals":[],"expressions":[],"evaluated":[]},
            {"level":3,"function":"past","file":"short.c","fullname":"/s/short.c","line":40,"pc":"0x4","object":null,"arguments":[],"locals":[],"expressions":[],"evaluated":[]}
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

    /// A look just after an access says so in its header: which access,
    /// of how many bytes where, among the step's.
    #[test]
    fn a_look_after_an_access_says_which() {
        let after = |kind, hit| Sought {
            value: rewind_core::accesses::Value {
                address: 0x5556f833c068,
                len: 8,
            },
            kind,
            hit,
        };
        assert_eq!(
            moment_words(Some(after(rewind_core::accesses::Kind::Write, 1))),
            ", just after its write of the 8 bytes at 0x5556f833c068"
        );
        assert_eq!(
            moment_words(Some(after(rewind_core::accesses::Kind::Read, 3))),
            ", just after its 3rd access to the 8 bytes at 0x5556f833c068 in the step"
        );
        assert_eq!(moment_words(None), "");
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
            after: None,
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

    /// The chosen frame's arguments and locals follow its source, as gdb's
    /// `info args` and `info locals` name them; a caller's are left out,
    /// and a frame with neither shows no heading for them.
    #[test]
    fn the_answer_lists_the_frame_s_arguments_and_locals() {
        let mut stack = frames(IN_GLIBC);
        stack[2].arguments = vec![Variable {
            name: "arg".into(),
            value: "0x4c6a20".into(),
            location: Location::OptimizedOut,
        }];
        stack[2].locals = vec![
            Variable {
                name: "p".into(),
                value: "0x4c6a20".into(),
                location: Location::OptimizedOut,
            },
            Variable {
                name: "job".into(),
                value: "<optimized out>".into(),
                location: Location::OptimizedOut,
            },
        ];
        stack[3].locals = vec![Variable {
            name: "pd".into(),
            value: "0x7f6158000b70".into(),
            location: Location::OptimizedOut,
        }];
        let whose = Whose {
            pid: 166,
            process: "test_pool_shutdown",
            tid: 168,
            step: 5017,
            after: None,
        };
        assert_eq!(
            render(&whose, &stack, Some(2), None, 1),
            "process 166 (test_pool_shutdown), thread 168, at step 5017\n\
             #2 worker (src/pool.c:70)\n\
             arguments:\n\
             \x20 arg = 0x4c6a20\n\
             locals:\n\
             \x20 p = 0x4c6a20\n\
             \x20 job = <optimized out>\n\
             called from #3 start_thread (./nptl/pthread_create.c:448)\n"
        );

        // Locals alone: no arguments heading.
        stack[2].arguments.clear();
        assert!(
            !render(&whose, &stack, Some(2), None, 1).contains("arguments:"),
            "a frame without arguments shows no heading for them"
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
            after: None,
        };
        let stack = frames(
            r#"[{"level":0,"function":null,"file":null,"fullname":null,"line":null,"pc":"0x401913","object":null,"arguments":[],"locals":[],"expressions":[],"evaluated":[]}]"#,
        );
        assert_eq!(
            render(&whose, &stack, None, None, 2),
            "process 34 (big), thread 34, at step 310\n\
             no frame names a function; the stack was:\n\
             #0 0x401913\n"
        );
    }
}
