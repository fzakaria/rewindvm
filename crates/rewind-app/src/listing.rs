//! A directory's listing in the file viewer: the entries the VM has
//! under the directory, each with the step the run last wrote it at.
//!
//! Two sources make a listing. The trace has the files the run wrote,
//! with the step of each one's last write, at once and in every run. The
//! engine's `rewind files` has everything under the directory as the VM
//! held it at the step, links, empty directories and the input image's
//! files included, with sizes, after the seconds a look inside a run
//! takes. The viewer shows the trace's files first and the two merged
//! once the engine answers.

use std::collections::{HashMap, HashSet};

use rewind_trace::{ListedAll, ListedKind, Listing};

use crate::model::WrittenFile;

/// What separates the names of a path.
const PATH_SEPARATOR: char = '/';

/// What a link's target follows in a row's text.
const POINTS_TO: &str = " -> ";

/// One row of a directory's listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The entry's whole path.
    pub path: String,
    pub kind: ListedKind,
    /// A file's length in bytes, when the VM listed the directory.
    pub size: Option<u64>,
    /// Where a symbolic link points.
    pub target: Option<String>,
    /// The step the run last opened the file for writing at, or renamed a
    /// file to this path at, when the trace has such a step.
    pub written: Option<u64>,
    /// The process whose view of paths the engine reads the entry with.
    pub pid: u32,
}

impl Entry {
    /// The row's text in the listing of `dir`: the path below the
    /// directory, a slash after a directory's, and a link's target.
    pub fn shown(&self, dir: &str) -> String {
        let name = name_under(dir, &self.path);
        match (self.kind, &self.target) {
            (ListedKind::Directory, _) => format!("{name}{PATH_SEPARATOR}"),
            (_, Some(target)) => format!("{name}{POINTS_TO}{target}"),
            (_, None) => name.to_string(),
        }
    }
}

/// Where a listing's entries come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The trace alone: the files the run wrote.
    Trace,
    /// The VM, at the step, with the trace's steps; whether the VM listed
    /// every entry or cut the listing.
    Vm(ListedAll),
}

/// The listing the trace alone gives: the files the run wrote.
pub fn from_trace(written: Vec<WrittenFile>) -> Vec<Entry> {
    written
        .into_iter()
        .map(|file| Entry {
            path: file.path,
            kind: ListedKind::File,
            size: None,
            target: None,
            written: Some(file.step),
            pid: file.pid,
        })
        .collect()
}

/// The VM's `listing` of `dir` with the trace's steps, by path: every
/// file, link and empty directory the VM has, each file the run wrote
/// with the step and process in `written`, and anything else read as
/// process `pid` sees paths. A directory with entries shows through
/// them. A listing that was cut also has the written files the cut left
/// out.
pub fn merged(dir: &str, listing: &Listing, written: &[WrittenFile], pid: u32) -> Vec<Entry> {
    let dir = dir.trim_end_matches(PATH_SEPARATOR);
    let steps: HashMap<&str, &WrittenFile> = written
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect();

    // The directories that hold a listed entry.
    let mut holding: HashSet<&str> = HashSet::new();
    for listed in &listing.entries {
        let mut rest = listed.path.as_str();
        while let Some((parent, _)) = rest.rsplit_once(PATH_SEPARATOR) {
            if !holding.insert(parent) {
                break;
            }
            rest = parent;
        }
    }

    let mut entries: Vec<Entry> = listing
        .entries
        .iter()
        .filter(|listed| {
            listed.kind != ListedKind::Directory || !holding.contains(listed.path.as_str())
        })
        .map(|listed| {
            let path = format!("{dir}{PATH_SEPARATOR}{}", listed.path);
            let wrote = steps.get(path.as_str());
            Entry {
                kind: listed.kind,
                size: (listed.kind == ListedKind::File).then_some(listed.size),
                target: listed.target.clone(),
                written: wrote.map(|file| file.step),
                pid: wrote.map_or(pid, |file| file.pid),
                path,
            }
        })
        .collect();

    // What a cut listing left out, as far as the trace knows.
    if listing.complete == ListedAll::No {
        let listed: HashSet<String> = entries.iter().map(|e| e.path.clone()).collect();
        let left_out = written.iter().filter(|file| !listed.contains(&file.path));
        entries.extend(from_trace(left_out.cloned().collect()));
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    entries
}

/// `path`, an entry below directory `dir`, as the listing of `dir` names
/// the entry: by the names under the directory.
pub fn name_under<'a>(dir: &str, path: &'a str) -> &'a str {
    let dir = dir.trim_end_matches(PATH_SEPARATOR);
    path.strip_prefix(dir)
        .map_or(path, |rest| rest.trim_start_matches(PATH_SEPARATOR))
}

