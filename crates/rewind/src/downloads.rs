//! What gdb's debuginfod client is downloading, said as each download
//! starts.
//!
//! The first lookup on a machine can wait a minute while the session's
//! debuginfod server fetches a library's DWARF or its sources from a
//! binary cache. gdb says "Downloading ..." only once the server answers,
//! after that wait, so rewind reads the client's own log instead: with
//! `DEBUGINFOD_VERBOSE` set, libdebuginfod writes each URL it asks to
//! standard error before it waits, whatever gdb is capturing at the time.
//! A file already in the client's cache is never asked for, so nothing is
//! said for it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// The variable that has libdebuginfod log its queries on standard error,
/// and the value that turns the log on.
pub const VERBOSE_ENV: &str = "DEBUGINFOD_VERBOSE";
pub const VERBOSE_ON: &str = "1";

/// Programs' and libraries' file names by their build IDs.
pub type FileNames = BTreeMap<String, String>;

/// How the client logs a URL it asks: "url 0 http://host/buildid/<id>/...".
/// A failed query is logged the same way with curl's error for the URL.
const URL_LINE: &str = "url ";

/// The part of a debuginfod URL before the build ID.
const BUILD_ID_PATH: &str = "/buildid/";

/// What a debuginfod URL asks for after the build ID.
const DEBUGINFO: &str = "debuginfo";
const SOURCE: &str = "source";

/// How the lines of the client's log begin. Its queries print these; the
/// rest are its errors.
const CLIENT_LOG_PREFIXES: &[&str] = &[
    "debuginfod_find_",
    "server urls ",
    "checking ",
    "suffix ",
    "using ",
    "init server ",
    URL_LINE,
    "query ",
    "header ",
    "committed to url ",
    "server response ",
    "got file from server",
    "found ",
    "not found ",
    "saved ",
    "duplicate url: ",
    "Retry failed query",
    "Timeout with max time",
    "Content-Length too large",
];

/// What a query asks the server for: a program's DWARF, or one of its
/// source files by the path its DWARF names.
enum Asked<'l> {
    DebugInfo,
    Source(&'l str),
}

/// What is said once for each build ID: its DWARF, and its sources however
/// many of its source files are asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Said {
    DebugInfo,
    Sources,
}

/// Says the downloads in the client's log, each program's DWARF and its
/// sources once.
pub struct Downloads<'a> {
    names: &'a FileNames,
    said: BTreeSet<(Said, String)>,
}

impl<'a> Downloads<'a> {
    pub fn new(names: &'a FileNames) -> Downloads<'a> {
        Downloads {
            names,
            said: BTreeSet::new(),
        }
    }

    /// What to say for `line` of the client's log, such as "downloading
    /// debug info for libc.so.6; first time only", when the line starts a
    /// download not yet said. None for any other line.
    pub fn message(&mut self, line: &str) -> Option<String> {
        // The URL asked: the build ID, then what is wanted for it.
        let (index, url) = line.strip_prefix(URL_LINE)?.split_once(' ')?;
        index.parse::<usize>().ok()?;
        let (_, after) = url.split_once(BUILD_ID_PATH)?;
        let (build_id, asked) = after.split_once('/')?;
        let asked = if asked == DEBUGINFO {
            Asked::DebugInfo
        } else {
            Asked::Source(asked.strip_prefix(SOURCE)?)
        };

        // Each build ID's DWARF and sources once: a query asks every
        // server, and the first source file asked takes the longest, while
        // the server fetches them all.
        let said = match asked {
            Asked::DebugInfo => Said::DebugInfo,
            Asked::Source(_) => Said::Sources,
        };
        if !self.said.insert((said, build_id.to_string())) {
            return None;
        }

        // The program's name, else what the query names.
        let what = match (asked, self.names.get(build_id)) {
            (Asked::DebugInfo, Some(name)) => format!("debug info for {name}"),
            (Asked::DebugInfo, None) => format!("debug info for build ID {build_id}"),
            (Asked::Source(_), Some(name)) => format!("the sources of {name}"),
            (Asked::Source(path), None) => {
                let file = Path::new(path).file_name()?.to_string_lossy();
                format!("the sources of {file}")
            }
        };
        Some(format!("downloading {what}; first time only"))
    }
}

/// Whether `line` on gdb's standard error is the client's log rather than
/// gdb's own.
pub fn is_client_log(line: &str) -> bool {
    line.is_empty() || CLIENT_LOG_PREFIXES.iter().any(|p| line.starts_with(p))
}

#[cfg(test)]
mod tests {
    // The debuginfod client's log as gdb's standard error carries it,
    // turned into rewind's lines about downloads, and the log's lines told
    // apart from gdb's own.
    use super::*;

