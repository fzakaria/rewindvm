//! What the guest is asked to run.
//!
//! The monitor appends a small cpio archive to the initramfs holding
//! `/rewind/job.json`, and the guest's init reads it back. This crate is
//! both sides of that: the host builds a [`Job`] and serializes it, and the
//! `rewind-init` binary deserializes and runs it.

use serde::{Deserialize, Serialize};

/// Where the job file lands in the guest.
pub const JOB_PATH: &str = "/rewind/job.json";

/// Where a job with a root filesystem has its root, as init sees it: init
/// chroots the job into it, so the paths init reports for the job's files,
/// such as those in its memory map, start with it.
pub const IMAGE_ROOT: &str = "/newroot";

/// Init's process id. Any process can write to /dev/rewind, so only marks
/// from this pid are init's: a job that writes `rewind-exit 0` there
/// reports nothing.
pub const INIT_PID: u32 = 1;

/// A mark init writes to /dev/rewind, one write each: around the job, and
/// around an inspection's answer. The kernel stops every other process
/// while an inspection runs, so the inspection's marks come from the
/// inspecting process alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mark {
    /// Right before the job starts, so everything before is boot and
    /// setup, the same for every job.
    Start,
    /// The job's main process exited, with this wait status.
    Exit { status: i32 },
    /// After a successful job, for each output: its path, and its tree
    /// hash as [`nar_hash`] gives it.
    Output { path: String, hash: String },
    /// The inspecting process is about to answer: everything it writes
    /// until [`Mark::InspectEnd`] is the answer.
    InspectBegin,
    /// The inspection is over, and how it ended.
    InspectEnd(InspectStatus),
}

/// How each mark starts. A mark with more to say has it after a space.
const START_MARK: &str = "rewind-start";
const EXIT_MARK: &str = "rewind-exit";
const OUTPUT_MARK: &str = "rewind-output";
const INSPECT_BEGIN_MARK: &str = "rewind-inspect-begin";
const INSPECT_END_MARK: &str = "rewind-inspect-end";

impl Mark {
    /// The mark a write to /dev/rewind holds, if it is one of these.
    pub fn parse(text: &str) -> Option<Mark> {
        let (word, rest) = match text.split_once(' ') {
            Some((word, rest)) => (word, Some(rest)),
            None => (text, None),
        };
        match (word, rest) {
            (START_MARK, None) => Some(Mark::Start),
            (EXIT_MARK, Some(status)) => Some(Mark::Exit {
                status: status.parse().ok()?,
            }),
            (OUTPUT_MARK, Some(rest)) => {
                let (path, hash) = rest.rsplit_once(' ')?;
                Some(Mark::Output {
                    path: path.to_string(),
                    hash: hash.to_string(),
                })
            }
            (INSPECT_BEGIN_MARK, None) => Some(Mark::InspectBegin),
            (INSPECT_END_MARK, Some(code)) => Some(Mark::InspectEnd(InspectStatus::from_code(
                code.parse().ok()?,
            ))),
            _ => None,
        }
    }
}

impl std::fmt::Display for Mark {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Mark::Start => write!(f, "{START_MARK}"),
            Mark::Exit { status } => write!(f, "{EXIT_MARK} {status}"),
            Mark::Output { path, hash } => write!(f, "{OUTPUT_MARK} {path} {hash}"),
            Mark::InspectBegin => write!(f, "{INSPECT_BEGIN_MARK}"),
            Mark::InspectEnd(status) => write!(f, "{INSPECT_END_MARK} {}", status.code()),
        }
    }
}

/// The argument the kernel starts this binary with when Rewind asks what a
/// forked run looks like inside at some step, followed by the request's
/// [`InspectRequest::args`].
pub const INSPECT_ARG: &str = "--inspect";

