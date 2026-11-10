//! Text renderings of runs, events and processes for the terminal.

use std::fmt::Write;
use std::time::Duration;

use rewind_core::Run;
use rewind_core::compare::{Comparison, Source, Verdict};
use rewind_trace::ending;
use rewind_trace::manifest::Manifest;
use rewind_trace::{Divergence, Event, Trace};

use crate::color::{Paint, Same, Style};

/// How wide `rewind ls` keeps the column that says how a run ended.
const ENDING_WIDTH: usize = 9;

/// How a run's ending reads from its manifest, painted green when the
/// manifest's verdict says the run passed and red when it did not.
fn ending(paint: Paint, manifest: &Manifest, width: usize) -> String {
    let Some(outcome) = &manifest.outcome else {
        return paint.padded(Style::Differs, ending::Ending::NoStatus, width);
    };
    let style = if manifest.verdict.passed() {
        Style::Same
    } else {
        Style::Differs
    };
    paint.padded(style, ending::label(outcome, &manifest.verdict), width)
}

/// A one-line summary for `rewind ls`, painted for standard output.
pub(crate) fn summary(run: &Run, paint: Paint) -> String {
    let m = &run.manifest;
    let outcome = match &m.outcome {
        Some(o) => format!("{} {:>12} steps", ending(paint, m, ENDING_WIDTH), o.step),
        None if run.executing() => paint.paint(Style::Warn, "running"),
        None => paint.paint(Style::Differs, "interrupted"),
    };
    format!("{}  {outcome}  {}", m.id, m.name)
}

/// The line printed when a run finishes, with how it stopped in the
/// words `stop` gives, else the stop's own, painted for standard error.
pub(crate) fn finished(run: &Run, stop: Option<&str>, paint: Paint) -> String {
    let m = &run.manifest;
    let Some(o) = &m.outcome else {
        return format!("{} run {} did not finish", paint.prefix(), m.id);
    };
    format!(
        "{} run {} {} after {} steps, {:.3}s virtual, {:.3}s wall ({})",
        paint.prefix(),
        m.id,
        ending(paint, m, 0),
        o.step,
        Duration::from_nanos(o.virtual_ns).as_secs_f64(),
        Duration::from_millis(o.wall_ms).as_secs_f64(),
        stop.map_or_else(|| o.stop.to_string(), str::to_string),
    )
}

/// How a run ended, as something two runs can be compared by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OutcomeKey {
    /// The job's wait status.
    pub status: Option<i32>,
    /// Each output init hashed, by path, with its hash.
    pub outputs: Vec<(String, String)>,
}

/// How `run` ended: its exit status and the hash of every output.
pub(crate) fn outcome_key(run: &Run) -> anyhow::Result<OutcomeKey> {
    Ok(OutcomeKey {
        status: run.manifest.outcome.as_ref().and_then(|o| o.status),
        outputs: run.trace()?.outputs(),
    })
}

/// The step the job started on, from init's start mark; 0 if there is
/// none.
pub(crate) fn start_step(trace: &Trace) -> u64 {
    trace.job_start().unwrap_or(0)
}

