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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    /// The program and its arguments. A program without a slash is looked
    /// up on the PATH in `env`.
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