/// What Rewind asks a forked run at some step. A `pid` of None means the
/// job's view for every request but `Running`; a process that is gone by
/// the step means the job's view too.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InspectRequest {
    /// The file's bytes on standard output, with the path resolved in the
    /// root and working directory of process `pid`.
    Cat { pid: Option<u32>, path: String },
    /// The process that was running at the step, which the kernel names
    /// in [`RUNNING_ENV`], or process `pid` when given. The answer is in
    /// [sections](section_header): the pid as [`SECTION_PID`], its
    /// /proc/<pid>/maps as [`SECTION_MAPS`], and then each ELF file it had
    /// mapped that did not come from the input image's store, named by its
    /// path as init sees it. Those are the files only the VM has, such as a
    /// program the job compiled. Not found when the kernel or the idle task
    /// was running, or when there is no process `pid`.
    Running { pid: Option<u32> },
    /// Files' bytes in sections named by their paths, resolved as process
    /// `pid` sees them. A request has room for few arguments, so the paths
    /// come as input on the console instead, one per line, ending with an
    /// empty line. Files that are missing, are not regular files or are
    /// too large are left out.
    Files { pid: Option<u32> },
    /// An interactive shell in the root and working directory of process
    /// `pid`, with the job's environment, on a pty of `size` (columns,
    /// rows). Its terminal is /dev/rewind-console: what the shell prints
    /// arrives as output on [`CONSOLE_FD`], and what Rewind sends is typed
    /// into it. With `with`, the extras slot holds more Nix packages, whose
    /// store paths the shell sees, and those bin directories go first on
    /// its PATH (`rewind shell --with`).
    Shell {
        pid: Option<u32>,
        size: (u16, u16),
        with: Option<Vec<String>>,
    },
}

/// The words a request's arguments start with.
const INSPECT_CAT: &str = "cat";
const INSPECT_RUNNING: &str = "running";
const INSPECT_FILES: &str = "files";
const INSPECT_SHELL: &str = "shell";

/// After a shell's size: the bin directories of the extras slot follow.
const INSPECT_WITH: &str = "--with";

/// How a request names the job's view in place of a pid.
const JOBS_VIEW: u32 = 0;

impl InspectRequest {
    /// The request as the arguments the kernel passes on to init.
    pub fn args(&self) -> Vec<String> {
        let pid = |pid: &Option<u32>| pid.unwrap_or(JOBS_VIEW).to_string();
        match self {
            InspectRequest::Cat { pid: p, path } => {
                vec![INSPECT_CAT.into(), pid(p), path.clone()]
            }
            InspectRequest::Running { pid: None } => vec![INSPECT_RUNNING.into()],
            InspectRequest::Running { pid: Some(p) } => {
                vec![INSPECT_RUNNING.into(), p.to_string()]
            }
            InspectRequest::Files { pid: p } => vec![INSPECT_FILES.into(), pid(p)],
            InspectRequest::Shell {
                pid: p,
                size: (cols, rows),
                with,
            } => {
                let mut args = vec![
                    INSPECT_SHELL.into(),
                    pid(p),
                    cols.to_string(),
                    rows.to_string(),
                ];
                if let Some(bins) = with {
                    args.push(INSPECT_WITH.into());
                    args.extend(bins.iter().cloned());
                }
                args
            }
        }
    }

    /// The request `args` make, as init reads them after [`INSPECT_ARG`].
    pub fn parse(args: &[String]) -> Option<InspectRequest> {
        let pid = |arg: &str| -> Option<Option<u32>> {
            let pid: u32 = arg.parse().ok()?;
            Some((pid != JOBS_VIEW).then_some(pid))
        };
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        match args.as_slice() {
            [INSPECT_CAT, p, path] => Some(InspectRequest::Cat {
                pid: pid(p)?,
                path: path.to_string(),
            }),
            [INSPECT_RUNNING] => Some(InspectRequest::Running { pid: None }),
            [INSPECT_RUNNING, p] => Some(InspectRequest::Running {
                pid: Some(p.parse().ok()?),
            }),
            [INSPECT_FILES, p] => Some(InspectRequest::Files { pid: pid(p)? }),
            [INSPECT_SHELL, p, cols, rows, extras @ ..] => {
                let with = match extras {
                    [] => None,
                    [INSPECT_WITH, bins @ ..] => Some(bins.iter().map(|b| b.to_string()).collect()),
                    _ => return None,
                };
                Some(InspectRequest::Shell {
                    pid: pid(p)?,
                    size: (cols.parse().ok()?, rows.parse().ok()?),
                    with,
                })
            }
            _ => None,
        }
    }
}

/// The names of a running answer's first two sections.
pub const SECTION_PID: &str = "pid";
pub const SECTION_MAPS: &str = "maps";

/// The variable the kernel starts every inspection with: the thread group
/// id of the process that was running at the step, or 0.
pub const RUNNING_ENV: &str = "REWIND_RUNNING";

/// The output stream /dev/rewind-console's writes are reported on.
pub const CONSOLE_FD: u32 = 3;

