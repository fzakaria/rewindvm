//! Text renderings of runs, events and processes for the terminal.

use std::fmt::Write;
use std::time::Duration;

use rewind_core::Run;
use rewind_trace::{Event, EventKind, Trace, signal_name};

/// A one-line summary for `rewind ls`.
pub fn summary(run: &Run) -> String {
    let m = &run.manifest;
    let outcome = match &m.outcome {
        Some(o) => format!("{:<9} {:>12} steps", status(o.status), o.step),
        None => "running or interrupted".into(),
    };
    format!("{}  {outcome}  {}", m.id, m.name)
}

/// The line printed when a run finishes.
pub fn finished(run: &Run) -> String {
    let m = &run.manifest;
    let Some(o) = &m.outcome else {
        return format!("rewind: run {} did not finish", m.id);
    };
    format!(
        "rewind: run {} {} after {} steps, {:.3}s virtual, {:.3}s wall ({})",
        m.id,
        status(o.status),
        o.step,
        Duration::from_nanos(o.virtual_ns).as_secs_f64(),
        Duration::from_millis(o.wall_ms).as_secs_f64(),
        o.stop,
    )
}

/// How a run ended, as something two runs can be compared by: the exit
/// status and the hash of every output.
pub fn outcome_key(run: &Run) -> anyhow::Result<(Option<i32>, Vec<String>)> {
    let status = run.manifest.outcome.as_ref().and_then(|o| o.status);
    let mut outputs = Vec::new();
    for e in run.trace()?.events {
        if let EventKind::Mark { text } = e.kind {
            if let Some(rest) = text.strip_prefix(rewind_init::OUTPUT_MARK) {
                outputs.push(rest.to_string());
            }
        }
    }
    Ok((status, outputs))
}

/// The step the job started on, from init's start mark; 0 if there is
/// none.
pub fn start_step(trace: &Trace) -> u64 {
    trace
        .events
        .iter()
        .find(|e| matches!(&e.kind, EventKind::Mark { text } if text == rewind_init::START_MARK))
        .map_or(0, |e| e.step)
}

/// The command line of the process a failed run's failure came from: the
/// first to receive a fatal signal, else the first to exit non-zero.
pub fn culprit(trace: &Trace) -> Option<Vec<String>> {
    trace.culprit()
}

/// Where one program's own events first differ between two runs (see
/// `Trace::divergence_in`), with the events around it on each side.
pub fn divergence_in(left: &Trace, right: &Trace, argv: &[String]) -> String {
    let Some(d) = left.divergence_in(right, argv) else {
        return "  no difference in its own events\n".into();
    };
    let i = d.position;

    const BEFORE: usize = 3;
    const AFTER: usize = 4;
    let mut out = String::new();
    for &e in &d.left.indices[i.saturating_sub(BEFORE)..i] {
        let _ = writeln!(out, "  both  {}", event(&left.events[e]));
    }
    for &e in d.left.indices.iter().skip(i).take(AFTER) {
        let _ = writeln!(out, "  left  {}", event(&left.events[e]));
    }
    for &e in d.right.indices.iter().skip(i).take(AFTER) {
        let _ = writeln!(out, "  right {}", event(&right.events[e]));
    }
    out
}

/// One line per run for `rewind check`.
pub fn outcome_line(run: &Run) -> anyhow::Result<String> {
    let (status_, outputs) = outcome_key(run)?;
    let steps = run.manifest.outcome.as_ref().map_or(0, |o| o.step);
    let hashes: Vec<String> = outputs
        .iter()
        .filter_map(|o| o.split_once(' ').map(|(_, h)| h[..12].to_string()))
        .collect();
    Ok(format!(
        "{:<14} {:>10} steps  {}  run {}",
        status(status_),
        steps,
        hashes.join(" "),
        run.manifest.id
    ))
}

