//! `rewind blame RUN --at STEP EXPR`: who wrote a value in a process's
//! memory, the same question blame asks of a file, asked of memory.
//!
//! gdb, on a fork at the step, evaluates each expression in the thread's
//! frame in the program's own code, as `rewind where` picks it, and says
//! where it is. A value in memory and at most 32 bytes can be watched:
//! rewind-core's accesses module replays the run back from the step with
//! the value in the CPU's debug registers, a keyframe window at a time,
//! for its last writes, or with `--reads` for every access since the
//! process began. A second gdb session, on a fork at the last write's
//! step, names each access's source line and where the process's other
//! threads were then. The same gdb serves both sessions when the process
//! made no exec between the two steps, so its files are read and loaded
//! once.

use std::process::ExitCode;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use rewind_core::accesses::{self, Kind, Search, Value, Wanted};
use rewind_core::{Home, Run};
use rewind_trace::blamed::{
    AccessKind, Blamed, BlamedAccess, Listed, SourcePlace, ThreadAt, Watched,
};
use rewind_trace::located::{Frame, ValueClass};
use rewind_trace::searching::{Phase, Searching};

use crate::gdb::{self, KeptGdb, Needs, Say, Symbols};
use crate::locate::{self, Pick, PlaceAt};
use crate::progress::Format;

/// Where x86-64's lower half, user space, ends: an access made at a pc
/// past it was the kernel's.
const USER_END: u64 = 0x0000_8000_0000_0000;

/// The most accesses `--reads` lists by default, the latest ones.
pub(crate) const DEFAULT_MAX: usize = 1000;

/// What `rewind blame --at` is asked.
pub(crate) struct Asked<'a> {
    pub(crate) run: &'a str,
    pub(crate) step: u64,
    pub(crate) expressions: &'a [String],
    pub(crate) thread: (Option<u32>, Option<u32>),
    pub(crate) frame: Option<u32>,
    /// A value by its address, size and class, instead of expressions,
    /// for a program that has already asked `rewind where` about it.
    pub(crate) address: Option<(u64, u64, ValueClass)>,
    pub(crate) wanted: Wanted,
    pub(crate) since: Option<u64>,
    pub(crate) json: bool,
    pub(crate) progress: Format,
}

/// The values to watch, the process and thread they were named in, the
/// step they were named at, and the frame's place.
struct Named {
    values: Vec<Watched>,
    pid: u32,
    tid: u32,
    step: u64,
    place: Option<SourcePlace>,
}

/// The gdb that named the values, kept running with the symbols it
/// loaded: those of process `pid` as its memory was from step `began`,
/// its last exec or its fork, on.
struct Kept {
    gdb: KeptGdb,
    pid: u32,
    began: u64,
}

/// Where a session at the last write's step gets gdb from: the one that
/// named the values, or one to start with symbols read there.
enum GdbFrom {
    Kept(KeptGdb),
    Loaded(Symbols),
}

