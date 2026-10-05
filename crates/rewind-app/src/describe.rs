//! Events and numbers as the scrubber prints them.
//!
//! The "At this step" card shows the last event before the playhead as the
//! system call or signal that produced it, the way strace would, so that
//! `write(1, "...")`, `execve(...)` and `SIGSEGV at 0x...` read the same as
//! in any other Linux tool.

use rewind_trace::{Event, EventKind, signal_name};

use crate::model::signo;
use crate::selection::{Mapped, Splice};
use rewind_trace::ending::ExitStatus;

/// Text inside a described event is cut to this many characters.
const MAX_QUOTED_CHARS: usize = 160;

/// The most argv entries shown for an execve.
const MAX_ARGV: usize = 8;

/// The character that marks cut text.
const ELLIPSIS: char = '\u{2026}';

/// Open flags, as the kernel numbers them on x86_64.
mod open_flag {
    pub const ACCESS_MASK: u32 = 0o3;
    pub const O_WRONLY: u32 = 0o1;
    pub const O_RDWR: u32 = 0o2;
    pub const O_CREAT: u32 = 0o100;
    pub const O_EXCL: u32 = 0o200;
    pub const O_TRUNC: u32 = 0o1000;
    pub const O_APPEND: u32 = 0o2000;
}

/// How a described event is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventTone {
    Normal,
    Error,
}

/// An event as one line of text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Described {
    pub text: String,
    pub tone: EventTone,
}

/// Describes an event as a system call or signal.
pub fn describe(event: &Event) -> Described {
    let normal = |text: String| Described {
        text,
        tone: EventTone::Normal,
    };
    let error = |text: String| Described {
        text,
        tone: EventTone::Error,
    };

    match &event.kind {
        EventKind::Console { text } => normal(format!("printk({})", quote(text))),
        EventKind::Output { fd, bytes } => {
            let text = String::from_utf8_lossy(bytes);
            let line = format!("write({fd}, {})", quote(&text));
            if crate::model::is_error_text(&text) {
                return error(line);
            }
            normal(line)
        }
        EventKind::Exec { filename, argv, .. } => {
            let mut shown: Vec<String> = argv.iter().take(MAX_ARGV).map(|a| quote(a)).collect();
            if argv.len() > MAX_ARGV {
                shown.push(ELLIPSIS.to_string());
            }
            normal(format!(
                "execve({}, [{}])",
                quote(filename),
                shown.join(", ")
            ))
        }
        EventKind::Fork { child, thread } => {
            if *thread {
                return normal(format!("clone(CLONE_THREAD) \u{2192} tid {child}"));
            }
            normal(format!("clone() \u{2192} pid {child}"))
        }
        EventKind::Exit {
            status,
            comm,
            thread,
        } => {
            let call = if *thread { "exit" } else { "exit_group" };
            match ExitStatus::from_raw(*status) {
                ExitStatus::Code(0) => normal(format!("{call}(0) {comm}")),
                ExitStatus::Code(code) => error(format!("{call}({code}) {comm}")),
                ExitStatus::Signal { signo, core } => {
                    let dumped = if core { " (core dumped)" } else { "" };
                    error(format!("{comm} killed by {}{dumped}", signal_name(signo)))
                }
            }
        }
        EventKind::Signal { signo, code, addr } => {
            let name = signal_name(*signo);
            if signo::FATAL.contains(signo) {
                return error(format!("{name} at {addr:#x} (si_code {code})"));
            }
            normal(format!("{name} delivered (si_code {code})"))
        }
        EventKind::Open { path, flags } => {
            let text = format!("openat({}, {})", quote(path), open_flags(*flags));
            if crate::model::is_core_path(path) {
                return error(text);
            }
            normal(text)
        }
        EventKind::Unlink { path } => normal(format!("unlink({})", quote(path))),
        EventKind::Rename { from, to } => normal(format!("rename({}, {})", quote(from), quote(to))),
        EventKind::Mark { text } => normal(format!("write(\"/dev/rewind\", {})", quote(text))),
        EventKind::Unknown { kind, data } => {
            normal(format!("record kind {kind}, {} bytes", data.len()))
        }
    }
}

