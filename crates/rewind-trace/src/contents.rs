//! Which contents a file had at a step of a run, as far as its trace
//! tells.
//!
//! A file keeps its contents until something writes, renames or unlinks
//! it, and the trace records each of those, so whatever reads a file at one
//! step can serve every step with the same last such event: the engine's
//! cache of source files out of the VM, and the desktop app's file viewer.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use crate::{EventKind, Trace};

/// Which contents a path had at a step, as far as the trace tells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Version {
    /// No event wrote, renamed or unlinked the path, or a directory above
    /// it, up to the step: it holds what the run started with.
    Original,
    /// The last such event was at this step, a rename, an unlink or an
    /// open for writing, and nothing writes the file since: every process
    /// that opened it for writing had exited by the step.
    Since(u64),
    /// A process that opened the file for writing was still running at the
    /// step, and may write more. Not kept.
    Changing,
}

/// The version of `path` at `step` of the run `trace` is of.
///
/// An open for writing marks only the start of the writes, which the
/// trace does not record one by one, so its version is finished once
/// every process that opened the file has exited. A process one of them
/// forked after opening could hold the file open longer; builds do not
/// write source files that way.
pub fn version(trace: &Trace, path: &str, step: u64) -> Version {
    // Events name absolute paths without `.` or `..`.
    let Some(path) = normal(path) else {
        return Version::Changing;
    };
    let path = path.as_str();

    // The last event up to the step that changed the path, and the
    // processes that opened it for writing and had not exited by the
    // step. An earlier opener than the last may still hold the file and
    // write it after a later open, rename or unlink, so every opener
    // counts, and an open is finished once its process has exited.
    let mut last = None;
    let mut writers: HashSet<u32> = HashSet::new();
    for e in trace.until(step) {
        if changes(&e.kind, path) {
            last = Some(e.step);
            if let EventKind::Open { .. } = e.kind {
                writers.insert(e.pid);
            }
            continue;
        }
        if let EventKind::Exit { thread: false, .. } = e.kind
            && !writers.is_empty()
        {
            writers.remove(&e.pid);
        }
    }

    match last {
        None => Version::Original,
        Some(_) if !writers.is_empty() => Version::Changing,
        Some(at) => Version::Since(at),
    }
}

/// An absolute path with its `.` and `..` components resolved, as the
/// kernel resolves them when no symbolic link is on the way, such as
/// /build/mylib/src/pool.h for /build/mylib/tests/../src/pool.h. None for
/// a relative path.
pub fn normal(path: &str) -> Option<String> {
    let path = Path::new(path);
    if !path.is_absolute() {
        return None;
    }
    let mut parts: Vec<&std::ffi::OsStr> = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part),
            Component::ParentDir => {
                parts.pop();
            }
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    let joined: PathBuf = parts.iter().collect();
    Some(format!("/{}", joined.to_str()?))
}

/// Whether an event of `kind` changes the file at `path`: writes it,
/// renames or unlinks it, or renames or removes a directory above it.
fn changes(kind: &EventKind, path: &str) -> bool {
    let at_or_above = |changed: &str| {
        path == changed
            || path
                .strip_prefix(changed)
                .is_some_and(|rest| rest.starts_with('/'))
    };
    match kind {
        EventKind::Open { path: opened, .. } => opened == path,
        EventKind::Unlink { path: unlinked } => at_or_above(unlinked),
        EventKind::Rename { from, to } => at_or_above(from) || at_or_above(to),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    // The version of a path from hand-built traces of a Nix build's
    // unpacking, renaming and cleaning up.
    use super::*;
    use crate::Event;

    /// An event of `kind` by process `pid` at `step`.
    fn event(step: u64, pid: u32, kind: EventKind) -> Event {
        Event {
            step,
            pid,
            tid: pid,
            kind,
        }
    }

    fn open(step: u64, pid: u32, path: &str) -> Event {
        event(
            step,
            pid,
            EventKind::Open {
                path: path.into(),
                flags: 0o301,
            },
        )
    }

    fn exit(step: u64, pid: u32) -> Event {
        event(
            step,
            pid,
            EventKind::Exit {
                status: 0,
                comm: "tar".into(),
                thread: false,
            },
        )
    }

    /// tar, pid 76, writes pool.c at 10 and exits at 20; mv, pid 80,
    /// renames the tree at 30; rm, pid 90, unlinks pool.c at 40.
    fn build() -> Trace {
        Trace {
            events: vec![
                open(10, 76, "/build/src/pool.c"),
                exit(20, 76),
                event(
                    30,
                    80,
                    EventKind::Rename {
                        from: "/build/src".into(),
                        to: "/build/mylib".into(),
                    },
                ),
                event(
                    40,
                    90,
                    EventKind::Unlink {
                        path: "/build/mylib/pool.c".into(),
                    },
                ),
            ],
        }
    }

    #[test]
    fn a_path_s_version_is_its_last_finished_change() {
        // Before tar opens pool.c it is the original; while tar runs it is
        // changing; once tar exits it is the version tar wrote. Renaming
        // the directory above a path, or unlinking the path, is a version
        // too. A path no event names stays the original, a path through
        // `..` is the one it leads to, and a relative path, which no event
        // names, is taken to be changing.
        let trace = build();
        assert_eq!(version(&trace, "/build/src/pool.c", 5), Version::Original);
        assert_eq!(version(&trace, "/build/src/pool.c", 15), Version::Changing);
        assert_eq!(version(&trace, "/build/src/pool.c", 25), Version::Since(10));
        assert_eq!(version(&trace, "/build/src/pool.c", 35), Version::Since(30));
        assert_eq!(
            version(&trace, "/build/mylib/pool.c", 35),
            Version::Since(30)
        );
        assert_eq!(
            version(&trace, "/build/mylib/pool.c", 45),
            Version::Since(40)
        );
        assert_eq!(
            version(&trace, "/build/mylib/main.c", 45),
            Version::Since(30)
        );
        assert_eq!(
            version(&trace, "/build/mylibx/main.c", 45),
            Version::Original
        );
        assert_eq!(version(&trace, "/src/main.c", 45), Version::Original);
        assert_eq!(
            version(&trace, "/build/src/./tests/../pool.c", 25),
            Version::Since(10)
        );
        assert_eq!(version(&trace, "src/pool.c", 5), Version::Changing);
    }

    #[test]
    fn a_path_is_changing_while_any_process_that_opened_it_runs() {
        // Process 100 opens out.c at 10, process 200 opens it again at 20
        // and exits at 30, and process 100 exits at 50. Between 30 and 50
        // process 100 may still write the file, so it is changing; once
        // both have exited it is the version of the last open. Process
        // 100 holding the file open across an unlink at 40 keeps it
        // changing too.
        let trace = Trace {
            events: vec![
                open(10, 100, "/build/out.c"),
                open(20, 200, "/build/out.c"),
                exit(30, 200),
                exit(50, 100),
            ],
        };
        assert_eq!(version(&trace, "/build/out.c", 40), Version::Changing);
        assert_eq!(version(&trace, "/build/out.c", 55), Version::Since(20));

        let mut unlinked = trace.clone();
        unlinked.events.insert(
            3,
            event(
                40,
                300,
                EventKind::Unlink {
                    path: "/build/out.c".into(),
                },
            ),
        );
        assert_eq!(version(&unlinked, "/build/out.c", 45), Version::Changing);
        assert_eq!(version(&unlinked, "/build/out.c", 55), Version::Since(40));
    }
}
