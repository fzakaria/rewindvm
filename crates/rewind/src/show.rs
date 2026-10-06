//! Text renderings of runs, events and processes for the terminal.

use std::fmt::Write;
use std::time::Duration;

use rewind_core::Run;
use rewind_core::compare::{Comparison, Source, Verdict};
use rewind_trace::ending::{Ending, ExitStatus};
use rewind_trace::{Event, EventKind, Trace, signal_name};

/// A one-line summary for `rewind ls`.
pub fn summary(run: &Run) -> String {
    let m = &run.manifest;
    let outcome = match &m.outcome {
        Some(o) => format!(
            "{:<9} {:>12} steps",
            Ending::of(&o.stop, o.status, &[]).to_string(),
            o.step
        ),
        None if run.executing() => "running".into(),
        None => "interrupted".into(),
    };
    format!("{}  {outcome}  {}", m.id, m.name)
}

/// The line printed when a run finishes, with how it stopped in the
/// words `stop` gives, else the stop's own.
pub fn finished(run: &Run, stop: Option<&str>) -> String {
    let m = &run.manifest;
    let Some(o) = &m.outcome else {
        return format!("rewind: run {} did not finish", m.id);
    };
    format!(
        "rewind: run {} {} after {} steps, {:.3}s virtual, {:.3}s wall ({})",
        m.id,
        Ending::of(&o.stop, o.status, &[]),
        o.step,
        Duration::from_nanos(o.virtual_ns).as_secs_f64(),
        Duration::from_millis(o.wall_ms).as_secs_f64(),
        stop.map_or_else(|| o.stop.to_string(), str::to_string),
    )
}

/// How a run ended, as something two runs can be compared by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutcomeKey {
    /// The job's wait status.
    pub status: Option<i32>,
    /// Each output init hashed, by path, with its hash.
    pub outputs: Vec<(String, String)>,
}

/// How `run` ended: its exit status and the hash of every output.
pub fn outcome_key(run: &Run) -> anyhow::Result<OutcomeKey> {
    Ok(OutcomeKey {
        status: run.manifest.outcome.as_ref().and_then(|o| o.status),
        outputs: run.trace()?.outputs(),
    })
}

/// The step the job started on, from init's start mark; 0 if there is
/// none.
pub fn start_step(trace: &Trace) -> u64 {
    trace.job_start().unwrap_or(0)
}

/// A file size the way people read one: bytes, KB or MB, in decimal
/// units.
pub fn size(bytes: u64) -> String {
    const KB: f64 = 1e3;
    const MB: f64 = 1e6;
    const GB: f64 = 1e9;
    match bytes as f64 {
        b if b >= GB => format!("{:.1} GB", b / GB),
        b if b >= MB => format!("{:.1} MB", b / MB),
        b if b >= KB => format!("{:.1} KB", b / KB),
        _ => format!("{bytes} bytes"),
    }
}

/// Where two runs first behave differently, as
/// [`rewind_trace::compare::Comparison`] finds it,
/// with the events around it on each side: the culprit program's own
/// events when the comparison is about one, else every event. `this` is
/// the run the comparison was made from, `other` the run it was compared
/// with, each with the word its lines start with.
pub fn comparison(
    (this, this_name): (&Trace, &str),
    (other, other_name): (&Trace, &str),
    c: &rewind_trace::compare::Comparison,
) -> String {
    const BEFORE: usize = 3;
    const AFTER: usize = 4;
    const BOTH: &str = "both";

    let mut out = match &c.program {
        Some(argv) => format!("where {} first behaves differently:\n", argv.join(" ")),
        None => "where the runs first behave differently:\n".to_string(),
    };
    let Some(point) = &c.point else {
        out.push_str("  no difference in what they did\n");
        return out;
    };

    // The events compared on each side, in order.
    let compared = |t: &Trace| -> Vec<usize> {
        match &c.program {
            Some(argv) => t.program_events(argv).indices,
            None => (0..t.events.len()).collect(),
        }
    };
    let (mine, theirs) = (compared(this), compared(other));
    let width = this_name.len().max(other_name.len()).max(BOTH.len());
    let i = point.matched;
    for &e in &mine[i.saturating_sub(BEFORE)..i] {
        let _ = writeln!(out, "  {BOTH:<width$}  {}", event(&this.events[e]));
    }
    for (name, trace, indices) in [(this_name, this, &mine), (other_name, other, &theirs)] {
        for &e in indices.iter().skip(i).take(AFTER) {
            let _ = writeln!(out, "  {name:<width$}  {}", event(&trace.events[e]));
        }
    }
    out
}

/// The outputs the job was to create that init never hashed, from the
/// paths and hashes of [`outcome_key`]. Init hashes outputs only after
/// the job exits 0, so for such a job these are the outputs it did not
/// create.
pub fn missing_outputs(expected: &[String], hashed: &[(String, String)]) -> Vec<String> {
    expected
        .iter()
        .filter(|path| !hashed.iter().any(|(p, _)| p == *path))
        .cloned()
        .collect()
}

