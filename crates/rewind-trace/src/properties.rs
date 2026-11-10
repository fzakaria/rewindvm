//! Properties of a run that its trace alone decides, for `rewind check
//! --property`.
//!
//! Each looks at what the job's processes did, from the job's start to its
//! exit, and returns the first step that breaks the property and why. They
//! judge what a run did rather than how it ended, so they can find a bug in
//! a run that passed: a file truncated in place is a bug whether or not a
//! reader was unlucky enough to see it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Bound;

use crate::ending::ExitStatus;
use crate::{EventKind, Process, Trace, signal_name};

/// Signals whose default action dumps core: a process killed by one
/// crashed, rather than being asked to stop.
const CRASH_SIGNALS: [u32; 7] = [
    4,  // SIGILL
    5,  // SIGTRAP
    6,  // SIGABRT
    7,  // SIGBUS
    8,  // SIGFPE
    11, // SIGSEGV
    31, // SIGSYS
];

/// O_TRUNC on x86-64 Linux.
const O_TRUNC: u32 = 0o1000;

/// Directories whose files are devices and kernel state, not files a job
/// leaves behind: writing /dev/null is not writing outside the job's
/// directories.
const PSEUDO_DIRS: [&str; 3] = ["/dev", "/proc", "/sys"];

/// What separates the components of a guest path.
const PATH_SEPARATOR: char = '/';

/// Where and why a run breaks a property.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub step: u64,
    pub why: String,
}

/// A path pattern: `*` matches within one path component, `**` across
/// components, and `?` one character. A pattern without a `/` matches a
/// path's file name, so `*.conf` matches /etc/app/app.conf.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Glob(String);