    const LIBC_ID: &str = "2486ec7a27148b622f7c6ddd177e988307e67252";
    const SERVER: &str = "http://127.0.0.1:41998";

    /// The client's log of one download of libc's DWARF and two of its
    /// source files, as `DEBUGINFOD_VERBOSE` makes it print them.
    fn log() -> Vec<String> {
        [
            format!("debuginfod_find_debuginfo {LIBC_ID}"),
            format!("server urls \"{SERVER}\""),
            "checking build-id".into(),
            "checking cache dir /home/u/.cache/debuginfod_client".into(),
            "using timeout 90".into(),
            format!("init server 0 {SERVER}/buildid"),
            format!("url 0 {SERVER}/buildid/{LIBC_ID}/debuginfo"),
            "query 1 urls in parallel".into(),
            "".into(),
            "header HTTP/1.1 200 OK".into(),
            "header content-length: 4557664".into(),
            "committed to url 0".into(),
            "server response No error".into(),
            "got file from server".into(),
            format!("found /home/u/.cache/debuginfod_client/{LIBC_ID}/debuginfo (fd=13)"),
            format!("debuginfod_find_source {LIBC_ID} /build/glibc-2.44/io/write.c"),
            format!("url 0 {SERVER}/buildid/{LIBC_ID}/source/build/glibc-2.44/io/write.c"),
            format!("url 0 {SERVER}/buildid/{LIBC_ID}/source/build/glibc-2.44/libio/fileops.c"),
        ]
        .into()
    }

    /// Feeds the log through `Downloads` with libc's build ID named, and
    /// checks one line is said for its DWARF and one for its sources,
    /// however many source files follow.
    #[test]
    fn each_download_is_said_once_by_the_file_s_name() {
        let names = FileNames::from([(LIBC_ID.to_string(), "libc.so.6".to_string())]);
        let mut downloads = Downloads::new(&names);
        let said: Vec<String> = log().iter().filter_map(|l| downloads.message(l)).collect();
        assert_eq!(
            said,
            vec![
                "downloading debug info for libc.so.6; first time only".to_string(),
                "downloading the sources of libc.so.6; first time only".to_string(),
            ]
        );
    }

    /// Without a name for the build ID, the DWARF is named by the build
    /// ID, and the sources by the first source file asked for.
    #[test]
    fn a_file_with_no_name_is_said_by_its_build_id_or_source() {
        let names = FileNames::new();
        let mut downloads = Downloads::new(&names);
        let said: Vec<String> = log().iter().filter_map(|l| downloads.message(l)).collect();
        assert_eq!(
            said,
            vec![
                format!("downloading debug info for build ID {LIBC_ID}; first time only"),
                "downloading the sources of write.c; first time only".to_string(),
            ]
        );
    }

    /// A query that failed is logged as "url N" with curl's error, which is
    /// no download; neither are gdb's own lines.
    #[test]
    fn errors_and_gdb_s_lines_are_no_downloads() {
        let names = FileNames::new();
        let mut downloads = Downloads::new(&names);
        assert_eq!(downloads.message("url 0 Couldn't connect to server"), None);
        assert_eq!(
            downloads.message("warning: No executable has been specified"),
            None
        );
    }

    /// Every line of the log is the client's, and gdb's warnings and
    /// errors are not.
    #[test]
    fn the_client_s_log_is_told_from_gdb_s_lines() {
        for line in log() {
            assert!(is_client_log(&line), "{line:?}");
        }
        assert!(!is_client_log(
            "warning: No executable has been specified and target does not support"
        ));
        assert!(!is_client_log("Remote connection closed"));
    }
}