/// Console input that resizes the shell's terminal: this byte, which
/// never occurs in UTF-8, then [`RESIZE_TAG`], then the columns and rows
/// as little-endian u16s. Everything else is typed as is.
pub const RESIZE_ESCAPE: u8 = 0xff;
pub const RESIZE_TAG: u8 = b'W';
pub const RESIZE_LEN: usize = 6;

/// The bytes that resize the shell's terminal.
pub fn resize_message(cols: u16, rows: u16) -> [u8; RESIZE_LEN] {
    let [c0, c1] = cols.to_le_bytes();
    let [r0, r1] = rows.to_le_bytes();
    [RESIZE_ESCAPE, RESIZE_TAG, c0, c1, r0, r1]
}

/// How an inspection ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InspectStatus {
    Done,
    Failed,
    NotFound,
}

impl InspectStatus {
    pub fn code(self) -> u32 {
        match self {
            InspectStatus::Done => 0,
            InspectStatus::Failed => 1,
            InspectStatus::NotFound => 2,
        }
    }

    pub fn from_code(code: u32) -> InspectStatus {
        match code {
            0 => InspectStatus::Done,
            2 => InspectStatus::NotFound,
            _ => InspectStatus::Failed,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    /// The program to execute, when it is not `argv[0]`: nix-daemon
    /// executes a builder by its path and passes only its file name as
    /// `argv[0]`.
    pub program: Option<String>,
    /// The program and its arguments, or with `program` set, its name and
    /// its arguments. A program without a slash is looked up on the PATH in
    /// `env`.
    pub argv: Vec<String>,
    /// The environment, in order.
    pub env: Vec<(String, String)>,
    pub cwd: String,
    pub uid: u32,
    pub gid: u32,
    pub hostname: String,
    pub root: Root,
    /// Files init writes before the job starts, owned by the job's user.
    pub files: Vec<JobFile>,
    /// Paths the job produces. After it succeeds, init hashes each with
    /// [`nar_hash`] and reports the hash as a mark, so two runs can be
    /// compared by what they built.
    pub outputs: Vec<String>,
    /// What the job's standard output and error are.
    pub output: Output,
}

/// What a job's standard output and error are. Programs ask: one whose
/// output is a terminal writes a line at a time, and colors it; one whose
/// output is not buffers it and writes it in blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Output {
    /// A terminal in raw mode, as nix-daemon gives a builder a
    /// pseudoterminal.
    Terminal,
    /// Devices that are not a terminal, as `docker run` gives a container
    /// without -t.
    Plain,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobFile {
    pub path: String,
    pub contents: String,
}

/// The hash of a file tree serialized as a NAR, `sha256:` and the digest
/// in hex: the hash Nix records as a store path's narHash and binary caches
/// publish as NarHash. It covers names, file contents, the executable bit
/// and symlink targets, and nothing else, so it is equal wherever and
/// whenever the same tree was built.
pub fn nar_hash(path: &std::path::Path) -> std::io::Result<String> {
    let hash = nix_archive::nar::hash_path(path, nix_archive::nar::CaseHack::native())
        .map_err(std::io::Error::other)?;
    let hex: String = hash.sha256.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("{NAR_HASH_PREFIX}{hex}"))
}

/// How [`nar_hash`] names its algorithm, as Nix writes a base-16 hash.
pub const NAR_HASH_PREFIX: &str = "sha256:";

/// The header of one section of an answer that carries several things:
/// the length in decimal, a space and the name, on a line, then that many
/// bytes. The length comes first so a name may hold any character but a
/// newline.
pub fn section_header(name: &str, len: usize) -> String {
    format!("{len} {name}\n")
}

/// The sections of an answer, in order, or None when it is cut short.
pub fn sections(mut answer: &[u8]) -> Option<Vec<(String, &[u8])>> {
    let mut found = Vec::new();
    while !answer.is_empty() {
        let line_end = answer.iter().position(|b| *b == b'\n')?;
        let header = std::str::from_utf8(&answer[..line_end]).ok()?;
        let (len, name) = header.split_once(' ')?;
        let len: usize = len.parse().ok()?;
        let body = answer.get(line_end + 1..line_end + 1 + len)?;
        found.push((name.to_string(), body));
        answer = &answer[line_end + 1 + len..];
    }
    Some(found)
}

/// How the input image becomes the job's filesystem.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Root {
    /// The image holds store paths. It is mounted read-only under a
    /// writable overlay at /nix/store, and the initramfs is the root, laid
    /// out like the Nix build sandbox.
    Store,
    /// The image is a whole root filesystem. It is mounted under a writable
    /// overlay and the job runs chrooted into it.
    Image,
    /// There is no image; the job runs in the initramfs.
    Initramfs,
}