impl Glob {
    pub fn new(pattern: &str) -> Glob {
        Glob(pattern.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether `path` matches.
    pub fn matches(&self, path: &str) -> bool {
        let subject = if self.0.contains('/') {
            path
        } else {
            path.rsplit('/').next().unwrap_or(path)
        };
        glob_match(self.0.as_bytes(), subject.as_bytes())
    }
}

/// Whether `text` matches `pattern`, as [`Glob`] describes.
fn glob_match(pattern: &[u8], text: &[u8]) -> bool {
    match pattern {
        [] => text.is_empty(),
        [b'*', b'*', rest @ ..] => (0..=text.len()).any(|i| glob_match(rest, &text[i..])),
        [b'*', rest @ ..] => {
            let component = text.iter().position(|&c| c == b'/').unwrap_or(text.len());
            (0..=component).any(|i| glob_match(rest, &text[i..]))
        }
        [b'?', rest @ ..] => matches!(text, [c, tail @ ..] if *c != b'/' && glob_match(rest, tail)),
        [p, rest @ ..] => matches!(text, [c, tail @ ..] if c == p && glob_match(rest, tail)),
    }
}

/// The job's processes and the steps it ran over.
struct Job {
    start: u64,
    exit: u64,
    processes: Vec<Process>,
}

impl Job {
    /// The job of `trace`: the first process init forked once the job
    /// started, and every process forked from it. None when the job never
    /// started.
    fn of(trace: &Trace) -> Option<Job> {
        let start = trace.job_start()?;
        let exit = trace
            .job_exit()
            .map_or_else(|| trace.last_step(), |e| e.step);
        let mut all = trace.processes();
        all.sort_by_key(|p| p.start);

        // The root, then each process whose parent is in the job and was
        // alive when it was forked.
        let mut processes: Vec<Process> = Vec::new();
        for p in all {
            let root =
                processes.is_empty() && p.parent == rewind_init::INIT_PID && p.start >= start;
            let child = processes.iter().any(|m| {
                m.pid == p.parent && m.start <= p.start && m.end.is_none_or(|end| p.start <= end)
            });
            if root || child {
                processes.push(p);
            }
        }
        Some(Job {
            start,
            exit,
            processes,
        })
    }

    /// The pids of the job's processes.
    fn pids(&self) -> HashSet<u32> {
        self.processes.iter().map(|p| p.pid).collect()
    }

    /// Whether process `ancestor` forked process `pid`, directly or
    /// through its children, as the processes alive at `step` are
    /// related.
    fn descends(&self, pid: u32, ancestor: u32, step: u64) -> bool {
        let alive = |pid: u32| {
            self.processes
                .iter()
                .rfind(|p| p.pid == pid && p.start <= step)
        };
        let mut seen = HashSet::new();
        let mut at = alive(pid);
        while let Some(p) = at {
            if p.parent == ancestor {
                return true;
            }
            if !seen.insert(p.pid) {
                return false;
            }
            at = alive(p.parent);
        }
        false
    }

    /// A process of the job by pid, as it was at `step`, for its name.
    fn name(&self, pid: u32, step: u64) -> String {
        self.processes
            .iter()
            .rfind(|p| p.pid == pid && p.start <= step)
            .map_or_else(|| format!("pid {pid}"), |p| short(&p.as_of(step).name()))
    }
}

/// A command's file name without its directory.
fn short(command: &str) -> String {
    command.rsplit('/').next().unwrap_or(command).to_string()
}

/// No process of the job crashed: none was killed by a signal that dumps
/// core, such as SIGSEGV or SIGABRT. A crash a retry or a supervisor
/// hides from the job's exit status still counts.
pub fn no_crash(trace: &Trace) -> Option<Finding> {
    let job = Job::of(trace)?;
    job.processes
        .iter()
        .filter_map(|p| {
            let end = p.end?;
            let ExitStatus::Signal { signo, .. } = ExitStatus::from_raw(p.status?) else {
                return None;
            };
            CRASH_SIGNALS.contains(&signo).then(|| Finding {
                step: end,
                why: format!(
                    "process {} ({}) died of {}",
                    p.pid,
                    short(&p.as_of(end).name()),
                    signal_name(signo)
                ),
            })
        })
        .min_by_key(|f| f.step)
}

/// Every process the job started had exited when its main process did:
/// no server, background task or child left running. Init stops what is
/// left with SIGKILL once the main process exits, before it marks the
/// job's exit, so a process that outlived the main process is one the
/// job left behind.
pub fn no_orphans(trace: &Trace) -> Option<Finding> {
    let job = Job::of(trace)?;
    let (main, rest) = job.processes.split_first()?;
    let main_end = main.end.unwrap_or(job.exit);
    rest.iter()
        .filter(|p| p.end.is_none_or(|end| end > main_end))
        .map(|p| Finding {
            step: p.start,
            why: format!(
                "process {} ({}), started at step {}, was still running when the job's main process exited at step {main_end}",
                p.pid,
                short(&p.as_of(main_end).name()),
                p.start,
            ),
        })
        .min_by_key(|f| f.step)
}

/// The processes that have each file open for writing, as far as the
/// trace tells: from an open for writing until the process closes the
/// file's last reference or exits. A close by a process that did not open
/// the file is by a child that inherited it, and ends its ancestor's write.
#[derive(Default)]
struct Writers<'t> {
    open: HashMap<&'t str, Vec<(u32, u64)>>,
}

impl<'t> Writers<'t> {
    /// Follows `e`: an open for writing, a close, or a process's exit.
    fn follow(&mut self, job: &Job, e: &'t crate::Event) {
        match &e.kind {
            EventKind::Open { path, .. } => {
                let open = self.open.entry(path.as_str()).or_default();
                if !open.iter().any(|(pid, _)| *pid == e.pid) {
                    open.push((e.pid, e.step));
                }
            }
            EventKind::Close { path } => {
                if let Some(open) = self.open.get_mut(path.as_str()) {
                    if open.iter().any(|(pid, _)| *pid == e.pid) {
                        open.retain(|(pid, _)| *pid != e.pid);
                    } else {
                        open.retain(|(pid, _)| !job.descends(e.pid, *pid, e.step));
                    }
                }
            }
            EventKind::Exit { thread: false, .. } => {
                self.open
                    .values_mut()
                    .for_each(|w| w.retain(|(pid, _)| *pid != e.pid));
            }
            _ => {}
        }
    }