/// Runs `rewind blame --at`.
pub(crate) fn blame(home: &Home, asked: Asked<'_>) -> Result<ExitCode> {
    let started = Instant::now();
    let run = Run::find(home, asked.run)?;
    let step = run.check_step(asked.step)?;
    super::inspect::warn_of_machine(&run, step);
    let (named, naming) = name_values(home, &run, step, &asked)?;
    let since = match asked.since {
        Some(since) => since,
        None => run.process_began(named.pid, named.step)?,
    };

    // The search, newest window first, saying how far it has got.
    let search = Search {
        pid: named.pid,
        values: named
            .values
            .iter()
            .map(|w| Value {
                address: parse_hex(&w.address).expect("an address rewind wrote"),
                len: w.size,
            })
            .collect(),
        before: named.step,
        since,
        wanted: asked.wanted,
    };
    let mut say = Saying::new(asked.progress);
    let (found, at_write) = accesses::search_keeping(home, &run, &search, &mut |s| say.heard(s))?;
    say.naming(&found, named.step);

    let kept = &found.accesses;

    // Each access's source line and the other threads just after the
    // last write, from one gdb session on a fork run into that write's
    // step to the write.
    let last_write = kept.iter().rev().find(|a| a.kind == Kind::Write).cloned();
    let user_pcs: Vec<u64> = kept
        .iter()
        .filter(|a| a.after_pc < USER_END)
        .map(|a| a.after_pc - 1)
        .collect();
    let context = match &last_write {
        Some(write) => Some(context_at(
            home,
            &run,
            named.pid,
            (write, search.values[write.value], at_write),
            (&user_pcs, naming),
        )?),
        None if !user_pcs.is_empty() => Some(context_at_step(
            home,
            &run,
            (named.pid, named.step),
            (&[], None, None),
            (&user_pcs, naming),
        )?),
        None => None,
    };
    let (places, threads) = match context {
        Some(context) => (context.places, context.threads),
        None => (Vec::new(), Vec::new()),
    };

    // The answer.
    let mut values = named.values;
    for (i, value) in values.iter_mut().enumerate() {
        let class = value.class;
        let show = |bytes: Option<&Option<Vec<u8>>>| bytes?.as_ref().map(|b| class.show(b));
        value.initial = show(found.initial.get(i));
        value.now = value.now.take().or_else(|| show(found.now.get(i)));
    }
    let mut user = places.into_iter();
    let accesses = kept
        .iter()
        .map(|a| {
            let class = values[a.value].class;
            let in_user = a.after_pc < USER_END;
            BlamedAccess {
                step: a.step,
                value: a.value,
                kind: match a.kind {
                    Kind::Read => AccessKind::Read,
                    Kind::Write => AccessKind::Write,
                },
                tid: a.tid,
                thread: a.thread.clone(),
                before: a.before.as_ref().map(|b| class.show(b)),
                after: a.after.as_ref().map(|b| class.show(b)),
                pc: in_user.then(|| format!("{:#x}", a.after_pc - 1)),
                kernel: (!in_user)
                    .then(|| {
                        rewind_core::run::kernel_symbol_at(&run.manifest.spec.kernel, a.after_pc)
                    })
                    .flatten(),
                syscall: a.syscall,
                place: if in_user {
                    user.next().map(source_place)
                } else {
                    None
                },
                hit: a.hit,
            }
        })
        .collect();
    let blamed = Blamed {
        run: run.manifest.id.to_string(),
        step: named.step,
        pid: named.pid,
        tid: named.tid,
        place: named.place,
        values,
        accesses,
        threads,
        searched_from: found.searched_from,
        since,
        windows: found.windows,
        listed: match asked.wanted {
            Wanted::LastWrites(_) => Listed::LastWrites,
            Wanted::Accesses(_) => Listed::Accesses,
        },
        dropped: found.dropped,
        elapsed_ms: started.elapsed().as_millis() as u64,
    };
    if asked.json {
        println!("{}", serde_json::to_string(&blamed)?);
    } else {
        print!("{}", render(&blamed));
    }
    Ok(ExitCode::SUCCESS)
}

