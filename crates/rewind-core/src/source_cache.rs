//! Source files only a run's VM has, kept between lookups.
//!
//! `rewind where` and `rewind gdb` read the source files a program the VM
//! built names, such as /build/mylib/src/pool.c, out of a fork of the run.
//! A file keeps its contents until something writes, renames or unlinks
//! it, and the trace records each of those, so the contents read at one
//! step serve every step with the same last such event. The cache keeps
//! them in the home under `cache/sources/<run id>`, in a directory named
//! for that event, files under `files/` by their path in the VM and paths
//! the VM did not have under `absent/`.
//!
//! A run's cache goes with the run: `rewind remove` and `rewind prune`
//! delete it with the run's directory, and `rewind gc` deletes the caches
//! of runs that are gone.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
pub use rewind_trace::contents::{Version, version};

use crate::home::Home;

/// The directories of one version: the files, and markers for the paths
/// the VM did not have.
const FILES: &str = "files";
const ABSENT: &str = "absent";

/// The directory of the version no event changed.
const ORIGINAL: &str = "original";

/// What the cache holds for a path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    /// The file's contents.
    File(Vec<u8>),
    /// The VM had no such file.
    Absent,
}

/// One run's cache.
pub struct SourceCache {
    dir: PathBuf,
}

impl SourceCache {
    /// The cache of the run with id `run`.
    pub fn of(home: &Home, run: &str) -> SourceCache {
        SourceCache {
            dir: home.source_cache().join(run),
        }
    }

    /// The directory of `version`, None for a changing file.
    fn version_dir(&self, version: Version) -> Option<PathBuf> {
        match version {
            Version::Original => Some(self.dir.join(ORIGINAL)),
            Version::Since(step) => Some(self.dir.join(step.to_string())),
            Version::Changing => None,
        }
    }

    /// Where `path` is kept in `kind`, FILES or ABSENT, at `version`: by
    /// the path it leads to, so it never leads out of the cache. None for
    /// a relative path.
    fn place(&self, version: Version, kind: &str, path: &str) -> Option<PathBuf> {
        let path = rewind_trace::contents::normal(path)?;
        let inside = path.trim_start_matches('/');
        Some(self.version_dir(version)?.join(kind).join(inside))
    }

    /// What the cache holds for `path` at `version`, if anything.
    pub fn get(&self, version: Version, path: &str) -> Option<Entry> {
        if let Ok(bytes) = fs::read(self.place(version, FILES, path)?) {
            return Some(Entry::File(bytes));
        }
        if self.place(version, ABSENT, path)?.is_file() {
            return Some(Entry::Absent);
        }
        None
    }

    /// Keeps `entry` for `path` at `version`, written whole or not at all.
    /// A changing file is not kept.
    pub fn put(&self, version: Version, path: &str, entry: &Entry) -> Result<()> {
        let (kind, bytes) = match entry {
            Entry::File(bytes) => (FILES, bytes.as_slice()),
            Entry::Absent => (ABSENT, &[][..]),
        };
        let Some(to) = self.place(version, kind, path) else {
            return Ok(());
        };
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        crate::image::write_atomic(&to, bytes).with_context(|| format!("writing {}", to.display()))
    }
}

/// Deletes the cache of run `run`, if it has one.
pub fn remove(home: &Home, run: &str) -> Result<()> {
    let dir = home.source_cache().join(run);
    match fs::remove_dir_all(&dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", dir.display())),
    }
}

#[cfg(test)]
mod tests {
    // The cache over a temporary home: a hit, a miss, a miss after a later
    // write, and removal.
    use super::*;

    /// A fresh home for one test.
    fn home(name: &str) -> Home {
        let root =
            std::env::temp_dir().join(format!("rewind-source-cache-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        Home::at(root).unwrap()
    }

    #[test]
    fn a_kept_file_is_found_at_its_version_only() {
        // A file kept at one version is a hit at that version and a miss
        // at a later one, as after a later write; a path the VM lacked is
        // kept as absent; a path through `..` is the path it leads to; a
        // changing file, or a relative path, is never kept.
        let home = home("hit");
        let cache = SourceCache::of(&home, "8fd5378ddf70075e");
        let path = "/build/mylib/src/pool.c";
        assert_eq!(cache.get(Version::Since(10), path), None);

        let file = Entry::File(b"int x;\n".to_vec());
        cache.put(Version::Since(10), path, &file).unwrap();
        assert_eq!(cache.get(Version::Since(10), path), Some(file.clone()));
        assert_eq!(cache.get(Version::Since(30), path), None);
        assert_eq!(cache.get(Version::Original, path), None);

        cache
            .put(Version::Original, "/src/gone.c", &Entry::Absent)
            .unwrap();
        assert_eq!(
            cache.get(Version::Original, "/src/gone.c"),
            Some(Entry::Absent)
        );

        cache.put(Version::Changing, path, &file).unwrap();
        assert_eq!(cache.get(Version::Changing, path), None);

        let header = Entry::File(b"struct pool;\n".to_vec());
        let roundabout = "/build/mylib/tests/../src/pool.h";
        cache.put(Version::Since(10), roundabout, &header).unwrap();
        let straight = cache.get(Version::Since(10), "/build/mylib/src/pool.h");
        assert_eq!(straight, Some(header));

        let relative = "../escape.c";
        cache.put(Version::Original, relative, &file).unwrap();
        assert_eq!(cache.get(Version::Original, relative), None);
        fs::remove_dir_all(home.root()).unwrap();
    }

    #[test]
    fn removing_a_run_s_cache_leaves_the_others() {
        // remove() deletes one run's cache directory and no other, and a
        // run without a cache is not an error.
        let home = home("remove");
        let file = Entry::File(b"x".to_vec());
        SourceCache::of(&home, "a")
            .put(Version::Original, "/src/a.c", &file)
            .unwrap();
        SourceCache::of(&home, "b")
            .put(Version::Original, "/src/b.c", &file)
            .unwrap();
        remove(&home, "a").unwrap();
        remove(&home, "never-cached").unwrap();
        assert!(!home.source_cache().join("a").exists());
        assert_eq!(
            SourceCache::of(&home, "b").get(Version::Original, "/src/b.c"),
            Some(file)
        );
        fs::remove_dir_all(home.root()).unwrap();
    }
}
