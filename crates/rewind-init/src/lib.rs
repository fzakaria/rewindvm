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