/// The values `asked` names at `step`: by address, or each expression as
/// gdb evaluates it in the thread's chosen frame, with that gdb kept.
fn name_values(
    home: &Home,
    run: &Run,
    step: u64,
    asked: &Asked<'_>,
) -> Result<(Named, Option<Kept>)> {
    if let Some((address, size, class)) = asked.address {
        let (pid, tid) =
            match locate::thread_at(run.trace()?, step, asked.thread.0, asked.thread.1)? {
                Pick::Thread { pid, tid } => (pid, tid),
                Pick::OnTheCpu => {
                    bail!("at step {step} no process's event names a thread; give --pid")
                }
            };
        let value = Watched {
            text: format!("{address:#x}"),
            address: format!("{address:#x}"),
            size,
            class,
            type_name: None,
            now: None,
            initial: None,
        };
        let named = Named {
            values: vec![value],
            pid,
            tid,
            step,
            place: None,
        };
        return Ok((named, None));
    }

    if asked.expressions.is_empty() {
        bail!("name a value to blame, such as 'p->queue', or give --address");
    }
    let (answer, gdb) =
        locate::located_keeping(home, run, step, asked.thread, asked.expressions, None)?;
    let answer = &answer;
    let index = match asked.frame {
        Some(level) => answer
            .frames
            .iter()
            .position(|f| f.level == level)
            .with_context(|| format!("thread {} has no frame #{level}", answer.tid))?,
        None => answer.chosen.unwrap_or(0),
    };
    let frame = answer
        .frames
        .get(index)
        .with_context(|| format!("gdb found no frames for thread {}", answer.tid))?;
    let mut values = Vec::new();
    for (i, text) in asked.expressions.iter().enumerate() {
        let evaluated = frame.evaluated.get(i).cloned().flatten().with_context(|| {
            format!(
                "gdb cannot evaluate {text} in frame #{} ({})",
                frame.level,
                frame.function_label()
            )
        })?;
        let Some((address, size)) = evaluated.location.watchable() else {
            let why = evaluated.location.why_not().unwrap_or_default();
            bail!("{text} is {why}.{}", suggestion(frame));
        };
        let rewind_trace::located::Location::Memory {
            class, type_name, ..
        } = &evaluated.location
        else {
            unreachable!("a watchable value is in memory");
        };
        values.push(Watched {
            text: text.clone(),
            address: format!("{address:#x}"),
            size,
            class: *class,
            type_name: Some(type_name.clone()),
            now: Some(evaluated.value),
            initial: None,
        });
    }
    let named = Named {
        values,
        pid: answer.pid,
        tid: answer.tid,
        step: answer.step,
        place: Some(SourcePlace {
            function: frame.function.clone(),
            file: frame.file.clone(),
            line: frame.line,
            text: None,
        }),
    };
    let kept = Kept {
        gdb,
        pid: answer.pid,
        began: run.process_began(answer.pid, answer.step)?,
    };
    Ok((named, Some(kept)))
}

/// A hint, after a value that cannot be watched, at ones in the frame
/// that can: on the frame's own line first.
fn suggestion(frame: &Frame) -> String {
    let watchable = |line: Option<u32>| -> Vec<&str> {
        let mut texts: Vec<&str> = frame
            .expressions
            .iter()
            .filter(|e| line.is_none_or(|l| e.line == l) && e.location.watchable().is_some())
            .map(|e| e.text.as_str())
            .collect();
        texts.dedup();
        texts
    };
    let on_line = watchable(frame.line);
    let texts = if on_line.is_empty() {
        watchable(None)
    } else {
        on_line
    };
    match texts.as_slice() {
        [] => String::new(),
        [one] => format!("\n{one} is in memory and can be blamed."),
        many => format!(
            "\nThese are in memory and can be blamed: {}.",
            many.join(", ")
        ),
    }
}

/// Places of addresses, and where the other threads were.
struct Surroundings {
    places: Vec<PlaceAt>,
    threads: Vec<ThreadAt>,
}

/// The places of `pcs`, and where process `pid`'s threads other than the
/// writer's were at `write`'s step, with the gdb `kept` when it fits.
fn context_at(
    home: &Home,
    run: &Run,
    pid: u32,
    (write, value, machine): (&accesses::Access, Value, Option<rewind_vmm::Machine>),
    (pcs, kept): (&[u64], Option<Kept>),
) -> Result<Surroundings> {
    let after = accesses::Sought {
        value,
        kind: Kind::Write,
        hit: write.hit,
    };
    context_at_step(
        home,
        run,
        (pid, write.step),
        (&[write.tid], Some(after), machine),
        (pcs, kept),
    )
}