#[cfg(test)]
mod tests {
    // Sections written one after another and read back, and an answer cut
    // short in the middle of one.
    use super::*;

    #[test]
    fn an_output_is_hashed_as_nix_hashes_a_store_path() {
        // A tree with a file, an executable and a symlink, and a file on its
        // own, hash to what `nix hash path --algo sha256 --base16` printed
        // for the same tree, the hash Nix records as a path's narHash.
        let dir = std::env::temp_dir().join(format!("rewind-nar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("a"), "hello\n").unwrap();
        std::fs::write(dir.join("bin/run"), "#!/bin/sh\necho hi\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.join("bin/run"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::os::unix::fs::symlink("a", dir.join("l")).unwrap();

        assert_eq!(
            nar_hash(&dir).unwrap(),
            "sha256:3396a269de6b6355be0bcaef381dfaddf85eb8237aeff4d930f72816a67080fb"
        );
        assert_eq!(
            nar_hash(&dir.join("a")).unwrap(),
            "sha256:1c37d01af40be2e80691de3cc3df44377a699afbb17c68f080964b2fd071fc13"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sections_come_back_in_order_with_any_bytes() {
        let mut answer = Vec::new();
        for (name, body) in [("pid", &b"42"[..]), ("/build/a b", b"\n\x7fELF\n")] {
            answer.extend(section_header(name, body.len()).into_bytes());
            answer.extend(body);
        }
        assert_eq!(
            sections(&answer).unwrap(),
            vec![
                ("pid".to_string(), &b"42"[..]),
                ("/build/a b".to_string(), &b"\n\x7fELF\n"[..])
            ]
        );
    }

    #[test]
    fn marks_read_back_as_init_wrote_them() {
        // Each mark's text parses back to the mark; text that only starts
        // like one, or says too little or too much, is not one.
        let marks = [
            Mark::Start,
            Mark::Exit { status: 512 },
            Mark::Output {
                path: "/nix/store/x-mylib".into(),
                hash: "sha256:00ff".into(),
            },
            Mark::InspectBegin,
            Mark::InspectEnd(InspectStatus::NotFound),
        ];
        for mark in marks {
            assert_eq!(Mark::parse(&mark.to_string()), Some(mark));
        }
        assert_eq!(
            Mark::parse("rewind-exit 512"),
            Some(Mark::Exit { status: 512 })
        );
        for not_a_mark in [
            "rewind-exit",
            "rewind-exit x",
            "rewind-start now",
            "rewind-started",
            "build",
            "",
        ] {
            assert_eq!(Mark::parse(not_a_mark), None, "{not_a_mark:?}");
        }
    }

    #[test]
    fn requests_read_back_as_rewind_made_them() {
        // Each request's arguments parse back to the request, the job's
        // view as pid 0; malformed arguments are no request.
        let requests = [
            InspectRequest::Cat {
                pid: None,
                path: "/build/a b".into(),
            },
            InspectRequest::Cat {
                pid: Some(42),
                path: "x".into(),
            },
            InspectRequest::Running { pid: None },
            InspectRequest::Running { pid: Some(7) },
            InspectRequest::Files { pid: Some(3) },
            InspectRequest::Shell {
                pid: None,
                size: (80, 24),
                with: None,
            },
            InspectRequest::Shell {
                pid: Some(9),
                size: (120, 40),
                with: Some(vec!["/nix/store/x-gdb/bin".into()]),
            },
        ];
        for request in requests {
            assert_eq!(InspectRequest::parse(&request.args()), Some(request));
        }
        let parse = |args: &[&str]| {
            InspectRequest::parse(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>())
        };
        assert_eq!(
            parse(&["cat", "0", "/a"]).unwrap(),
            InspectRequest::Cat {
                pid: None,
                path: "/a".into()
            }
        );
        assert_eq!(parse(&["cat", "x", "/a"]), None);
        assert_eq!(parse(&["shell", "0", "80"]), None);
        assert_eq!(parse(&["shell", "0", "80", "24", "--without"]), None);
        assert_eq!(parse(&["format", "/"]), None);
    }

    #[test]
    fn an_answer_cut_short_has_no_sections() {
        let mut answer = section_header("maps", 10).into_bytes();
        answer.extend(b"short");
        assert_eq!(sections(&answer), None);
    }
}