/// A wait status in words.
pub fn status(status: Option<i32>) -> String {
    match status {
        None => "no-status".into(),
        Some(s) if s & 0x7f != 0 => format!("killed:{}", signal_name((s & 0x7f) as u32)),
        Some(s) => match (s >> 8) & 0xff {
            0 => "exited:0".into(),
            code => format!("exited:{code}"),
        },
    }
}

/// The shell convention for a wait status as an exit code.
pub fn exit_code(status: i32) -> u8 {
    if status & 0x7f != 0 {
        128 + (status & 0x7f) as u8
    } else {
        ((status >> 8) & 0xff) as u8
    }
}

/// One event, in the syscall-ish notation the scrubber uses.
pub fn event(e: &Event) -> String {
    let what = match &e.kind {
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
            status: s,
            comm,
            thread,
        } => {
            let who = if *thread { "thread exit" } else { "exit_group" };
            format!("{who}({comm}) {}", status(Some(*s as i32)))
        }
        EventKind::Signal { signo, code, addr } => {
            format!("{} code={code} addr={addr:#x}", signal_name(*signo))
        }
        EventKind::Open { path, flags } => format!("open({path:?}, {flags:#o})"),
        EventKind::Unlink { path } => format!("unlink({path:?})"),
        EventKind::Rename { from, to } => format!("rename({from:?}, {to:?})"),
        EventKind::Mark { text } => format!("mark {text:?}"),
        EventKind::Unknown { kind, data } => format!("record kind {kind}, {} bytes", data.len()),
    };
    format!("{:>10} {:>5}/{:<5} {what}", e.step, e.pid, e.tid)
}

/// Whether `rewind ps` lists the kernel's own threads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KernelThreads {
    Show,
    Hide,
}

/// kthreadd, the parent of every kernel thread.
const KTHREADD: u32 = 2;

/// The process tree alive at a step, indented by parent.
pub fn process_tree(trace: &Trace, step: u64, kernel: KernelThreads) -> String {
    let procs = trace.processes();
    let alive: Vec<_> = procs
        .iter()
        .filter(|p| p.alive_at(step))
        .filter(|p| kernel == KernelThreads::Show || (p.pid != KTHREADD && p.parent != KTHREADD))
        .collect();
    let mut out = String::new();
    let roots = alive
        .iter()
        .filter(|p| !alive.iter().any(|q| q.pid == p.parent));
    for root in roots {
        tree(&alive, root.pid, 0, step, &mut out);
    }
    out
}

fn tree(alive: &[&rewind_trace::Process], pid: u32, depth: usize, step: u64, out: &mut String) {
    let Some(p) = alive.iter().find(|p| p.pid == pid) else {
        return;
    };
    let name = if p.argv.is_empty() {
        "[kernel thread]".to_string()
    } else {
        p.argv.join(" ")
    };
    let _ = writeln!(out, "{:>6} {}{}", p.pid, "  ".repeat(depth), name);
    for (tid, start, end) in &p.threads {
        if *start <= step && end.is_none_or(|e| step < e) {
            let _ = writeln!(out, "{:>6} {}  (thread)", tid, "  ".repeat(depth));
        }
    }
    for child in alive.iter().filter(|c| c.parent == pid) {
        tree(alive, child.pid, depth + 1, step, out);
    }
}

/// Where two runs first differ, with the events on each side.
pub fn divergence(left: &Trace, right: &Trace) -> String {
    let Some(d) = left.divergence(right) else {
        return "identical\n".into();
    };
    let mut out = format!(
        "first difference at event {}: step {} on the left, step {} on the right\n",
        d.index, d.left_step, d.right_step
    );
    let context = d.index.saturating_sub(3)..d.index;
    for e in &left.events[context] {
        let _ = writeln!(out, "  both  {}", event(e));
    }
    const AFTER: usize = 4;
    for e in left.events.iter().skip(d.index).take(AFTER) {
        let _ = writeln!(out, "  left  {}", event(e));
    }
    for e in right.events.iter().skip(d.index).take(AFTER) {
        let _ = writeln!(out, "  right {}", event(e));
    }
    out
}