/// The places of `pcs`, and where process `pid`'s threads, but those in
/// `leave_out`, were in `step`: at its start, or just after the access
/// `after` names; on `machine`, the search's at the step, when it kept
/// one, else on a fork made there; with `kept`, the gdb that named the
/// values, when its symbols hold at the step, else with symbols read
/// there.
fn context_at_step(
    home: &Home,
    run: &Run,
    (pid, step): (u32, u64),
    (leave_out, after, machine): (
        &[u32],
        Option<accesses::Sought>,
        Option<rewind_vmm::Machine>,
    ),
    (pcs, kept): (&[u64], Option<Kept>),
) -> Result<Surroundings> {
    let needs = Needs::Tasks("`rewind blame --at` cannot find the threads".into());

    // The naming's symbols hold at the step when the process's memory is
    // the same program's there: the same process, with no exec between.
    // A library loaded or unloaded between the two steps is not looked
    // for.
    let began = run.process_began(pid, step)?;
    let kept = kept
        .filter(|k| k.pid == pid && k.began == began)
        .map(|k| k.gdb);

    // A machine at the step, run to the access: the search's, when it
    // kept one there, else a fork. Without a kept gdb, the symbols'
    // inspection runs on a fork of its own beside it.
    let at_step = |machine: &rewind_vmm::Machine| -> Result<bool> {
        Ok(machine.step() == step && machine.task_layout()?.is_some())
    };
    let (mut machine, loaded) = match machine {
        Some(mut machine) if at_step(&machine)? => {
            if let Some(sought) = after {
                accesses::run_to_access(&mut machine, pid, step, sought)?;
            }
            (machine, None)
        }
        _ if kept.is_some() => {
            let mut machine = gdb::fork(home, run, step, needs)?;
            if let Some(sought) = after {
                accesses::run_to_access(&mut machine, pid, step, sought)?;
            }
            (machine, None)
        }
        _ => {
            let (machine, symbols, _) =
                locate::fork_and_symbols(home, run, (pid, step), needs, after)?;
            (machine, Some(symbols))
        }
    };

    // gdb with the process's symbols as they were mapped at the access: a
    // library loaded earlier in the step, such as the one that made the
    // access, is mapped only by then. The naming's, else read on the
    // machine, which the inspection puts back as it was, so no keyframe
    // is restored.
    let from = match (loaded, kept) {
        (Some(symbols), _) => GdbFrom::Loaded(symbols),
        (None, Some(gdb)) => GdbFrom::Kept(gdb),
        (None, None) => {
            let symbols = Symbols::load_on(home, run, (step, &mut machine), pid, Say::Aloud)?;
            GdbFrom::Loaded(symbols)
        }
    };
    let tids: Vec<u32> = match machine.task_layout()? {
        Some(layout) if !leave_out.is_empty() => {
            let tasks = rewind_core::threads::Tasks::new(&machine, layout);
            match tasks.process(pid)? {
                Some(task) => tasks
                    .threads(task)?
                    .into_iter()
                    .map(|t| t.tid)
                    .filter(|tid| !leave_out.contains(tid))
                    .collect(),
                None => Vec::new(),
            }
        }
        _ => Vec::new(),
    };

    // The threads and places from a session on the fork.
    let scope = rewind_core::debug::Scope::Process(pid);
    let mut debuggee = gdb::debuggee(run, step, machine, scope)?;
    let mut gdb = match from {
        GdbFrom::Kept(gdb) => gdb,
        GdbFrom::Loaded(symbols) => KeptGdb::start(symbols, &debuggee, Say::Aloud)?,
    };
    let at = locate::threads_at(&mut debuggee, &mut gdb, (&tids, pcs))?;
    let threads = at
        .threads
        .into_iter()
        .map(|(tid, walked)| match walked {
            Ok((frames, chosen)) => {
                let frame = chosen
                    .or((!frames.is_empty()).then_some(0))
                    .map(|i| &frames[i]);
                ThreadAt {
                    tid,
                    place: frame.map(|f| SourcePlace {
                        function: f.function.clone(),
                        file: f.file.clone(),
                        line: f.line,
                        text: None,
                    }),
                    innermost: frames.first().map(Frame::function_label),
                    error: None,
                }
            }
            Err(error) => ThreadAt {
                tid,
                place: None,
                innermost: None,
                error: Some(error),
            },
        })
        .collect();
    Ok(Surroundings {
        places: at.places,
        threads,
    })
}

fn source_place(place: PlaceAt) -> SourcePlace {
    SourcePlace {
        function: place.function,
        file: place.file,
        line: place.line,
        text: place.text.map(|t| t.trim().to_string()),
    }
}

/// An address in hex, with or without 0x.
pub(crate) fn parse_hex(text: &str) -> Option<u64> {
    u64::from_str_radix(text.trim_start_matches("0x"), 16).ok()
}

/// How a search says how far it has got: a line of JSON each time for a
/// program, a status line on a terminal, else nothing.
struct Saying {
    format: Format,
}

impl Saying {
    fn new(format: Format) -> Saying {
        Saying { format }
    }