/// An event with where and in which thread it happened, for comparing
/// two runs' events that may read the same but land on different steps.
pub fn summary(event: &Event) -> String {
    let thread = if event.tid == event.pid {
        format!("pid {}", event.pid)
    } else {
        format!("pid {} tid {}", event.pid, event.tid)
    };
    format!(
        "step {} \u{b7} {thread}: {}",
        thousands(event.step),
        describe(event).text
    )
}

/// The open flags a write shows: access mode, then create, exclusive,
/// truncate and append.
fn open_flags(flags: u32) -> String {
    let access = match flags & open_flag::ACCESS_MASK {
        open_flag::O_WRONLY => "O_WRONLY",
        open_flag::O_RDWR => "O_RDWR",
        _ => "O_RDONLY",
    };
    let mut parts = vec![access];
    let extra = [
        (open_flag::O_CREAT, "O_CREAT"),
        (open_flag::O_EXCL, "O_EXCL"),
        (open_flag::O_TRUNC, "O_TRUNC"),
        (open_flag::O_APPEND, "O_APPEND"),
    ];
    for (bit, name) in extra {
        if flags & bit != 0 {
            parts.push(name);
        }
    }
    parts.join("|")
}

/// Text in double quotes with control characters escaped, cut to
/// `MAX_QUOTED_CHARS`.
pub fn quote(text: &str) -> String {
    let mut out = String::from("\"");
    for (count, c) in text.chars().enumerate() {
        if count == MAX_QUOTED_CHARS {
            out.push(ELLIPSIS);
            break;
        }
        match c {
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '"' => out.push_str("\\\""),
            c if c.is_control() => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// How long ago something happened, in the largest whole unit: "just
/// now", "5m ago", "3d ago".
pub fn ago(elapsed: std::time::Duration) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    let secs = elapsed.as_secs();
    let (n, unit) = match secs {
        s if s < MINUTE => return "just now".to_string(),
        s if s < HOUR => (s / MINUTE, "m"),
        s if s < DAY => (s / HOUR, "h"),
        s => (s / DAY, "d"),
    };
    format!("{n}{unit} ago")
}

/// A count with thousands separators: 11760 as "11,760".
pub fn thousands(n: u64) -> String {
    const GROUP: usize = 3;
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / GROUP);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(GROUP) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Text cut to `max` characters with an ellipsis, for labels that must
/// fit a fixed space.
pub fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push(ELLIPSIS);
    out
}

/// Text with every Nix store path's hash cut to a few characters, as the
/// design shows them: /nix/store/9x2k...-mylib-0.3.0.drv. Everything else,
/// including text that only looks like a store path, is unchanged.
pub fn short_store_paths(text: &str) -> String {
    short_store_paths_mapped(text).shown
}

/// `short_store_paths`, keeping track of what each shortened hash stands
/// for, so a selection of the shown text copies the full paths.
pub fn short_store_paths_mapped(text: &str) -> Mapped {
    const STORE: &str = "/nix/store/";
    const HASH_LEN: usize = 32;
    const HASH_SHOWN: usize = 4;
    const SEPARATOR: u8 = b'-';

    let mut out = String::with_capacity(text.len());
    let mut splices = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(STORE) {
        let (before, after) = rest.split_at(at + STORE.len());
        out.push_str(before);

        // A hash is 32 ASCII letters and digits followed by a dash.
        let hash = after.get(..HASH_LEN);
        let is_hash = hash.is_some_and(|h| h.bytes().all(|b| b.is_ascii_alphanumeric()))
            && after.as_bytes().get(HASH_LEN) == Some(&SEPARATOR);
        if !is_hash {
            rest = after;
            continue;
        }

        // The hash's first characters stay; the rest becomes an ellipsis
        // that stands for them.
        let original_start = text.len() - after.len() + HASH_SHOWN;
        out.push_str(&after[..HASH_SHOWN]);
        let shown_start = out.len();
        out.push(ELLIPSIS);
        splices.push(Splice {
            shown: shown_start..out.len(),
            original: original_start..original_start + HASH_LEN - HASH_SHOWN,
        });
        rest = &after[HASH_LEN..];
    }
    out.push_str(rest);
    Mapped::new(out, text.to_string(), splices)
}

/// Quoted text in a plain sentence is cut to this many characters.
const MAX_PLAIN_QUOTE: usize = 48;

/// How many characters before where two quoted texts differ their quotes
/// show, when the difference is past what a quote shows from the start.
const QUOTE_LEAD: usize = 16;

/// One run's side of a divergence, for describing it in words.
#[derive(Clone, Copy, Debug)]
pub struct Party<'a> {
    pub event: &'a Event,
    /// The event's thread number within the compared program, 0 for its
    /// first thread; None when the comparison is not about one program.
    pub thread: Option<usize>,
}