/// One line for an output of a Nix run: its path, the start of its NAR
/// hash, and what this machine's store and the caches said about it.
pub fn verdict_line(path: &str, hash: &str, comparisons: &[Comparison]) -> String {
    let named = |verdict: &dyn Fn(&Verdict) -> bool| -> Vec<String> {
        comparisons
            .iter()
            .filter(|c| verdict(&c.verdict))
            .map(|c| source_name(&c.source))
            .collect()
    };
    let matches = named(&|v| *v == Verdict::Matches);
    let differs = named(&|v| *v == Verdict::Differs);
    let absent = named(&|v| *v == Verdict::Absent).len();

    // What had the path, or else how many places did not.
    let mut parts = Vec::new();
    if !matches.is_empty() {
        parts.push(format!("matches {}", matches.join(", ")));
    }
    if !differs.is_empty() {
        parts.push(format!("differs from {}", differs.join(", ")));
    }
    if parts.is_empty() {
        parts.push(match absent {
            0 => "not in your store".to_string(),
            1 => "not in your store or 1 cache".to_string(),
            n => format!("not in your store or {n} caches"),
        });
    }

    // Caches that could not say either way.
    for c in comparisons {
        match &c.verdict {
            Verdict::NeedsCredentials => {
                parts.push(format!("{} needs credentials", source_name(&c.source)))
            }
            Verdict::Unreachable(why) => {
                parts.push(format!("{} unreachable ({why})", source_name(&c.source)))
            }
            _ => {}
        }
    }

    let digest = hash
        .strip_prefix(rewind_init::NAR_HASH_PREFIX)
        .unwrap_or(hash);
    let short = digest.get(..HASH_SHOWN).unwrap_or(digest);
    format!("{path} {short}  {}", parts.join("; "))
}

/// How many characters of each output's hash a `rewind check` line shows.
const CHECK_HASH_SHOWN: usize = 12;

/// How many characters of an output's hash a verdict line shows.
const HASH_SHOWN: usize = 16;

/// A source as a reader knows it: "your store", or a cache's URL without
/// its scheme.
fn source_name(source: &Source) -> String {
    match source {
        Source::Store => "your store".to_string(),
        Source::Cache(url) => url
            .split_once("://")
            .map_or(url.as_str(), |(_, rest)| rest)
            .to_string(),
    }
}

/// Printed once under the verdict lines when some build of an output
/// differs from the run's.
pub const DIFFERS_NOTE: &str = "rewind: a store path names a build's inputs, not its contents, so \
     another build of it can differ in bytes: the package may not be bit-reproducible, or the \
     VM's CPU model, kernel or --cores reached its output";

/// One line per run for `rewind check`.
pub fn outcome_line(run: &Run) -> anyhow::Result<String> {
    let OutcomeKey {
        status: status_,
        outputs,
    } = outcome_key(run)?;
    let missing = missing_outputs(&run.manifest.spec.job.outputs, &outputs);
    let ended = match &run.manifest.outcome {
        Some(o) => Ending::of(&o.stop, status_, &missing),
        None => Ending::NoStatus,
    };
    let steps = run.manifest.outcome.as_ref().map_or(0, |o| o.step);
    let hashes: Vec<String> = outputs
        .iter()
        .map(|(_, h)| {
            let digest = h.strip_prefix(rewind_init::NAR_HASH_PREFIX).unwrap_or(h);
            digest.get(..CHECK_HASH_SHOWN).unwrap_or(digest).to_string()
        })
        .collect();
    Ok(format!(
        "{:<14} {:>10} steps  {}  run {}",
        ended,
        steps,
        hashes.join(" "),
        run.manifest.id
    ))
}

