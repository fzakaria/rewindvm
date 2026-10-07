//! The caches of the debuginfod servers `rewind gdb` starts, one
//! numbered directory under the home's for each session running at once.
//!
//! nixseparatedebuginfod2 keeps what it knows of its cache in memory and
//! guards the directory with locks that are its own, so two servers on one
//! directory break each other's entries. Each session's server takes a
//! directory no running server has: a lock on the file beside the
//! directory says it is taken.
//!
//! Taking a directory moves into it the entries it lacks from directories
//! no session holds, so what one session fetched serves the next whichever
//! directory each had. Each move is one rename, however many files the
//! entry holds, and nothing is deleted then, so taking a directory never
//! waits on a large tree. What is left behind, entries both directories
//! had and fetches a killed server did not finish, goes with `rewind gc`.

use std::fs::{self, File};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::home::bytes_under;
pub use crate::prune::Act;

/// The extension of the file beside each directory that the session using
/// the directory holds a lock on.
const LOCK_EXTENSION: &str = "lock";

/// In the server's layout, each kind of file it fetches has a directory
/// holding whole entries, one per key, and one holding fetches under way.
const SERVER_CACHE: &str = "cache";
const SERVER_PARTIAL: &str = "partial";

/// A cache directory held for one session's server, until this is dropped
/// and, when the server inherits the lock's descriptor, until the server
/// ends.
pub struct Slot {
    dir: PathBuf,
    lock: File,
}

impl Slot {
    /// The directory the server is to use.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The locked file, for the server to inherit.
    pub fn lock(&self) -> &File {
        &self.lock
    }
}

/// A cache directory no session holds, as `rewind gc` finds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unused {
    pub path: PathBuf,
    pub bytes: u64,
}

/// Takes the first directory under `root` that no session holds, made if
/// it is not there yet, with the entries it lacks moved in from the
/// directories no session holds. An entry that does not move stays where
/// it is.
pub fn take(root: &Path) -> Result<Slot> {
    fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
    for number in 0u32.. {
        let Some(slot) = try_hold(root, number)? else {
            continue;
        };
        fs::create_dir_all(&slot.dir)
            .with_context(|| format!("creating {}", slot.dir.display()))?;

        // The other directories no session holds give up what this one
        // lacks; one a session holds is passed over.
        for other in numbers(root) {
            if other == number {
                continue;
            }
            let Ok(Some(free)) = try_hold(root, other) else {
                continue;
            };
            move_missing(&free.dir, &slot.dir);
        }
        return Ok(slot);
    }
    bail!("every debuginfod cache under {} is in use", root.display())
}

/// Removes the directories under `root` that no session holds, or with
/// `Act::DryRun` finds them and removes nothing.
pub fn collect(root: &Path, act: Act) -> Result<Vec<Unused>> {
    let mut unused = Vec::new();
    for number in numbers(root) {
        let Some(free) = try_hold(root, number)? else {
            continue;
        };
        let bytes = bytes_under(&free.dir);
        if act == Act::Remove {
            fs::remove_dir_all(&free.dir)
                .with_context(|| format!("removing {}", free.dir.display()))?;
        }
        unused.push(Unused {
            path: free.dir.clone(),
            bytes,
        });
    }
    Ok(unused)
}

/// Directory `number` under `root` held for this process, or None while
/// another holds it. The lock file is kept when the directory goes, since
/// a process may have it open to lock.
fn try_hold(root: &Path, number: u32) -> Result<Option<Slot>> {
    let dir = root.join(number.to_string());
    let path = dir.with_extension(LOCK_EXTENSION);
    let lock = File::create(&path).with_context(|| format!("creating {}", path.display()))?;
    match lock.try_lock() {
        Ok(()) => Ok(Some(Slot { dir, lock })),
        Err(fs::TryLockError::WouldBlock) => Ok(None),
        Err(fs::TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("locking {}", path.display()))
        }
    }
}

/// The numbers of the directories under `root`, smallest first.
fn numbers(root: &Path) -> Vec<u32> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut numbers: Vec<u32> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().to_str()?.parse().ok())
        .collect();
    numbers.sort_unstable();
    numbers
}

