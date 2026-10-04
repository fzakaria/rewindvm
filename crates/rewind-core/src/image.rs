//! Input images: the read-only filesystem the guest mounts from
//! persistent memory.
//!
//! Images are erofs, built by mkfs.erofs with everything that could vary
//! between two builds of the same files pinned: owners, timestamps, the
//! filesystem UUID, and the order of entries. The same files therefore
//! always make the same image, and the image's hash names the input. That
//! includes the environment: mkfs.erofs and tar take SOURCE_DATE_EPOCH
//! over the timestamp they are given, and a Nix development shell sets it,
//! so the commands that build images run without it.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};

/// A fixed UUID, so the superblock does not change between builds.
const UUID: &str = "00000000-0000-0000-0000-000000000000";

/// The variable mkfs.erofs and tar read a timestamp from ahead of their
/// flags, which `nix develop` sets to the start of 1980.
const SOURCE_DATE_EPOCH: &str = "SOURCE_DATE_EPOCH";

/// The timestamp every file in an image carries.
const MTIME: &str = "1";

/// The mkfs.erofs flags that make an image a function of its files.
fn mkfs_erofs() -> Command {
    let mut cmd = Command::new("mkfs.erofs");
    cmd.args(["--quiet", "--all-root", "--ignore-mtime", "-x-1"])
        .arg(format!("-T{MTIME}"))
        .arg(format!("-U{UUID}"))
        .env_remove(SOURCE_DATE_EPOCH);
    cmd
}

/// tar, for packing store paths into an image's input, with nothing from
/// the environment to change what it writes.
fn tar_command() -> Command {
    let mut cmd = Command::new("tar");
    cmd.env_remove(SOURCE_DATE_EPOCH);
    cmd
}

/// An image of a directory tree, which becomes the job's root.
pub fn from_dir(dir: &Path, out: &Path) -> Result<()> {
    if !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    let status = mkfs_erofs()
        .arg(out)
        .arg(dir)
        .status()
        .context("running mkfs.erofs; is erofs-utils on PATH?")?;
    if !status.success() {
        bail!("mkfs.erofs failed on {}", dir.display());
    }
    Ok(())
}

/// An image of a tarball, for example `docker export` output.
pub fn from_tar(tar: &Path, out: &Path) -> Result<()> {
    let status = mkfs_erofs()
        .arg("--tar=f")
        .arg(out)
        .arg(tar)
        .status()
        .context("running mkfs.erofs; is erofs-utils on PATH?")?;
    if !status.success() {
        bail!("mkfs.erofs failed on {}", tar.display());
    }
    Ok(())
}

/// An image of store paths, laid out as they appear under /nix/store: the
/// guest mounts it there.
pub fn from_store_paths(paths: &[PathBuf], out: &Path) -> Result<()> {
    let mut names: Vec<String> = paths
        .iter()
        .map(|p| {
            p.strip_prefix("/nix/store")
                .with_context(|| format!("{} is not a store path", p.display()))
                .map(|n| n.to_string_lossy().into_owned())
        })
        .collect::<Result<_>>()?;
    names.sort();
    names.dedup();

    // tar reads the names from a file, since a closure can hold thousands
    // of them.
    let list = out.with_extension("list");
    fs::write(&list, names.join("\n") + "\n")?;
    let tar = out.with_extension("tar");

    let status = tar_command()
        .args(["--create", "--file"])
        .arg(&tar)
        .args([
            "--directory",
            "/nix/store",
            "--sort=name",
            "--owner=0",
            "--group=0",
            "--numeric-owner",
            "--mtime=@1",
            "--format=gnu",
            "--files-from",
        ])
        .arg(&list)
        .stdout(Stdio::null())
        .status()
        .context("running tar")?;
    if !status.success() {
        bail!("tar failed on the store paths");
    }
    let built = from_tar(&tar, out);
    let _ = fs::remove_file(&tar);
    let _ = fs::remove_file(&list);
    built
}

/// The BLAKE3 hash of a file, in hex.
pub fn hash_file(path: &Path) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    hasher
        .update_mmap(path)
        .with_context(|| format!("hashing {}", path.display()))?;
    Ok(hasher.finalize().to_hex().to_string())
}

/// A name beside `path` to write it under before renaming it into place,
/// unique to this process and call. Two writers of the same file, such as
/// two runs packing the same image, each get their own, and the last
/// rename wins.
pub fn temp_beside(path: &Path) -> PathBuf {
    static WRITES: AtomicU64 = AtomicU64::new(0);
    let n = WRITES.fetch_add(1, Ordering::Relaxed);
    path.with_extension(format!("tmp-{}-{n}", std::process::id()))
}

/// Writes `bytes` to `path` atomically, so a crash never leaves a half
/// written image where a whole one is expected. Writers of the same path in
/// other processes or threads each get their own temporary file, and the
/// last rename wins.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = temp_beside(path);
    let mut f = fs::File::create(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    // Temporary names beside a file: two writers of the same file, in one
    // process or two, never share one.
    use super::*;

    /// mkfs.erofs and tar read SOURCE_DATE_EPOCH ahead of the timestamps
    /// they are given, and a Nix development shell sets it, so the
    /// commands that build images take it out of their environment.
    /// Checks each command's environment rather than setting the variable
    /// here, which other tests would see.
    #[test]
    fn image_builders_ignore_source_date_epoch() {
        for cmd in [mkfs_erofs(), tar_command()] {
            let removed = cmd
                .get_envs()
                .any(|(name, value)| name == SOURCE_DATE_EPOCH && value.is_none());
            assert!(removed, "{:?} keeps SOURCE_DATE_EPOCH", cmd.get_program());
        }
    }

    #[test]
    fn each_temporary_name_is_its_own() {
        let image = Path::new("/home/images/store-abc.erofs");
        let first = temp_beside(image);
        let second = temp_beside(image);
        assert_ne!(first, second);
        assert_ne!(first, image);
        assert_eq!(first.parent(), image.parent());
        let name = first.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.contains(&std::process::id().to_string()), "{name}");
    }
}