/// A divergence in words: what each run did next, and, when both did the
/// same kind of thing, what exactly differs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Difference {
    pub here: String,
    pub there: String,
    pub detail: Option<String>,
}

/// Describes where two runs part. `program` names the compared program,
/// and `name_of` names a process by pid for comparisons of whole runs.
pub fn difference(
    here: Option<Party>,
    there: Option<Party>,
    program: Option<&str>,
    name_of: &dyn Fn(u32) -> Option<String>,
) -> Difference {
    // Two texts alike for longer than a quote shows are both quoted from
    // shortly before where they differ, so the difference is on screen.
    let texts = (
        here.and_then(|p| quotable(p.event)),
        there.and_then(|p| quotable(p.event)),
    );
    let from = match texts {
        (Some(a), Some(b)) => quote_start(&a, &b),
        _ => 0,
    };
    let said = |party: Option<Party>| match party {
        Some(p) => plain_from(p, program, name_of, from),
        None => match program {
            Some(program) => format!("{program} does nothing more"),
            None => "the run ends".to_string(),
        },
    };
    Difference {
        here: said(here),
        there: said(there),
        detail: match (here, there) {
            (Some(h), Some(t)) => detail(h, t),
            _ => None,
        },
    }
}

/// Who did something: a thread of the compared program, or a process.
fn subject(party: Party, program: Option<&str>, name_of: &dyn Fn(u32) -> Option<String>) -> String {
    const MAIN_THREAD: usize = 0;
    let event = party.event;
    match (program, party.thread) {
        (Some(program), Some(MAIN_THREAD)) => program.to_string(),
        (Some(program), Some(n)) => format!("thread {} of {program}", n + 1),
        _ => {
            let name = name_of(event.pid).unwrap_or_else(|| format!("pid {}", event.pid));
            if event.tid == event.pid {
                name
            } else {
                format!("thread {} of {name}", event.tid)
            }
        }
    }
}

/// What an event did, as a short sentence without its full stop:
/// "thread 2 of pool gets SIGSEGV at 0x108".
pub fn plain(
    party: Party,
    program: Option<&str>,
    name_of: &dyn Fn(u32) -> Option<String>,
) -> String {
    plain_from(party, program, name_of, 0)
}

/// The text an event's sentence quotes: what it wrote, the command line it
/// ran, or the mark or console line.
fn quotable(event: &Event) -> Option<String> {
    let text = match &event.kind {
        EventKind::Output { bytes, .. } => String::from_utf8_lossy(bytes).into_owned(),
        EventKind::Exec { argv, .. } => argv.join(" "),
        EventKind::Mark { text } | EventKind::Console { text } => text.clone(),
        _ => return None,
    };
    Some(text.trim_end().to_string())
}

/// The character two quoted texts are quoted from: 0 when they differ
/// within what a quote shows, else shortly before where they differ.
fn quote_start(a: &str, b: &str) -> usize {
    let alike = a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count();
    if alike + QUOTE_LEAD < MAX_PLAIN_QUOTE {
        return 0;
    }
    alike - QUOTE_LEAD
}

/// `text` cut to `max` characters from character `from`, marked as cut at
/// either end.
fn clip_from(text: &str, from: usize, max: usize) -> String {
    if from == 0 {
        return clip(text, max);
    }
    let rest: String = text.chars().skip(from).collect();
    format!("{ELLIPSIS}{}", clip(&rest, max.saturating_sub(1)))
}

