//! Events and numbers as the scrubber prints them.
//!
//! The "At this step" card shows the last event before the playhead as the
//! system call or signal that produced it, the way strace would, so that
//! `write(1, "...")`, `execve(...)` and `SIGSEGV at 0x...` read the same as
//! in any other Linux tool.

use rewind_trace::{Event, EventKind, signal_name};

use crate::model::{ExitStatus, signo};

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
    const STORE: &str = "/nix/store/";
    const HASH_LEN: usize = 32;
    const HASH_SHOWN: usize = 4;
    const SEPARATOR: u8 = b'-';

    let mut out = String::with_capacity(text.len());
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
        out.push_str(&after[..HASH_SHOWN]);
        out.push(ELLIPSIS);
        rest = &after[HASH_LEN..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
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
}
