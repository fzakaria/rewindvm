//! What the guest is asked to run.
//!
//! The monitor appends a small cpio archive to the initramfs holding
//! `/rewind/job.json`, and the guest's init reads it back. This crate is
//! both sides of that: the host builds a [`Job`] and serializes it, and the
//! `rewind-init` binary deserializes and runs it.

use serde::{Deserialize, Serialize};

/// Where the job file lands in the guest.
pub const JOB_PATH: &str = "/rewind/job.json";

/// The mark init writes to /dev/rewind when the job's main process exits,
/// followed by its wait status in decimal.
pub const EXIT_MARK: &str = "rewind-exit ";

/// The mark init writes right before it starts the job, so everything
/// before it is boot and setup, the same for every job.
pub const START_MARK: &str = "rewind-start";

/// The mark init writes for each output after a successful job: the path,
/// a space, and the output's tree hash in hex.
pub const OUTPUT_MARK: &str = "rewind-output ";

/// The argument the kernel starts this binary with when Rewind asks what a
/// forked run looks like inside at some step, followed by the request.
/// The first request is `cat <pid> <path>`: the file's bytes on
/// standard output, with the path resolved in the root and working
/// directory of process `pid`, or the job's when `pid` is 0 or gone.
pub const INSPECT_ARG: &str = "--inspect";
pub const INSPECT_CAT: &str = "cat";

/// The other request: `shell <pid> <cols> <rows>`, an interactive shell in
/// the root and working directory of process `pid` (the job's when 0 or
/// gone), with the job's environment, on a pty of that size. Its terminal
/// is /dev/rewind-console: what the shell prints arrives as output on
/// [`CONSOLE_FD`], and what Rewind sends is typed into it.
pub const INSPECT_SHELL: &str = "shell";

/// After the shell's size: the extras slot holds more Nix packages, whose
/// store paths the shell sees, and the bin directories that follow go
/// first on its PATH (`rewind shell --with`).
pub const INSPECT_WITH: &str = "--with";

/// The third request: `running`, the process that was running at the
/// step, which the kernel names in [`RUNNING_ENV`]. The answer is in
/// [sections](section_header): the pid as [`SECTION_PID`], its
/// /proc/<pid>/maps as [`SECTION_MAPS`], and then each ELF file it had
/// mapped that did not come from the input image's store, named by its
/// path as init sees it. Those are the files only the VM has, such as a
/// program the job compiled. Not found when the kernel or the idle task
/// was running.
pub const INSPECT_RUNNING: &str = "running";
pub const SECTION_PID: &str = "pid";
pub const SECTION_MAPS: &str = "maps";

/// The fourth request: `files <pid>`, files' bytes in sections named by
/// their paths, resolved as process `pid` sees them (the job's view when
/// 0 or gone). A request has room for few arguments, so the paths come as
/// input on the console instead, one per line, ending with an empty line.
/// Files that are missing, are not regular files or are too large are
/// left out.
pub const INSPECT_FILES: &str = "files";

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

/// The marks around an inspection's answer: everything its process writes
/// between them is the answer, and the end mark carries an
/// [`InspectStatus`] code in decimal.
pub const INSPECT_BEGIN_MARK: &str = "rewind-inspect-begin";
pub const INSPECT_END_MARK: &str = "rewind-inspect-end ";

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
    #[serde(default)]
    pub files: Vec<JobFile>,
    /// Paths the job produces. After it succeeds, init hashes each with
    /// [`tree_hash`] and reports the hash as a mark, so two runs can be
    /// compared by what they built.
    #[serde(default)]
    pub outputs: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobFile {
    pub path: String,
    pub contents: String,
}

/// A hash of a file tree: file contents, the executable bit, symlink
/// targets and names, and nothing else, so it is equal wherever and
/// whenever the same tree was built. Directory entries are taken in byte
/// order of their names.
pub fn tree_hash(path: &std::path::Path) -> std::io::Result<blake3::Hash> {
    use std::os::unix::fs::PermissionsExt;

    let meta = std::fs::symlink_metadata(path)?;
    let mut h = blake3::Hasher::new();
    if meta.file_type().is_symlink() {
        h.update(b"l");
        h.update(std::fs::read_link(path)?.as_os_str().as_encoded_bytes());
    } else if meta.is_dir() {
        h.update(b"d");
        let mut names: Vec<_> = std::fs::read_dir(path)?
            .map(|e| e.map(|e| e.file_name()))
            .collect::<Result<_, _>>()?;
        names.sort();
        for name in names {
            h.update(name.as_encoded_bytes());
            h.update(&[0]);
            h.update(tree_hash(&path.join(&name))?.as_bytes());
        }
    } else {
        let exec = meta.permissions().mode() & 0o111 != 0;
        h.update(if exec { b"x" } else { b"f" });
        h.update_reader(std::fs::File::open(path)?)?;
    }
    Ok(h.finalize())
}

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
    fn an_answer_cut_short_has_no_sections() {
        let mut answer = section_header("maps", 10).into_bytes();
        answer.extend(b"short");
        assert_eq!(sections(&answer), None);
    }
}