/// What an event did, as `plain` says it, with its quote starting at
/// character `from`.
fn plain_from(
    party: Party,
    program: Option<&str>,
    name_of: &dyn Fn(u32) -> Option<String>,
    from: usize,
) -> String {
    let who = subject(party, program, name_of);
    let quoted = |text: &str| format!("\"{}\"", clip_from(text.trim_end(), from, MAX_PLAIN_QUOTE));
    match &party.event.kind {
        EventKind::Output { fd, bytes } => {
            let text = String::from_utf8_lossy(bytes);
            let stream = match fd {
                1 => "stdout".to_string(),
                2 => "stderr".to_string(),
                n => format!("file descriptor {n}"),
            };
            format!("{who} writes {} to {stream}", quoted(&text))
        }
        EventKind::Signal { signo, addr, .. } => {
            let name = signal_name(*signo);
            if signo::FATAL.contains(signo) {
                return format!("{who} gets {name} at {addr:#x}");
            }
            format!("{who} gets {name}")
        }
        EventKind::Exit { status, .. } => match ExitStatus::from_raw(*status) {
            ExitStatus::Code(0) => format!("{who} exits"),
            ExitStatus::Code(code) => format!("{who} exits with status {code}"),
            ExitStatus::Signal { signo, .. } => {
                format!("{who} is killed by {}", signal_name(signo))
            }
        },
        EventKind::Fork { thread: true, .. } => format!("{who} starts a thread"),
        EventKind::Fork { thread: false, .. } => format!("{who} starts a child process"),
        EventKind::Exec { argv, .. } => format!("{who} runs {}", quoted(&argv.join(" "))),
        EventKind::Open { path, .. } => format!("{who} opens {path} for writing"),
        EventKind::Unlink { path } => format!("{who} deletes {path}"),
        EventKind::Rename { from, to } => format!("{who} renames {from} to {to}"),
        EventKind::Mark { text } => format!("{who} writes the mark {}", quoted(text)),
        EventKind::Console { text } => format!("the kernel logs {}", quoted(text)),
        EventKind::Unknown { kind, .. } => format!("{who} reports a record of kind {kind}"),
    }
}