    fn heard(&mut self, s: &Searching) {
        match self.format {
            Format::Json => eprintln!("{}", s.line()),
            Format::Auto => crate::status(&status_line(s)),
            Format::Off => {}
        }
    }

    /// Says the search is done and its accesses are being named.
    fn naming(&mut self, found: &accesses::Found, before: u64) {
        match self.format {
            Format::Json => eprintln!("{}", naming_line(found, before).line()),
            Format::Auto => crate::clear_status(),
            Format::Off => {}
        }
    }
}

/// The progress line that says a search before step `before` is done
/// and its accesses are being named.
fn naming_line(found: &accesses::Found, before: u64) -> Searching {
    let writes = found
        .accesses
        .iter()
        .filter(|a| a.kind == Kind::Write)
        .count();
    Searching {
        phase: Phase::Naming,
        before,
        window: (found.searched_from, before),
        reached: before,
        steps: before - found.searched_from,
        windows: found.windows,
        writes: writes as u64,
        reads: (found.accesses.len() - writes) as u64,
        elapsed_ms: found.elapsed_ms,
    }
}

/// The status line for a search.
fn status_line(s: &Searching) -> String {
    match s.phase {
        Phase::Restoring => format!(
            "rewind: searching back from step {}: restoring step {}",
            s.before, s.window.0
        ),
        Phase::Searching | Phase::Naming => format!(
            "rewind: searching back from step {}: {} steps searched, {}",
            s.before,
            s.steps,
            counted(s.writes, "write")
        ),
    }
}

/// A place as `file:line`, by the file's last part.
fn file_line(place: &SourcePlace) -> Option<String> {
    let file = place.file.as_ref()?;
    let name = file.rsplit('/').next().unwrap_or(file);
    Some(format!("{name}:{}", place.line?))
}

/// An access's place in words: its file and line, function and the
/// line's text; or the kernel's symbol and the system call.
fn access_place(access: &BlamedAccess) -> String {
    if let Some(symbol) = &access.kernel {
        return match access.syscall {
            Some(n) => format!("the kernel, in {symbol}, for system call {n}"),
            None => format!("the kernel, in {symbol}"),
        };
    }
    let Some(place) = &access.place else {
        return access.pc.clone().unwrap_or_default();
    };
    let mut parts = Vec::new();
    parts.extend(file_line(place));
    parts.extend(place.function.clone());
    parts.extend(place.text.clone());
    if parts.is_empty() {
        return access.pc.clone().unwrap_or_default();
    }
    parts.join("  ")
}

/// `n` of `thing`, with a plural s when not one.
fn counted(n: u64, thing: &str) -> String {
    match n {
        1 => format!("1 {thing}"),
        n => format!("{n} {thing}s"),
    }
}