/// Moves each entry under `from` that `into` lacks to the same place in
/// `into`. Neither directory's server is running, so a whole entry stays
/// whole: it is renamed into the server's `cache` directory in one step.
fn move_missing(from: &Path, into: &Path) {
    for fetcher in fetchers(from, Path::new("")) {
        let Ok(entries) = fs::read_dir(from.join(&fetcher).join(SERVER_CACHE)) else {
            continue;
        };
        let target = into.join(&fetcher);
        if fs::create_dir_all(target.join(SERVER_CACHE)).is_err()
            || fs::create_dir_all(target.join(SERVER_PARTIAL)).is_err()
        {
            continue;
        }
        for entry in entries.flatten() {
            let to = target.join(SERVER_CACHE).join(entry.file_name());
            if fs::symlink_metadata(&to).is_ok() {
                continue;
            }
            let _ = fs::rename(entry.path(), &to);
        }
    }
}

/// The directories under `dir/relative` laid out as one of the server's
/// caches, with `cache` and `partial` in them, relative to `dir`. A
/// cache's own directories are not looked into.
fn fetchers(dir: &Path, relative: &Path) -> Vec<PathBuf> {
    let here = dir.join(relative);
    if here.join(SERVER_CACHE).is_dir() && here.join(SERVER_PARTIAL).is_dir() {
        return vec![relative.to_path_buf()];
    }
    let Ok(entries) = fs::read_dir(&here) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        found.extend(fetchers(dir, &relative.join(entry.file_name())));
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory for one test.
    fn scratch(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("rewind-debuginfod-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        root
    }

    /// Writes an entry `key` holding a file that says `text` into
    /// directory `number` under `root`, in the server's layout.
    fn entry(root: &Path, number: u32, key: &str, text: &str) -> PathBuf {
        let fetcher = root.join(number.to_string()).join("other/sources");
        fs::create_dir_all(fetcher.join(SERVER_PARTIAL)).unwrap();
        let entry = fetcher.join(SERVER_CACHE).join(key);
        fs::create_dir_all(&entry).unwrap();
        fs::write(entry.join("file"), text).unwrap();
        entry
    }

    /// Two sessions at once get directories of their own, and a directory
    /// let go is taken again. Takes two while both are held, then a third
    /// after the first is dropped, and checks the third reuses the first
    /// one's directory.
    #[test]
    fn sessions_at_once_get_directories_of_their_own() {
        let root = scratch("slots");
        let first = take(&root).unwrap();
        let second = take(&root).unwrap();
        assert_ne!(first.dir(), second.dir());
        assert!(first.dir().is_dir() && second.dir().is_dir());

        let freed = first.dir().to_path_buf();
        drop(first);
        let third = take(&root).unwrap();
        assert_eq!(third.dir(), freed);

        drop((second, third));
        fs::remove_dir_all(&root).unwrap();
    }

    /// Taking a directory moves in the entries it lacks from directories
    /// no session holds, keeps its own where both have one, and leaves
    /// alone a directory a session holds. Directory 0 has entry c,
    /// directory 1 has a and c, and directory 2, held, has b; taking
    /// directory 0 gets it a from 1 and keeps its own c, and nothing of 2.
    #[test]
    fn a_session_takes_in_what_free_directories_hold() {
        let root = scratch("merge");
        entry(&root, 0, "c", "zero");
        let a = entry(&root, 1, "a", "one");
        let c = entry(&root, 1, "c", "one");
        let b = entry(&root, 2, "b", "two");
        let held = try_hold(&root, 2).unwrap().unwrap();

        let taken = take(&root).unwrap();
        assert_eq!(taken.dir(), root.join("0"));
        let cache = taken.dir().join("other/sources").join(SERVER_CACHE);
        assert_eq!(fs::read_to_string(cache.join("a/file")).unwrap(), "one");
        assert_eq!(fs::read_to_string(cache.join("c/file")).unwrap(), "zero");
        assert!(!cache.join("b").exists());
        assert!(!a.exists());
        assert!(c.exists());
        assert!(b.join("file").exists());

        drop((taken, held));
        fs::remove_dir_all(&root).unwrap();
    }

    /// `rewind gc` removes the directories no session holds, with their
    /// sizes, and keeps one a session holds; a dry run removes nothing.
    /// Writes an entry into directories 0 and 1, holds 1, and collects.
    #[test]
    fn gc_removes_the_directories_no_session_holds() {
        let root = scratch("gc");
        entry(&root, 0, "a", "zero");
        entry(&root, 1, "b", "one");
        let held = try_hold(&root, 1).unwrap().unwrap();

        let planned = collect(&root, Act::DryRun).unwrap();
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].path, root.join("0"));
        assert!(planned[0].bytes > 0);
        assert!(root.join("0").exists());

        let removed = collect(&root, Act::Remove).unwrap();
        assert_eq!(removed, planned);
        assert!(!root.join("0").exists());
        assert!(root.join("1").exists());

        drop(held);
        fs::remove_dir_all(&root).unwrap();
    }
}