/// The directory that holds `path`, or None for the root and for a
/// relative path of one name, which name no directory above them.
pub fn parent_of(path: &str) -> Option<&str> {
    /// The root directory.
    const ROOT: &str = "/";

    let path = path.trim_end_matches(PATH_SEPARATOR);
    let (parent, _) = path.rsplit_once(PATH_SEPARATOR)?;
    if parent.is_empty() {
        return Some(ROOT);
    }
    Some(parent)
}

#[cfg(test)]
mod tests {
    // Listings made from a VM's answer and a trace's written files, both
    // hand-built, with no engine and no window.
    use super::*;
    use rewind_trace::Listed;

    /// The process the listing was opened with, and the one that wrote.
    const VIEWER: u32 = 1;
    const WRITER: u32 = 40;

    fn listed(path: &str, kind: ListedKind, size: u64, target: Option<&str>) -> Listed {
        Listed {
            path: path.into(),
            kind,
            mode: 0o755,
            size,
            target: target.map(str::to_string),
        }
    }

    fn wrote(path: &str, step: u64) -> WrittenFile {
        WrittenFile {
            path: path.into(),
            step,
            pid: WRITER,
        }
    }

    #[test]
    fn the_vm_listing_takes_the_trace_s_steps() {
        // The VM has a program the run wrote, a link to it, an empty
        // directory and a file of the image; the trace has the program's
        // last write. The listing has all four by path: the program with
        // its size, its step and its writer, the rest with no step and
        // the viewer's process, and neither directory that holds an entry.
        let listing = Listing {
            entries: vec![
                listed("bin", ListedKind::Directory, 0, None),
                listed("empty", ListedKind::Directory, 0, None),
                listed("share", ListedKind::Directory, 0, None),
                listed("bin/sh", ListedKind::Symlink, 0, Some("shuffle")),
                listed("bin/shuffle", ListedKind::File, 16_424, None),
                listed("share/doc", ListedKind::Directory, 0, None),
                listed("share/doc/README", ListedKind::File, 12, None),
            ],
            complete: ListedAll::Yes,
        };
        let written = [wrote("/out/bin/shuffle", 66_465)];
        let entries = merged("/out/", &listing, &written, VIEWER);
        let seen: Vec<(String, Option<u64>, Option<u64>, u32)> = entries
            .iter()
            .map(|e| (e.shown("/out"), e.size, e.written, e.pid))
            .collect();
        assert_eq!(
            seen,
            vec![
                ("bin/sh -> shuffle".to_string(), None, None, VIEWER),
                (
                    "bin/shuffle".to_string(),
                    Some(16_424),
                    Some(66_465),
                    WRITER
                ),
                ("empty/".to_string(), None, None, VIEWER),
                ("share/doc/README".to_string(), Some(12), None, VIEWER),
            ]
        );
        assert_eq!(entries[1].path, "/out/bin/shuffle");
    }

    #[test]
    fn a_cut_listing_keeps_the_written_files_it_left_out() {
        // The VM cut its listing after one file, and the trace has that
        // file and one more: the listing has both. A whole listing
        // without the second file leaves it out, as the VM does not have
        // the file.
        let mut listing = Listing {
            entries: vec![listed("a.o", ListedKind::File, 8, None)],
            complete: ListedAll::No,
        };
        let written = [wrote("/build/a.o", 10), wrote("/build/z.o", 20)];
        let paths = |listing: &Listing| -> Vec<String> {
            merged("/build", listing, &written, VIEWER)
                .into_iter()
                .map(|e| e.path)
                .collect()
        };
        assert_eq!(paths(&listing), vec!["/build/a.o", "/build/z.o"]);
        listing.complete = ListedAll::Yes;
        assert_eq!(paths(&listing), vec!["/build/a.o"]);
    }

    #[test]
    fn a_path_s_parent_is_the_directory_that_holds_it() {
        // A file, a directory named with a trailing slash, and a path one
        // name below the root: each has the directory above it. The root
        // has none, and neither does a relative path of one name.
        assert_eq!(parent_of("/build/src/main.c"), Some("/build/src"));
        assert_eq!(parent_of("/nix/store/abc-out/"), Some("/nix/store"));
        assert_eq!(parent_of("/build"), Some("/"));
        assert_eq!(parent_of("/"), None);
        assert_eq!(parent_of("src/main.c"), Some("src"));
        assert_eq!(parent_of("main.c"), None);
    }

    #[test]
    fn the_trace_alone_lists_the_files_the_run_wrote() {
        // Two written files: each is a file with its step and writer and
        // no size, shown by its path below the directory. A path outside
        // the directory is shown whole.
        let entries = from_trace(vec![wrote("/out/bin/shuffle", 5), wrote("/out/README", 9)]);
        let seen: Vec<(String, Option<u64>, Option<u64>)> = entries
            .iter()
            .map(|e| (e.shown("/out"), e.size, e.written))
            .collect();
        assert_eq!(
            seen,
            vec![
                ("bin/shuffle".to_string(), None, Some(5)),
                ("README".to_string(), None, Some(9)),
            ]
        );
        assert_eq!(name_under("/out", "/build/main.o"), "/build/main.o");
    }
}