/// When both runs did the same kind of thing, the part that differs.
fn detail(here: Party, there: Party) -> Option<String> {
    use std::mem::discriminant;
    let (a, b) = (&here.event.kind, &there.event.kind);
    if discriminant(a) != discriminant(b) {
        return None;
    }
    if a == b {
        let (x, y) = (here.thread?, there.thread?);
        return Some(format!(
            "Both do the same thing, but from thread {} in this run and thread {} in the other.",
            x + 1,
            y + 1
        ));
    }
    let text = match (a, b) {
        (EventKind::Output { fd: f, .. }, EventKind::Output { fd: g, .. }) if f == g => {
            "Both write to the same stream; the text differs."
        }
        (EventKind::Signal { signo: s, .. }, EventKind::Signal { signo: t, .. }) if s != t => {
            return Some(format!(
                "Both get a signal: {} in this run, {} in the other.",
                signal_name(*s),
                signal_name(*t)
            ));
        }
        (EventKind::Signal { .. }, EventKind::Signal { .. }) => {
            "Both get the same signal, at a different address."
        }
        (EventKind::Exit { .. }, EventKind::Exit { .. }) => "Both exit, with a different status.",
        (EventKind::Exec { .. }, EventKind::Exec { .. }) => {
            "Both run a program, with a different command line."
        }
        (EventKind::Open { .. }, EventKind::Open { .. })
        | (EventKind::Unlink { .. }, EventKind::Unlink { .. })
        | (EventKind::Rename { .. }, EventKind::Rename { .. }) => {
            "Both change a file, but not the same one."
        }
        (EventKind::Fork { .. }, EventKind::Fork { .. }) => {
            "Both start a new thread or process, with a different id."
        }
        _ => return None,
    };
    Some(text.to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn ago_says_the_largest_whole_unit() {
        // Seconds read as now; then minutes, hours and days, each by its
        // letter so the words fit beside a run in the Runs panel.
        use std::time::Duration;
        assert_eq!(ago(Duration::from_secs(30)), "just now");
        assert_eq!(ago(Duration::from_secs(60)), "1m ago");
        assert_eq!(ago(Duration::from_secs(5 * 3600 + 59)), "5h ago");
        assert_eq!(ago(Duration::from_secs(3 * 86_400)), "3d ago");
    }

    // Event descriptions and number formatting: each test describes one
    // event kind, or formats one number, and compares the exact text.
    use super::*;

    fn ev(kind: EventKind) -> Event {
        Event {
            step: 1,
            pid: 7,
            tid: 7,
            kind,
        }
    }

    #[test]
    fn writes_are_quoted_with_escapes() {
        // A write with a quote and a newline comes out as a quoted C string.
        let d = describe(&ev(EventKind::Output {
            fd: 1,
            bytes: b"hi \"x\"\n".to_vec(),
        }));
        assert_eq!(d.text, r#"write(1, "hi \"x\"\n")"#);
        assert_eq!(d.tone, EventTone::Normal);
    }

    #[test]
    fn crash_signals_and_nonzero_exits_are_errors() {
        // A SIGSEGV and a death by signal are errors; a clean thread exit is not.
        let d = describe(&ev(EventKind::Signal {
            signo: 11,
            code: 1,
            addr: 0xdead,
        }));
        assert_eq!(d.text, "SIGSEGV at 0xdead (si_code 1)");
        assert_eq!(d.tone, EventTone::Error);

        let d = describe(&ev(EventKind::Exit {
            status: 0x8b,
            comm: "test_pool".into(),
            thread: false,
        }));
        assert_eq!(d.text, "test_pool killed by SIGSEGV (core dumped)");
        assert_eq!(d.tone, EventTone::Error);

        let d = describe(&ev(EventKind::Exit {
            status: 0,
            comm: "cc".into(),
            thread: true,
        }));
        assert_eq!(d.text, "exit(0) cc");
        assert_eq!(d.tone, EventTone::Normal);
    }

    #[test]
    fn exec_clone_and_open_read_like_strace() {
        // An exec, a thread clone and an open, each against strace's spelling.
        let d = describe(&ev(EventKind::Exec {
            filename: "/bin/cc".into(),
            argv: vec!["cc".into(), "-c".into(), "pool.c".into()],
            old_pid: 7,
        }));
        assert_eq!(d.text, r#"execve("/bin/cc", ["cc", "-c", "pool.c"])"#);

        let d = describe(&ev(EventKind::Fork {
            child: 9,
            thread: true,
        }));
        assert_eq!(d.text, "clone(CLONE_THREAD) \u{2192} tid 9");

        let d = describe(&ev(EventKind::Open {
            path: "/build/a.o".into(),
            flags: 0o101101,
        }));
        assert_eq!(d.text, r#"openat("/build/a.o", O_WRONLY|O_CREAT|O_TRUNC)"#);
    }

    #[test]
    fn long_text_is_cut() {
        // Quoted text and clipped labels stop at their limits with an ellipsis.
        let long = "x".repeat(MAX_QUOTED_CHARS + 10);
        let q = quote(&long);
        assert_eq!(q.chars().count(), MAX_QUOTED_CHARS + 3);
        assert!(q.ends_with("\u{2026}\""));
        assert_eq!(clip("abcdef", 4), "abc\u{2026}");
        assert_eq!(clip("abc", 4), "abc");
    }

    #[test]
    fn thousands_are_grouped() {
        // Numbers of one to seven digits, grouped by threes.
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(11_760), "11,760");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn store_paths_lose_most_of_their_hash() {
        // Every store path in a line is shortened; paths outside the store
        // and store-like text without a full hash are left alone.
        assert_eq!(
            short_store_paths(
                "building '/nix/store/9x2kq8v1c7m3dzf0ha5slw4n6yrbp2jt-mylib-0.3.0.drv' with /nix/store/v6xdf1qz8l2mh0ksnb4ryc9j7w3agp5t-builder.sh"
            ),
            "building '/nix/store/9x2k\u{2026}-mylib-0.3.0.drv' with /nix/store/v6xd\u{2026}-builder.sh"
        );
        assert_eq!(short_store_paths("/tmp/run"), "/tmp/run");
        let full = "cd /nix/store/9x2kq8v1c7m3dzf0ha5slw4n6yrbp2jt-mylib-0.3.0 && make";
        let mapped = short_store_paths_mapped(full);
        assert_eq!(
            mapped.shown,
            "cd /nix/store/9x2k\u{2026}-mylib-0.3.0 && make"
        );
        assert_eq!(mapped.copy(0..mapped.shown.len()), full);
        let at = mapped.shown.find("-mylib").unwrap();
        assert_eq!(mapped.copy(at..mapped.shown.len()), "-mylib-0.3.0 && make");
        assert_eq!(
            short_store_paths("/nix/store/short-x"),
            "/nix/store/short-x"
        );
    }

    #[test]
    fn a_summary_says_where_and_in_which_thread() {
        // A write from a thread other than the main one names both ids.
        let mut e = ev(EventKind::Output {
            fd: 1,
            bytes: b"job 0\n".to_vec(),
        });
        e.step = 3_495;
        e.tid = 9;
        assert_eq!(
            summary(&e),
            "step 3,495 \u{b7} pid 7 tid 9: write(1, \"job 0\\n\")"
        );
    }

    #[test]
    fn a_divergence_reads_as_what_each_run_did_next() {
        // A crash in one run against a thread exit in the other names the
        // threads by their number in the program; two writes of different
        // text say that the text is what differs.
        let crash = ev(EventKind::Signal {
            signo: 11,
            code: 1,
            addr: 0x108,
        });
        let exit = ev(EventKind::Exit {
            status: 0,
            comm: "pool".into(),
            thread: true,
        });
        let nobody = |_| None;
        let d = difference(
            Some(Party {
                event: &crash,
                thread: Some(2),
            }),
            Some(Party {
                event: &exit,
                thread: Some(1),
            }),
            Some("pool-test"),
            &nobody,
        );
        assert_eq!(d.here, "thread 3 of pool-test gets SIGSEGV at 0x108");
        assert_eq!(d.there, "thread 2 of pool-test exits");
        assert_eq!(d.detail, None);

        let write = |text: &str| {
            ev(EventKind::Output {
                fd: 1,
                bytes: text.as_bytes().to_vec(),
            })
        };
        let (a, b) = (write("job 17 done\n"), write("job 16 done\n"));
        let party = |event| Party {
            event,
            thread: Some(1),
        };
        let d = difference(Some(party(&a)), Some(party(&b)), Some("pool-test"), &nobody);
        assert_eq!(
            d.here,
            "thread 2 of pool-test writes \"job 17 done\" to stdout"
        );
        assert_eq!(
            d.detail.as_deref(),
            Some("Both write to the same stream; the text differs.")
        );
    }

    #[test]
    fn long_texts_that_differ_late_show_where_they_differ() {
        // Two command lines alike for longer than a quote shows, then
        // different: both quotes start shortly before the difference, so
        // the words that differ are in both sentences. Texts that differ
        // early are quoted from their start.
        let exec = |last: &str| {
            ev(EventKind::Exec {
                filename: "/nix/store/x-gcc/bin/gcc".into(),
                argv: vec![
                    "gcc".into(),
                    "-O2".into(),
                    "-g".into(),
                    "-Wall".into(),
                    "-I/build/philosophers/include".into(),
                    "-c".into(),
                    last.into(),
                ],
                old_pid: 7,
            })
        };
        let (a, b) = (exec("fork.c"), exec("table.c"));
        let party = |event| Party {
            event,
            thread: None,
        };
        let named = |_| Some("gcc".to_string());
        let d = difference(Some(party(&a)), Some(party(&b)), None, &named);
        assert!(d.here.contains("fork.c"), "{}", d.here);
        assert!(d.there.contains("table.c"), "{}", d.there);
        assert!(d.here.starts_with("gcc runs \"\u{2026}"), "{}", d.here);

        let mut c = exec("fork.c");
        if let EventKind::Exec { argv, .. } = &mut c.kind {
            argv[1] = "-O0".into();
        }
        let early = difference(Some(party(&c)), Some(party(&a)), None, &named);
        assert!(
            early.here.starts_with("gcc runs \"gcc -O0"),
            "{}",
            early.here
        );
        assert!(
            early.there.starts_with("gcc runs \"gcc -O2"),
            "{}",
            early.there
        );
    }

    #[test]
    fn a_run_that_stops_early_and_a_same_event_from_another_thread() {
        // A side with no more events says so; the same event from another
        // thread names both threads.
        let exit = ev(EventKind::Exit {
            status: 2 << 8,
            comm: "make".into(),
            thread: false,
        });
        let named = |pid: u32| (pid == 7).then(|| "make".to_string());
        let d = difference(
            Some(Party {
                event: &exit,
                thread: None,
            }),
            None,
            None,
            &named,
        );
        assert_eq!(d.here, "make exits with status 2");
        assert_eq!(d.there, "the run ends");

        let d = difference(
            Some(Party {
                event: &exit,
                thread: Some(1),
            }),
            Some(Party {
                event: &exit,
                thread: Some(2),
            }),
            Some("make"),
            &named,
        );
        assert_eq!(
            d.detail.as_deref(),
            Some(
                "Both do the same thing, but from thread 2 in this run and thread 3 in the other."
            )
        );
    }
}