    /// The processes writing `path`, with the steps they opened it on.
    fn of(&self, path: &str) -> &[(u32, u64)] {
        self.open.get(path).map_or(&[], Vec::as_slice)
    }
}

/// No two of the job's processes had a file matching `glob` open for
/// writing at once. Devices and kernel state, such as /dev/null, are not
/// files and are left out. A process writes a file from its open until it
/// closes the file or exits, and a process that opens a file its own
/// ancestor or descendant has open is taken as handed it: gcc creates a
/// temporary file that its child cc1 then writes.
pub fn single_writer(trace: &Trace, glob: &Glob) -> Option<Finding> {
    let job = Job::of(trace)?;
    let pids = job.pids();
    let mut writers = Writers::default();
    for e in job_events(trace, &job) {
        if let EventKind::Open { path, .. } = &e.kind
            && pids.contains(&e.pid)
            && !under(path, &PSEUDO_DIRS)
            && glob.matches(path)
        {
            let unrelated = |other: u32| {
                other != e.pid
                    && !job.descends(e.pid, other, e.step)
                    && !job.descends(other, e.pid, e.step)
            };
            if let Some(&(other, since)) = writers.of(path).iter().find(|(pid, _)| unrelated(*pid))
            {
                return Some(Finding {
                    step: e.step,
                    why: format!(
                        "process {} ({}) opened {path} for writing while process {other} ({}), which opened it at step {since}, had not closed it",
                        e.pid,
                        job.name(e.pid, e.step),
                        job.name(other, since)
                    ),
                });
            }
        }
        writers.follow(&job, e);
    }
    None
}

/// No process of the job opened a file matching `glob` for reading while
/// another process had it open for writing, where the reader can see it
/// half written. The guest records such an open only when the kernel
/// counts a writer of the file at that moment, so a read of a finished
/// file costs nothing.
pub fn no_read_while_written(trace: &Trace, glob: &Glob) -> Option<Finding> {
    let job = Job::of(trace)?;
    let pids = job.pids();
    let mut writers = Writers::default();
    for e in job_events(trace, &job) {
        if let EventKind::ReadOpen { path, .. } = &e.kind
            && pids.contains(&e.pid)
            && !under(path, &PSEUDO_DIRS)
            && glob.matches(path)
        {
            let open = writers.of(path);
            let reader = format!("process {} ({})", e.pid, job.name(e.pid, e.step));
            let writer = open.iter().find(|(pid, _)| *pid != e.pid);
            match writer {
                Some(&(other, since)) => {
                    return Some(Finding {
                        step: e.step,
                        why: format!(
                            "{reader} opened {path} for reading while process {other} ({}), which opened it for writing at step {since}, had not closed it",
                            job.name(other, since)
                        ),
                    });
                }
                // A writer the trace does not know: one that opened the
                // file before the job started, or a process outside it.
                None if open.is_empty() => {
                    return Some(Finding {
                        step: e.step,
                        why: format!(
                            "{reader} opened {path} for reading while another process had it open for writing"
                        ),
                    });
                }
                // The reader writes the file itself.
                None => {}
            }
        }
        writers.follow(&job, e);
    }
    None
}

/// No file matching `glob` that the run had already written was opened
/// with O_TRUNC, or removed and then written again in place: each was
/// replaced by writing another file and renaming it over, so a reader
/// never sees it missing, empty or half written. A file the run started
/// with is not known to exist, so its first truncation or removal is not
/// counted, and devices such as /dev/null are not files.
pub fn atomic_replace(trace: &Trace, glob: &Glob) -> Option<Finding> {
    let job = Job::of(trace)?;
    let pids = job.pids();

    // The files the run has written or renamed into place, with the step,
    // and the ones it had that were removed since: by whom, when, and when
    // the run had written them.
    let mut existing: HashMap<&str, u64> = HashMap::new();
    let mut removed: HashMap<&str, Removal> = HashMap::new();
    for e in trace.events.iter().filter(|e| e.step <= job.exit) {
        match &e.kind {
            EventKind::Open { path, flags } => {
                let watched = e.step >= job.start
                    && pids.contains(&e.pid)
                    && !under(path, &PSEUDO_DIRS)
                    && glob.matches(path);
                let writer = || format!("process {} ({})", e.pid, job.name(e.pid, e.step));

                // A truncation of a file the run wrote.
                if let Some(&since) = existing.get(path.as_str())
                    && watched
                    && flags & O_TRUNC != 0
                {
                    return Some(Finding {
                        step: e.step,
                        why: format!(
                            "{} truncated {path} in place, which the run wrote at step {since}; a reader can see it empty or half written",
                            writer()
                        ),
                    });
                }

                // A write in place of a file the run wrote and then removed.
                if let Some(gone) = removed.get(path.as_str())
                    && watched
                {
                    return Some(Finding {
                        step: e.step,
                        why: format!(
                            "{} wrote {path} in place after process {} ({}) removed it at step {}, which the run wrote at step {}; a reader can see it missing or half written",
                            writer(),
                            gone.pid,
                            job.name(gone.pid, gone.step),
                            gone.step,
                            gone.written,
                        ),
                    });
                }
                removed.remove(path.as_str());
                existing.insert(path.as_str(), e.step);
            }
            EventKind::Rename { from, to } => {
                existing.remove(from.as_str());
                removed.remove(to.as_str());
                existing.insert(to.as_str(), e.step);
            }
            EventKind::Unlink { path } => {
                if let Some(written) = existing.remove(path.as_str()) {
                    let removal = Removal {
                        pid: e.pid,
                        step: e.step,
                        written,
                    };
                    removed.insert(path.as_str(), removal);
                }
            }
            _ => {}
        }
    }
    None
}

/// A file the run had written that a process removed: which process, at
/// which step, and the step the run had written the file at.
struct Removal {
    pid: u32,
    step: u64,
    written: u64,
}

/// The job wrote, renamed and removed files only under `dirs`, and in
/// /dev, /proc and /sys.
pub fn writes_within(trace: &Trace, dirs: &[String]) -> Option<Finding> {
    let job = Job::of(trace)?;
    let pids = job.pids();
    let dirs_given: Vec<&str> = dirs.iter().map(|d| d.trim_end_matches('/')).collect();
    let inside = |path: &str| under(path, &dirs_given) || under(path, &PSEUDO_DIRS);
    let within = dirs.join(", ");
    for e in job_events(trace, &job).filter(|e| pids.contains(&e.pid)) {
        let did = match &e.kind {
            EventKind::Open { path, .. } if !inside(path) => format!("opened {path} for writing"),
            EventKind::Unlink { path } if !inside(path) => format!("removed {path}"),
            EventKind::Rename { from, to } if !inside(from) || !inside(to) => {
                format!("renamed {from} to {to}")
            }
            _ => continue,
        };
        return Some(Finding {
            step: e.step,
            why: format!(
                "process {} ({}) {did}, outside {within}",
                e.pid,
                job.name(e.pid, e.step)
            ),
        });
    }
    None
}

/// The files matching `glob` that the job's processes wrote and that were
/// still there when the job exited, in the order first written: a file
/// renamed into place counts by its new name, a file under a renamed
/// directory counts by its name under the directory's new name, and one
/// removed or renamed away does not count. Devices are not files.
pub fn written_files(trace: &Trace, glob: &Glob) -> Vec<String> {
    let Some(job) = Job::of(trace) else {
        return Vec::new();
    };
    let pids = job.pids();
    let is_file = |path: &str| !under(path, &PSEUDO_DIRS);

    // Every file the job wrote that is still there, by path, with its
    // place in the order first written. `glob` is applied at the end: a
    // renamed directory can move a file to a name that matches.
    let mut files: BTreeMap<String, usize> = BTreeMap::new();
    let mut next_place = 0;
    for e in job_events(trace, &job) {
        let by_job = pids.contains(&e.pid);
        match &e.kind {
            EventKind::Open { path, .. } if by_job && is_file(path) => {
                files.entry(path.clone()).or_insert(next_place);
                next_place += 1;
            }
            EventKind::Rename { from, to } => {
                files.remove(from);

                // A directory: each file under the old name is now under
                // the new one, in its old place. The directory itself is
                // not a file.
                let moved = take_under(&mut files, from);
                if !moved.is_empty() {
                    for (path, place) in moved {
                        let renamed = format!("{to}{}", &path[from.len()..]);
                        if by_job && is_file(&renamed) {
                            files.entry(renamed).or_insert(place);
                        }
                    }
                    continue;
                }

                // A file, as far as the trace tells.
                if by_job && is_file(to) {
                    files.entry(to.clone()).or_insert(next_place);
                    next_place += 1;
                }
            }
            EventKind::Unlink { path } => {
                files.remove(path);
                take_under(&mut files, path);
            }
            _ => {}
        }
    }

    // The files that match, in the order first written.
    let mut left: Vec<(usize, String)> = files
        .into_iter()
        .filter(|(path, _)| glob.matches(path))
        .map(|(path, place)| (place, path))
        .collect();
    left.sort_unstable();
    left.into_iter().map(|(_, path)| path).collect()
}

/// Takes the entries of `files` whose paths are under directory `dir`
/// out of `files`, and returns them.
fn take_under(files: &mut BTreeMap<String, usize>, dir: &str) -> Vec<(String, usize)> {
    // The paths under `dir` sort together, from `dir` with a separator
    // after it.
    let prefix = format!("{dir}{PATH_SEPARATOR}");
    let paths: Vec<String> = files
        .range::<str, _>((Bound::Included(prefix.as_str()), Bound::Unbounded))
        .take_while(|(path, _)| path.starts_with(&prefix))
        .map(|(path, _)| path.clone())
        .collect();
    paths
        .into_iter()
        .filter_map(|path| files.remove_entry(&path))
        .collect()
}

/// Whether `path` is one of `dirs` or under one.
fn under(path: &str, dirs: &[&str]) -> bool {
    dirs.iter().any(|dir| {
        path == *dir
            || path
                .strip_prefix(dir)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// The events from the job's start to its exit.
fn job_events<'a>(trace: &'a Trace, job: &Job) -> impl Iterator<Item = &'a crate::Event> {
    let (start, exit) = (job.start, job.exit);
    trace
        .events
        .iter()
        .filter(move |e| e.step >= start && e.step <= exit)
}

#[cfg(test)]
mod tests {
    // Each property on hand-built traces of a job: init (pid 1) marks the
    // start, forks the job, and marks its exit, and the job's processes do
    // what each test needs in between.
    use super::*;
    use crate::Event;

    const JOB: u32 = 40;

    fn event(step: u64, pid: u32, kind: EventKind) -> Event {
        Event {
            step,
            pid,
            tid: pid,
            kind,
        }
    }

    fn mark(step: u64, text: &str) -> Event {
        event(step, 1, EventKind::Mark { text: text.into() })
    }

    fn fork(step: u64, parent: u32, child: u32) -> Event {
        event(
            step,
            parent,
            EventKind::Fork {
                child,
                thread: false,
            },
        )
    }

    fn exec(step: u64, pid: u32, program: &str) -> Event {
        event(
            step,
            pid,
            EventKind::Exec {
                filename: program.into(),
                argv: vec![program.into()],
                old_pid: pid,
            },
        )
    }

    fn exit(step: u64, pid: u32, status: u32) -> Event {
        event(
            step,
            pid,
            EventKind::Exit {
                status,
                comm: String::new(),
                thread: false,
            },
        )
    }

    fn open(step: u64, pid: u32, path: &str, flags: u32) -> Event {
        event(
            step,
            pid,
            EventKind::Open {
                path: path.into(),
                flags,
            },
        )
    }

    /// A trace of a job started at step 10 as pid 40, running `body`,
    /// its main process exiting 0 at step 990 and init marking the
    /// job's exit at step 1000.
    fn job(body: Vec<Event>) -> Trace {
        let mut events = vec![
            mark(10, "rewind-start"),
            fork(11, 1, JOB),
            exec(12, JOB, "/bin/sh"),
        ];
        events.extend(body);
        events.push(exit(990, JOB, 0));
        events.push(mark(1000, "rewind-exit 0"));
        events.sort_by_key(|e| e.step);
        Trace {
            events,
            ..Default::default()
        }
    }

    const CREATE: u32 = 0o101; // O_WRONLY | O_CREAT
    const REWRITE: u32 = 0o1101; // O_WRONLY | O_CREAT | O_TRUNC

    // A pattern without a slash matches the file name; `*` stays within a
    // component and `**` crosses them.
    #[test]
    fn globs_match_names_and_paths() {
        assert!(Glob::new("*.conf").matches("/build/app/app.conf"));
        assert!(!Glob::new("*.conf").matches("/build/app.conf.bak"));
        assert!(Glob::new("/build/*/app.conf").matches("/build/x/app.conf"));
        assert!(!Glob::new("/build/*/app.conf").matches("/build/x/y/app.conf"));
        assert!(Glob::new("/build/**/app.conf").matches("/build/x/y/app.conf"));
        assert!(Glob::new("/tmp/test-?/log").matches("/tmp/test-3/log"));
        assert!(!Glob::new("/tmp/test-?/log").matches("/tmp/test-33/log"));
    }

    // A child that dies of SIGSEGV is a crash even though the job exits
    // 0; one that exits 1, or is killed by SIGTERM, is not.
    #[test]
    fn a_crash_hidden_by_a_retry_is_found() {
        let sigsegv = 11;
        let sigterm = 15;
        let trace = job(vec![
            fork(100, JOB, 41),
            exec(101, 41, "/bin/worker"),
            exit(150, 41, sigsegv),
            fork(200, JOB, 42),
            exit(250, 42, 1 << 8),
            fork(300, JOB, 43),
            exit(350, 43, sigterm),
        ]);
        let found = no_crash(&trace).unwrap();
        assert_eq!(found.step, 150);
        assert_eq!(found.why, "process 41 (worker) died of SIGSEGV");

        let calm = job(vec![fork(200, JOB, 42), exit(250, 42, 1 << 8)]);
        assert_eq!(no_crash(&calm), None);
    }

    // A process the job forked that outlives the job's main process is
    // an orphan, even once init has killed it; one that exits before is
    // not, and neither is anything init itself started.
    #[test]
    fn a_process_left_running_is_an_orphan() {
        let sigkill = 9;
        let trace = job(vec![
            fork(100, JOB, 41),
            exec(101, 41, "/bin/server"),
            fork(200, JOB, 42),
            exit(250, 42, 0),
            fork(300, 1, 60),
            exit(995, 41, sigkill),
        ]);
        let found = no_orphans(&trace).unwrap();
        assert_eq!(found.step, 100);
        assert_eq!(
            found.why,
            "process 41 (server), started at step 100, was still running when the job's main process exited at step 990"
        );

        let tidy = job(vec![fork(100, JOB, 41), exit(500, 41, 0)]);
        assert_eq!(no_orphans(&tidy), None);
    }

    // Two live processes with one file open for writing break
    // single-writer; the same file written by one process after the
    // other exited does not, and neither does a file the glob leaves out.
    #[test]
    fn two_live_writers_of_one_file_are_found() {
        let trace = job(vec![
            fork(100, JOB, 41),
            fork(101, JOB, 42),
            open(110, 41, "/build/out.log", CREATE),
            open(120, 42, "/build/out.log", CREATE),
        ]);
        let found = single_writer(&trace, &Glob::new("*.log")).unwrap();
        let devices = job(vec![
            fork(100, JOB, 41),
            open(110, JOB, "/dev/null", CREATE),
            open(120, 41, "/dev/null", CREATE),
        ]);
        assert_eq!(single_writer(&devices, &Glob::new("**")), None);
        let handed = job(vec![
            fork(100, JOB, 41),
            exec(101, 41, "/bin/gcc"),
            open(110, 41, "/build/cc.s", CREATE),
            fork(111, 41, 42),
            exec(112, 42, "/bin/cc1"),
            open(120, 42, "/build/cc.s", CREATE),
        ]);
        assert_eq!(single_writer(&handed, &Glob::new("**")), None);
        let closed = job(vec![
            fork(100, JOB, 41),
            fork(101, JOB, 42),
            open(110, 41, "/build/out.log", CREATE),
            event(
                115,
                41,
                EventKind::Close {
                    path: "/build/out.log".into(),
                },
            ),
            open(120, 42, "/build/out.log", CREATE),
        ]);
        assert_eq!(single_writer(&closed, &Glob::new("*.log")), None);
        assert_eq!(found.step, 120);
        assert!(found.why.starts_with("process 42 (sh) opened /build/out.log for writing while process 41 (sh), which opened it at step 110"));
        assert_eq!(single_writer(&trace, &Glob::new("*.txt")), None);

        let in_turn = job(vec![
            fork(100, JOB, 41),
            open(110, 41, "/build/out.log", CREATE),
            exit(115, 41, 0),
            fork(116, JOB, 42),
            open(120, 42, "/build/out.log", CREATE),
        ]);
        assert_eq!(single_writer(&in_turn, &Glob::new("*.log")), None);
    }

    // Truncating a file the run wrote earlier is a rewrite in place;
    // creating it with O_TRUNC the first time, or renaming a new file
    // over it, is not.
    #[test]
    fn a_rewrite_in_place_is_found() {
        let in_place = job(vec![
            open(100, JOB, "/build/app.conf", REWRITE),
            open(200, JOB, "/build/app.conf", REWRITE),
        ]);
        let found = atomic_replace(&in_place, &Glob::new("*.conf")).unwrap();
        assert_eq!(found.step, 200);
        assert!(
            found
                .why
                .contains("truncated /build/app.conf in place, which the run wrote at step 100")
        );

        let renamed = job(vec![
            open(100, JOB, "/build/app.conf", REWRITE),
            open(200, JOB, "/build/app.conf.tmp", REWRITE),
            event(
                210,
                JOB,
                EventKind::Rename {
                    from: "/build/app.conf.tmp".into(),
                    to: "/build/app.conf".into(),
                },
            ),
            open(300, JOB, "/build/app.conf.tmp", REWRITE),
        ]);
        assert_eq!(atomic_replace(&renamed, &Glob::new("*.conf")), None);
        let device = job(vec![
            open(100, JOB, "/dev/null", REWRITE),
            open(200, JOB, "/dev/null", REWRITE),
        ]);
        assert_eq!(atomic_replace(&device, &Glob::new("**")), None);
    }

    // Removing a file the run wrote and writing it again in place, as
    // nix-direnv's cache renewal did with its .rc, is a rewrite in place
    // too: a reader in between finds the file missing, then half written.
    // Removing a file and renaming a new one into its place is not, nor is
    // writing a file the run never had.
    #[test]
    fn a_remove_then_rewrite_is_found() {
        let unlink = |step, pid, path: &str| {
            event(
                step,
                pid,
                EventKind::Unlink {
                    path: path.to_string(),
                },
            )
        };
        let rewritten = job(vec![
            fork(90, JOB, 41),
            open(100, JOB, "/p/.direnv/flake-profile.rc", REWRITE),
            unlink(150, 41, "/p/.direnv/flake-profile.rc"),
            open(200, 41, "/p/.direnv/flake-profile.rc", REWRITE),
        ]);
        let found = atomic_replace(&rewritten, &Glob::new("*.rc")).unwrap();
        assert_eq!(found.step, 200);
        assert!(found.why.contains(
            "wrote /p/.direnv/flake-profile.rc in place after process 41 (sh) removed it at step 150, which the run wrote at step 100; a reader can see it missing or half written"
        ), "{}", found.why);

        let renamed = job(vec![
            open(100, JOB, "/build/app.rc", REWRITE),
            unlink(150, JOB, "/build/app.rc"),
            open(200, JOB, "/build/app.rc.tmp", REWRITE),
            event(
                210,
                JOB,
                EventKind::Rename {
                    from: "/build/app.rc.tmp".into(),
                    to: "/build/app.rc".into(),
                },
            ),
        ]);
        assert_eq!(atomic_replace(&renamed, &Glob::new("*.rc")), None);

        let never_written = job(vec![
            unlink(150, JOB, "/build/app.rc"),
            open(200, JOB, "/build/app.rc", REWRITE),
        ]);
        assert_eq!(atomic_replace(&never_written, &Glob::new("*.rc")), None);
    }

    // A read of a file another process is writing is found, naming the
    // writer; a read after the writer closed it is never recorded, and a
    // process reading the file it writes itself is fine.
    #[test]
    fn a_read_of_a_file_being_written_is_found() {
        let read = |step: u64, pid: u32| {
            event(
                step,
                pid,
                EventKind::ReadOpen {
                    path: "/build/app.conf".into(),
                    flags: 0,
                },
            )
        };
        let trace = job(vec![
            fork(100, JOB, 41),
            exec(101, 41, "/bin/reader"),
            open(110, JOB, "/build/app.conf", REWRITE),
            read(120, 41),
        ]);
        let found = no_read_while_written(&trace, &Glob::new("*.conf")).unwrap();
        assert_eq!(found.step, 120);
        assert_eq!(
            found.why,
            "process 41 (reader) opened /build/app.conf for reading while process 40 (sh), which opened it for writing at step 110, had not closed it"
        );
        assert_eq!(no_read_while_written(&trace, &Glob::new("*.txt")), None);

        let own = job(vec![
            open(110, JOB, "/build/app.conf", REWRITE),
            read(120, JOB),
        ]);
        assert_eq!(no_read_while_written(&own, &Glob::new("*.conf")), None);

        let unknown = job(vec![read(120, JOB)]);
        let found = no_read_while_written(&unknown, &Glob::new("*.conf")).unwrap();
        assert!(
            found
                .why
                .ends_with("while another process had it open for writing")
        );
    }

    // The files a job wrote and left in place: one renamed into place by
    // its new name, and none removed, renamed away, or not matching.
    #[test]
    fn written_files_are_the_ones_left_in_place() {
        let trace = job(vec![
            open(100, JOB, "/build/reports/TEST-a.xml", CREATE),
            open(110, JOB, "/build/reports/TEST-b.xml.tmp", CREATE),
            event(
                120,
                JOB,
                EventKind::Rename {
                    from: "/build/reports/TEST-b.xml.tmp".into(),
                    to: "/build/reports/TEST-b.xml".into(),
                },
            ),
            open(130, JOB, "/build/reports/TEST-c.xml", CREATE),
            event(
                140,
                JOB,
                EventKind::Unlink {
                    path: "/build/reports/TEST-c.xml".into(),
                },
            ),
            open(150, JOB, "/build/notes.txt", CREATE),
            open(160, JOB, "/build/reports/TEST-a.xml", CREATE),
        ]);
        assert_eq!(
            written_files(&trace, &Glob::new("/build/reports/*.xml")),
            vec![
                "/build/reports/TEST-a.xml".to_string(),
                "/build/reports/TEST-b.xml".to_string(),
            ]
        );
    }

    // A directory the job renamed, as stdenv's moveToOutput does: the
    // files written under the old name are listed under the new one, in
    // the order first written, also when only the new names match the
    // pattern, and the directory is not listed as a file. A directory
    // removed takes the files under it off the list.
    #[test]
    fn written_files_follow_a_renamed_directory() {
        let rename = |step: u64, from: &str, to: &str| {
            event(
                step,
                JOB,
                EventKind::Rename {
                    from: from.into(),
                    to: to.into(),
                },
            )
        };
        let trace = job(vec![
            open(100, JOB, "/out/include/a.h", CREATE),
            open(110, JOB, "/out/lib/libx.so", CREATE),
            open(120, JOB, "/out/include/sub/b.h", CREATE),
            open(130, JOB, "/out/includes.txt", CREATE),
            open(140, JOB, "/build/tmp/scratch", CREATE),
            rename(150, "/out/include", "/dev-out/include"),
            event(
                160,
                JOB,
                EventKind::Unlink {
                    path: "/build/tmp".into(),
                },
            ),
        ]);
        assert_eq!(
            written_files(&trace, &Glob::new("**")),
            vec![
                "/dev-out/include/a.h".to_string(),
                "/out/lib/libx.so".to_string(),
                "/dev-out/include/sub/b.h".to_string(),
                "/out/includes.txt".to_string(),
            ]
        );
        assert_eq!(
            written_files(&trace, &Glob::new("/dev-out/**")),
            vec![
                "/dev-out/include/a.h".to_string(),
                "/dev-out/include/sub/b.h".to_string(),
            ]
        );
    }

    // A write outside the given directories is found; /dev/null and the
    // directories themselves are fine.
    #[test]
    fn a_write_outside_the_directories_is_found() {
        let trace = job(vec![
            open(100, JOB, "/dev/null", CREATE),
            open(110, JOB, "/build/out", CREATE),
            open(120, JOB, "/tmp/test-1/state", CREATE),
            open(130, JOB, "/root/.cache/x", CREATE),
        ]);
        let dirs = vec!["/build".to_string(), "/tmp/test-1".to_string()];
        let found = writes_within(&trace, &dirs).unwrap();
        assert_eq!(found.step, 130);
        assert_eq!(
            found.why,
            "process 40 (sh) opened /root/.cache/x for writing, outside /build, /tmp/test-1"
        );
        assert_eq!(writes_within(&trace, &["/".to_string()]), None);
    }
}