/// The text answer.
pub(crate) fn render(blamed: &Blamed) -> String {
    let mut out = String::new();
    let here = blamed.place.as_ref().and_then(file_line);
    for (i, value) in blamed.values.iter().enumerate() {
        let now = value.now.as_deref().unwrap_or("unreadable");
        let mut head = format!(
            "{} = {now}   {} bytes at {}",
            value.text, value.size, value.address
        );
        head.push_str(&format!(", thread {}", blamed.tid));
        if let Some(here) = &here {
            head.push_str(&format!(", {here}"));
        }
        out.push_str(&head);
        out.push('\n');
        if blamed.listed == Listed::Accesses {
            continue;
        }

        // Its writes, newest first.
        let writes: Vec<&BlamedAccess> = blamed
            .accesses
            .iter()
            .rev()
            .filter(|a| a.value == i)
            .collect();
        if writes.is_empty() {
            out.push_str(&format!(
                "\nnot written since step {}, where the search began; it held {} there\n",
                blamed.searched_from,
                value.initial.as_deref().unwrap_or("nothing readable")
            ));
        }
        for (n, write) in writes.iter().enumerate() {
            let lead = if n == 0 {
                "last written"
            } else {
                "before that, written"
            };
            out.push_str(&format!(
                "\n{lead} at step {} by thread {} ({})\n  {}\n",
                write.step,
                write.tid,
                write.thread,
                access_place(write)
            ));
            if let Some(before) = &write.before {
                out.push_str(&format!("  was {before}\n"));
            }
        }
    }

    // Every access in order, when reads are listed.
    if blamed.listed == Listed::Accesses && blamed.accesses.is_empty() {
        out.push_str(&format!(
            "\nnot read or written since step {}, where the search began\n",
            blamed.searched_from
        ));
    }
    if blamed.listed == Listed::Accesses && !blamed.accesses.is_empty() {
        out.push_str("\n  step  thread           access  value               where\n");
        for a in &blamed.accesses {
            let kind = match a.kind {
                AccessKind::Read => "read",
                AccessKind::Write => "write",
            };
            let value = a.after.as_deref().unwrap_or("?");
            let who = format!("{} {}", a.tid, a.thread);
            out.push_str(&format!(
                "{:>6}  {who:<16} {kind:<7} {value:<19} {}  {}\n",
                a.step,
                access_place(a),
                blamed.values[a.value].text
            ));
        }
    }

    // Where the other threads were at the last write.
    if let Some(write) = blamed.last_write()
        && !blamed.threads.is_empty()
    {
        out.push_str(&format!(
            "\nother threads just after the write, in step {}:\n",
            write.step
        ));
        for t in &blamed.threads {
            let place = t.place.as_ref();
            let line = place.and_then(file_line).unwrap_or_default();
            let function = place.and_then(|p| p.function.clone()).unwrap_or_default();
            let innermost = t
                .innermost
                .clone()
                .filter(|f| *f != function)
                .unwrap_or_default();
            let words = match &t.error {
                Some(error) => error.clone(),
                None => format!("{line}  {function}  {innermost}")
                    .trim_end()
                    .to_string(),
            };
            out.push_str(&format!("  thread {}  {words}\n", t.tid));
        }
    }

    // How far back the search went.
    let writes = blamed
        .accesses
        .iter()
        .filter(|a| a.kind == AccessKind::Write)
        .count();
    let reads = blamed.accesses.len() - writes;
    let mut seen = counted(writes as u64, "write");
    if reads > 0 {
        seen.push_str(&format!(", {}", counted(reads as u64, "read")));
    }
    let windows = match blamed.windows {
        1 => "1 keyframe window".to_string(),
        n => format!("{n} keyframe windows"),
    };
    out.push_str(&format!(
        "\nsearched back to step {}, {windows}, {seen}, {:.1}s\n",
        blamed.searched_from,
        blamed.elapsed_ms as f64 / 1000.0
    ));
    if blamed.dropped > 0 {
        let latest = match blamed.accesses.len() {
            1 => "only the latest access is listed".to_string(),
            n => format!("only the latest {n} accesses are listed"),
        };
        out.push_str(&format!(
            "{latest}: {} before them were left out, and the search went no further back; \
             give --max to list more or --since to search less\n",
            blamed.dropped
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    // Which accesses the answer keeps, from made-up accesses to two values.
    use super::*;

    fn access(step: u64, value: usize, kind: Kind) -> accesses::Access {
        accesses::Access {
            step,
            value,
            kind,
            after_pc: 0x401000,
            tid: 1,
            thread: "t".into(),
            before: None,
            after: None,
            syscall: None,
            hit: 1,
        }
    }

    /// The line that says naming has begun carries how long the search
    /// took, from a made-up search of one write and one read.
    #[test]
    fn the_naming_line_says_how_long_the_search_took() {
        let found = accesses::Found {
            accesses: vec![access(10, 0, Kind::Write), access(11, 0, Kind::Read)],
            searched_from: 4,
            initial: Vec::new(),
            now: Vec::new(),
            windows: 2,
            dropped: 0,
            elapsed_ms: 1234,
        };
        let line = naming_line(&found, 20);
        assert_eq!(line.phase, Phase::Naming);
        assert_eq!(line.window, (4, 20));
        assert_eq!((line.writes, line.reads), (1, 1));
        assert_eq!(line.elapsed_ms, 1234);
    }

    /// Counts read as words: one write, two reads.
    #[test]
    fn counts_take_a_plural_past_one() {
        assert_eq!(counted(1, "write"), "1 write");
        assert_eq!(counted(2, "read"), "2 reads");
        assert_eq!(counted(0, "write"), "0 writes");
    }
}