/// A file size the way people read one: bytes, KB or MB, in decimal
/// units.
pub(crate) fn size(bytes: u64) -> String {
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
/// with, each with the word its lines start with, `this` amber and
/// `other` blue.
pub(crate) fn comparison(
    (this, this_name): (&Trace, &str),
    (other, other_name): (&Trace, &str),
    c: &rewind_trace::compare::Comparison,
    paint: Paint,
) -> String {
    const BEFORE: usize = 3;
    const AFTER: usize = 4;

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
    let sides = [
        (this_name, Style::Left, this, &mine),
        (other_name, Style::Right, other, &theirs),
    ];
    for (name, style, trace, indices) in sides {
        let label = paint.padded(style, name, width);
        for &e in indices.iter().skip(i).take(AFTER) {
            let _ = writeln!(out, "  {label}  {}", event(&trace.events[e]));
        }
    }
    out
}

/// One line for an output of a Nix run: its path, the start of its NAR
/// hash, and what this machine's store and the caches said about it,
/// painted for standard output.
pub(crate) fn verdict_line(
    path: &str,
    hash: &str,
    comparisons: &[Comparison],
    paint: Paint,
) -> String {
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
        parts.push(paint.verdict(Same::Yes, format!("matches {}", matches.join(", "))));
    }
    if !differs.is_empty() {
        parts.push(paint.verdict(Same::No, format!("differs from {}", differs.join(", "))));
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
            Verdict::NeedsCredentials => parts.push(paint.paint(
                Style::Warn,
                format!("{} needs credentials", source_name(&c.source)),
            )),
            Verdict::Unreachable(why) => parts.push(paint.paint(
                Style::Warn,
                format!("{} unreachable ({why})", source_name(&c.source)),
            )),
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
/// differs from the run's, after rewind's prefix.
pub(crate) const DIFFERS_NOTE: &str = "a store path names a build's inputs, not its contents, so \
     another build of it can differ in bytes: the package may not be bit-reproducible, or the \
     VM's CPU model, kernel or --cores reached its output";

/// How wide `rewind check` keeps the column that says how a run ended.
const CHECK_ENDING_WIDTH: usize = 14;

/// One line per run for `rewind check`, painted for standard output: its
/// ending green when the manifest's verdict says the run passed and red
/// when not, and each output's hash red where it differs from the same
/// output's in `base`, the unperturbed run's.
pub(crate) fn outcome_line(run: &Run, base: &OutcomeKey, paint: Paint) -> anyhow::Result<String> {
    let OutcomeKey { outputs, .. } = outcome_key(run)?;
    let steps = run.manifest.outcome.as_ref().map_or(0, |o| o.step);
    let hashes: Vec<String> = outputs
        .iter()
        .map(|(path, h)| {
            let digest = h.strip_prefix(rewind_init::NAR_HASH_PREFIX).unwrap_or(h);
            let short = digest.get(..CHECK_HASH_SHOWN).unwrap_or(digest);
            let same = base.outputs.iter().any(|(p, b)| p == path && b == h);
            if same {
                short.to_string()
            } else {
                paint.verdict(Same::No, short)
            }
        })
        .collect();
    Ok(format!(
        "{} {:>10} steps  {}  run {}",
        ending(paint, &run.manifest, CHECK_ENDING_WIDTH),
        steps,
        hashes.join(" "),
        run.manifest.id
    ))
}

/// A run's schedule for `rewind check`'s report: schedule 0, or a seed
/// and the steps it perturbs, and its longest stall when not the default.
pub(crate) fn schedule_words(spec: &rewind_core::Spec) -> String {
    let words = match (spec.schedule, spec.window()) {
        (0, _) => return "schedule 0".into(),
        (seed, Some((from, until))) => format!("schedule {seed} over steps {from}..{until}"),
        (seed, None) => format!("schedule {seed} from step {}", spec.schedule_from),
    };
    if spec.stall_max.is_default() {
        return words;
    }
    let longest = rewind_trace::schedule::stall_words(spec.stall_max.ns());
    format!("{words}, stalls up to {longest}")
}

/// The seed a run's randomness came from: its own from boot, or for a
/// fork reseeded since, the last reseed's.
pub(crate) fn seed_of(spec: &rewind_core::Spec) -> u64 {
    spec.reseeds.last().map_or(spec.seed, |r| r.seed)
}

/// The seed a run's randomness came from, in words: its own from boot, or
/// for a fork reseeded since, the last reseed and the step it was made at.
pub(crate) fn seed_words(spec: &rewind_core::Spec) -> String {
    match spec.reseeds.last() {
        Some(r) => format!("seed {} from step {}", r.seed, r.step),
        None => format!("seed {}", spec.seed),
    }
}

/// One event, in the syscall-ish notation the scrubber uses.
pub(crate) fn event(e: &Event) -> String {
    let what = e.kind.words();
    format!("{:>10} {:>5}/{:<5} {what}", e.step, e.pid, e.tid)
}

/// An argument as a shell would need it typed: as it is when every
/// character is one no shell treats specially, else in single quotes.
pub(crate) fn quote(arg: &str) -> String {
    let plain = |c: char| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c);
    if !arg.is_empty() && arg.chars().all(plain) {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', r"'\''"))
}

/// Whether `rewind ps` lists the kernel's own threads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KernelThreads {
    Show,
    Hide,
}

/// kthreadd, the parent of every kernel thread.
const KTHREADD: u32 = 2;

/// The processes alive at a step, as they were then, the kernel's own
/// threads only when `kernel` says so.
pub(crate) fn alive(trace: &Trace, step: u64, kernel: KernelThreads) -> Vec<rewind_trace::Process> {
    trace
        .processes()
        .into_iter()
        .filter(|p| p.alive_at(step))
        .filter(|p| kernel == KernelThreads::Show || (p.pid != KTHREADD && p.parent != KTHREADD))
        .map(|p| p.as_of(step))
        .collect()
}

/// The process tree alive at a step, indented by parent.
pub(crate) fn process_tree(trace: &Trace, step: u64, kernel: KernelThreads) -> String {
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

/// Where a replay's read and write records first differ from the
/// recording's, when their events are the same: the recording is the left
/// side and the replay the right.
pub(crate) fn io_divergence(d: &Divergence) -> String {
    format!(
        "the read and write records first differ at step {} (record {}, step {} replayed)",
        d.left_step, d.index, d.right_step
    )
}

/// The words for a divergence in the context switches, with the events
/// the same: the switch the two first differ at, and the step of each.
pub(crate) fn switch_divergence(d: &Divergence) -> String {
    format!(
        "the context switches first differ at step {} (switch {}, step {} replayed)",
        d.left_step, d.index, d.right_step
    )
}

/// The words a divergence's lines start with, and the column they fill.
const BOTH: &str = "both";
const LEFT: &str = "left";
const RIGHT: &str = "right";
const SIDE_WIDTH: usize = 5;

/// Where two traces differ when one of them has no event at the index
/// where they first differ: the last event of the trace that ended and the
/// next event of the other, each trace named by `names`. None when both
/// have an event there, as two traces that part at an event both took do.
/// A divergence's step for a trace that ran out is its last event's step.
pub(crate) fn ran_out(
    d: &Divergence,
    left: &Trace,
    right: &Trace,
    names: [&str; 2],
) -> Option<String> {
    let [left_name, right_name] = names;
    let (ended, ended_trace, went_on, next_step) = if d.index >= left.events.len() {
        (left_name, left, right_name, d.right_step)
    } else if d.index >= right.events.len() {
        (right_name, right, left_name, d.left_step)
    } else {
        return None;
    };

    // The trace that ended, by its last event, or by having none.
    let last = match d.index.checked_sub(1) {
        Some(i) => format!("ends at event {i}, step {}", ended_trace.last_step()),
        None => "has no events".to_string(),
    };
    Some(format!(
        "the {ended} {last}, and the {went_on} goes on to event {} at step {next_step}",
        d.index
    ))
}

/// Where two runs first differ, with the events on each side, the left
/// run's amber and the right run's blue.
pub(crate) fn divergence(left: &Trace, right: &Trace, paint: Paint) -> String {
    let Some(d) = left.divergence(right) else {
        return format!("{}\n", paint.verdict(Same::Yes, "identical"));
    };
    let first = paint.verdict(Same::No, "first difference");
    let mut out = match ran_out(&d, left, right, [LEFT, RIGHT]) {
        Some(words) => format!("{first}: {words}\n"),
        None => format!(
            "{first} at event {}: step {} on the left, step {} on the right\n",
            d.index, d.left_step, d.right_step
        ),
    };
    let context = d.index.saturating_sub(3)..d.index;
    for e in &left.events[context] {
        let _ = writeln!(out, "  {BOTH:<SIDE_WIDTH$} {}", event(e));
    }
    const AFTER: usize = 4;
    let sides = [(LEFT, Style::Left, left), (RIGHT, Style::Right, right)];
    for (name, style, trace) in sides {
        let label = paint.padded(style, name, SIDE_WIDTH);
        for e in trace.events.iter().skip(d.index).take(AFTER) {
            let _ = writeln!(out, "  {label} {}", event(e));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    // The words runs, schedules and outputs are printed in.
    use super::*;
    use rewind_trace::EventKind;

    use crate::reproduce::tests::{job, spec};

    #[test]
    fn records_that_differ_say_where() {
        // A replay whose events matched and whose read and write records
        // part at record 4: the line names the recorded step first, then
        // the record and the replayed step.
        let d = Divergence {
            index: 4,
            left_step: 120,
            right_step: 121,
            part: rewind_trace::Part::IoRecords,
        };
        assert_eq!(
            io_divergence(&d),
            "the read and write records first differ at step 120 (record 4, step 121 replayed)"
        );
    }

    #[test]
    fn a_trace_that_ran_out_of_events_says_so() {
        // Two traces that agree on two events, one of which goes on to a
        // third: the words name the last event of the one that ended and
        // the next event of the other, whichever side ran out. Traces
        // that differ at an event both have leave the usual words.
        use rewind_trace::{Event, EventKind};
        let mark = |step| Event {
            step,
            pid: 1,
            tid: 1,
            kind: EventKind::Mark { text: "x".into() },
        };
        let short = Trace {
            events: vec![mark(10), mark(20)],
            ..Default::default()
        };
        let long = Trace {
            events: vec![mark(10), mark(20), mark(90)],
            ..Default::default()
        };
        let names = ["recording", "replay"];
        let d = short.divergence(&long).unwrap();
        assert_eq!(
            ran_out(&d, &short, &long, names).as_deref(),
            Some(
                "the recording ends at event 1, step 20, and the replay goes on to event 2 at step 90"
            )
        );
        let d = long.divergence(&short).unwrap();
        assert_eq!(
            ran_out(&d, &long, &short, names).as_deref(),
            Some(
                "the replay ends at event 1, step 20, and the recording goes on to event 2 at step 90"
            )
        );

        // A trace with no events at all ends before its first.
        let empty = Trace {
            events: vec![],
            ..Default::default()
        };
        let d = empty.divergence(&short).unwrap();
        assert_eq!(
            ran_out(&d, &empty, &short, names).as_deref(),
            Some("the recording has no events, and the replay goes on to event 0 at step 10")
        );

        // Both have an event where they differ.
        let other = Trace {
            events: vec![mark(10), mark(21)],
            ..Default::default()
        };
        let d = short.divergence(&other).unwrap();
        assert_eq!(ran_out(&d, &short, &other, names), None);
    }

    #[test]
    fn a_schedule_reads_as_its_seed_and_window() {
        // Schedule 0 is just that; a seed names the steps it perturbs, as
        // a window or from a step on, and a longest stall if not the
        // default.
        let mut s = spec(job(&["sh"], &[], "/"));
        assert_eq!(schedule_words(&s), "schedule 0");
        s.schedule = 6;
        s.schedule_from = 3629;
        assert_eq!(schedule_words(&s), "schedule 6 from step 3629");
        s.schedule_until = 5140;
        assert_eq!(schedule_words(&s), "schedule 6 over steps 3629..5140");

        // A longest stall away from the default is named after the window.
        s.stall_max = "50ms".parse().unwrap();
        assert_eq!(
            schedule_words(&s),
            "schedule 6 over steps 3629..5140, stalls up to 50 ms"
        );
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
            Paint::Plain,
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
            Paint::Plain,
        );
        assert_eq!(
            line,
            format!(
                "{prefix}not in your store or 1 cache; \
                 private.cachix.org needs credentials; leviathan:5000 unreachable (timeout)"
            )
        );
        assert_eq!(
            verdict_line(path, hash, &[], Paint::Plain),
            format!("{prefix}not in your store")
        );
    }

    #[test]
    fn a_colored_verdict_line_paints_each_verdict() {
        // With color, what matched reads green, what differs red and a
        // cache that could not say amber; the path and hash stay plain.
        let path = "/nix/store/x-hello";
        let hash = "sha256:dfd7aed38b3557dcbca63b4a1a7572f4af98c98e7649549a6d53cbfbcee928f4";
        let line = verdict_line(
            path,
            hash,
            &[
                Comparison {
                    source: Source::Store,
                    verdict: Verdict::Matches,
                },
                Comparison {
                    source: Source::Cache("http://leviathan:5000".into()),
                    verdict: Verdict::Differs,
                },
                Comparison {
                    source: Source::Cache("https://private.cachix.org".into()),
                    verdict: Verdict::NeedsCredentials,
                },
            ],
            Paint::Ansi,
        );
        assert_eq!(
            line,
            format!(
                "{path} dfd7aed38b3557dc  \x1b[32mmatches your store\x1b[0m; \
                 \x1b[31mdiffers from leviathan:5000\x1b[0m; \
                 \x1b[33mprivate.cachix.org needs credentials\x1b[0m"
            )
        );
    }

    /// A write of `text` by `pid` at `step`.
    fn write(step: u64, pid: u32, text: &str) -> Event {
        Event {
            step,
            pid,
            tid: pid,
            kind: EventKind::Output {
                fd: 1,
                bytes: text.as_bytes().to_vec(),
            },
        }
    }

    #[test]
    fn a_colored_divergence_paints_each_side() {
        // The events both runs share stay plain; the left run's label is
        // amber and the right run's blue, and the columns after them line
        // up as without color.
        let left = Trace {
            events: vec![write(3, 7, "a"), write(5, 7, "b")],
            ..Default::default()
        };
        let right = Trace {
            events: vec![write(3, 7, "a"), write(6, 7, "c")],
            ..Default::default()
        };
        let plain = divergence(&left, &right, Paint::Plain);
        let painted = divergence(&left, &right, Paint::Ansi);
        assert!(painted.contains("\n  both  "), "{painted}");
        assert!(painted.contains("\n  \x1b[33mleft\x1b[0m  "), "{painted}");
        assert!(painted.contains("\n  \x1b[34mright\x1b[0m "), "{painted}");
        let stripped = painted
            .replace("\x1b[31m", "")
            .replace("\x1b[33m", "")
            .replace("\x1b[34m", "")
            .replace("\x1b[0m", "");
        assert_eq!(stripped, plain);
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