/// A run's schedule for `rewind check`'s report: schedule 0, or a seed
/// and the steps it perturbs.
pub fn schedule_words(spec: &rewind_core::Spec) -> String {
    match (spec.schedule, spec.window()) {
        (0, _) => "schedule 0".into(),
        (seed, Some((from, until))) => format!("schedule {seed} over steps {from}..{until}"),
        (seed, None) => format!("schedule {seed} from step {}", spec.schedule_from),
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
            format!("{who}({comm}) {}", ExitStatus::from_raw(*s))
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

/// An argument as a shell would need it typed: as it is when every
/// character is one no shell treats specially, else in single quotes.
pub fn quote(arg: &str) -> String {
    let plain = |c: char| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c);
    if !arg.is_empty() && arg.chars().all(plain) {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', r"'\''"))
}

/// Whether `rewind ps` lists the kernel's own threads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KernelThreads {
    Show,
    Hide,
}

/// kthreadd, the parent of every kernel thread.
const KTHREADD: u32 = 2;

/// The processes alive at a step, the kernel's own threads only when
/// `kernel` says so.
pub fn alive(trace: &Trace, step: u64, kernel: KernelThreads) -> Vec<rewind_trace::Process> {
    trace
        .processes()
        .into_iter()
        .filter(|p| p.alive_at(step))
        .filter(|p| kernel == KernelThreads::Show || (p.pid != KTHREADD && p.parent != KTHREADD))
        .collect()
}

/// The process tree alive at a step, indented by parent.
pub fn process_tree(trace: &Trace, step: u64, kernel: KernelThreads) -> String {
    let procs = alive(trace, step, kernel);
    let alive: Vec<_> = procs.iter().collect();
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
    // A child that forked and never exec'd runs its parent's program.
    let name = if p.argv.is_empty() {
        "[kernel thread]".to_string()
    } else if p.execd {
        p.argv.join(" ")
    } else {
        format!("{} (fork)", p.argv.join(" "))
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

#[cfg(test)]
mod tests {
    // How a run that exited 0 reads when its job left an output uncreated:
    // nix-daemon fails such a build, so Rewind calls it missing-output
    // rather than exited:0.
    use super::*;

    use crate::reproduce::tests::{job, spec};

    #[test]
    fn a_schedule_reads_as_its_seed_and_window() {
        // Schedule 0 is just that; a seed names the steps it perturbs, as
        // a window or from a step on.
        let mut s = spec(job(&["sh"], &[], "/"));
        assert_eq!(schedule_words(&s), "schedule 0");
        s.schedule = 6;
        s.schedule_from = 3629;
        assert_eq!(schedule_words(&s), "schedule 6 from step 3629");
        s.schedule_until = 5140;
        assert_eq!(schedule_words(&s), "schedule 6 over steps 3629..5140");
    }

    const OUT: &str = "/nix/store/0000000000000000000000000000000b-x";
    const DEV: &str = "/nix/store/0000000000000000000000000000000c-x-dev";

    #[test]
    fn an_output_init_never_hashed_is_missing() {
        let expected = vec![OUT.to_string(), DEV.to_string()];
        let hashed = vec![(OUT.to_string(), "0123abcd".to_string())];
        assert_eq!(missing_outputs(&expected, &hashed), vec![DEV.to_string()]);
        assert!(missing_outputs(&expected[..1], &hashed).is_empty());
    }

    #[test]
    fn an_output_line_names_who_matches_and_who_differs() {
        // Sources are named the way a reader knows them, the store as
        // "your store" and a cache by its URL without the scheme. Caches
        // without the path are only counted, and only when no source had
        // it; ones that could not answer say so, with the reason.
        use rewind_core::compare::{Comparison, Source, Verdict};
        let path = "/nix/store/0rk0cxq5669yg1bwzwk3s6nkhk9fqk6w-pkgconf-2.4.3";
        let hash = "sha256:dfd7aed38b3557dcbca63b4a1a7572f4af98c98e7649549a6d53cbfbcee928f4";
        let cache = |url: &str, verdict| Comparison {
            source: Source::Cache(url.into()),
            verdict,
        };
        let store = |verdict| Comparison {
            source: Source::Store,
            verdict,
        };
        let prefix = format!("{path} dfd7aed38b3557dc  ");

        let line = verdict_line(
            path,
            hash,
            &[
                store(Verdict::Matches),
                cache("https://cache.nixos.org", Verdict::Matches),
                cache("http://leviathan:5000", Verdict::Differs),
                cache("https://other.cachix.org", Verdict::Absent),
            ],
        );
        assert_eq!(
            line,
            format!("{prefix}matches your store, cache.nixos.org; differs from leviathan:5000")
        );

        let line = verdict_line(
            path,
            hash,
            &[
                cache("https://cache.nixos.org", Verdict::Absent),
                cache("https://private.cachix.org", Verdict::NeedsCredentials),
                cache(
                    "http://leviathan:5000",
                    Verdict::Unreachable("timeout".into()),
                ),
            ],
        );
        assert_eq!(
            line,
            format!(
                "{prefix}not in your store or 1 cache; \
                 private.cachix.org needs credentials; leviathan:5000 unreachable (timeout)"
            )
        );
        assert_eq!(
            verdict_line(path, hash, &[]),
            format!("{prefix}not in your store")
        );
    }

    #[test]
    fn an_argument_is_quoted_unless_a_shell_takes_it_as_it_is() {
        // Plain words, paths and KEY=VALUE pass; spaces, quotes, a
        // redirection, a comment and the empty argument are quoted, a
        // quote inside closed and reopened around an escaped one.
        assert_eq!(quote("-q"), "-q");
        assert_eq!(quote("CC=gcc"), "CC=gcc");
        assert_eq!(quote("/nix/store/0a-x-1.0.drv"), "/nix/store/0a-x-1.0.drv");
        assert_eq!(quote("target remote x"), "'target remote x'");
        assert_eq!(quote("it's"), r"'it'\''s'");
        assert_eq!(quote("a>b"), "'a>b'");
        assert_eq!(quote("nixpkgs#hello"), "'nixpkgs#hello'");
        assert_eq!(quote(""), "''");
    }
}
