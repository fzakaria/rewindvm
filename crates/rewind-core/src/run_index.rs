//! Finding a run by name or by age without reading every manifest.
//!
//! `rewind <command> <run>` names a run by its id, a prefix of it, its
//! name, or `@N` for the Nth newest. A prefix is matched against the
//! names of the directories in `runs/`, but a name or `@N` needs every
//! run's name and age, which only its manifest holds: reading all of
//! them took 78 ms in a home of 6,233 runs, for every command.
//!
//! So the home keeps an index, `cache/runs.json`: for each directory in
//! `runs/`, the name and creation time its manifest gave when it was last
//! read, and when the manifest was written. The code paths that make,
//! replace and remove runs bring it up to date, under a lock of its own,
//! since the home's lock is held shared by every process that executes a
//! run. It is a cache: a reader that finds it missing, unreadable, or
//! naming other directories than `runs/` holds rebuilds it from the
//! manifests, and a lookup checks the manifest it opens says what the
//! index said, so a writer that does not know the index, or a copy of a
//! home, costs one rebuild and nothing else.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::home::Home;
use crate::run::MANIFEST;

/// The index, under the home's cache directory, and the file a writer
/// holds an exclusive lock on while it replaces the index.
const INDEX: &str = "runs.json";
const LOCK: &str = "runs.json.lock";

/// The index's format, named first so another build's index is rebuilt
/// rather than misread.
const VERSION: u32 = 1;

/// The index as the file holds it.
#[derive(Serialize, Deserialize)]
struct File {
    version: u32,
    runs: Vec<Entry>,
}

/// A directory in `runs/` and what its manifest said.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    /// The directory's name, which is the run's id.
    id: String,
    manifest: Status,
}

/// What a run directory's manifest gave when the index last read it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Status {
    /// The fields a lookup needs, and when the manifest was written, in
    /// nanoseconds since the Unix epoch.
    Read {
        name: String,
        created: u64,
        written: u128,
    },
    /// The manifest does not parse, as one another build wrote may not.
    Unreadable,
    /// There was no manifest, as in a directory a run is executing into
    /// for the first time. The index reads it again at every load until
    /// there is one.
    Missing,
}

/// What a lookup reads of a manifest: the fields it matches and orders
/// on, which every build's manifest has.
#[derive(Deserialize)]
struct Header {
    name: String,
    created: u64,
}

/// Reads what the manifest in `dir` says now.
fn status_of(dir: &Path) -> Status {
    let path = dir.join(MANIFEST);
    let Ok(bytes) = fs::read(&path) else {
        return Status::Missing;
    };
    let Ok(header) = serde_json::from_slice::<Header>(&bytes) else {
        return Status::Unreadable;
    };
    let written = fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    Status::Read {
        name: header.name,
        created: header.created,
        written,
    }
}

/// The names of the directories in `runs/`, in no order; none when the
/// home has no runs directory yet.
fn run_dirs(home: &Home) -> Result<Vec<String>> {
    let entries = match fs::read_dir(home.runs()) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).context("reading the runs directory"),
    };
    let mut dirs = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        dirs.push(entry.file_name().to_string_lossy().into_owned());
    }
    Ok(dirs)
}

/// Whether a load rebuilds the index from the manifests only when it is
/// missing, another build's or names other directories than `runs/`
/// holds, or whatever it says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Rebuild {
    IfStale,
    Now,
}

/// The index of a home's runs, as far as it is loaded.
#[derive(Debug)]
pub(crate) struct Index {
    home_runs: PathBuf,
    entries: Vec<Entry>,
}

impl Index {
    fn path(home: &Home) -> PathBuf {
        home.root().join("cache").join(INDEX)
    }

    /// The index file's entries, or None when there is no file this build
    /// reads.
    fn read(home: &Home) -> Option<Vec<Entry>> {
        let bytes = fs::read(Self::path(home)).ok()?;
        let file: File = serde_json::from_slice(&bytes).ok()?;
        if file.version != VERSION {
            return None;
        }
        Some(file.runs)
    }

    /// The index, brought up to date with the directories in `runs/`:
    /// rebuilt from the manifests when it is missing, another build's, or
    /// names other directories, and the manifests of directories that had
    /// none read again.
    pub(crate) fn load(home: &Home) -> Result<Index> {
        Self::load_with(home, Rebuild::IfStale)
    }

    /// [`Index::load`], or with `Rebuild::Now` the index rebuilt from the
    /// manifests whatever it says, for a lookup that found the index
    /// wrong about a run.
    pub(crate) fn load_with(home: &Home, rebuild: Rebuild) -> Result<Index> {
        let dirs = run_dirs(home)?;
        let fresh = match rebuild {
            Rebuild::IfStale => Self::read(home).filter(|entries| covers(entries, &dirs)),
            Rebuild::Now => None,
        };
        let entries = match fresh {
            Some(entries) => entries,
            None => {
                let _held = lock(home)?;
                let dirs = run_dirs(home)?;
                let entries = scan(home, &dirs);
                write(home, &entries)?;
                entries
            }
        };
        let mut index = Index {
            home_runs: home.runs(),
            entries,
        };
        for entry in &mut index.entries {
            if entry.manifest == Status::Missing {
                entry.manifest = status_of(&index.home_runs.join(&entry.id));
            }
        }
        Ok(index)
    }

    /// Every readable run's directory, newest first: by creation time,
    /// and among runs created in the same second by when the manifest was
    /// written, as `rewind ls` lists them.
    pub(crate) fn newest_first(&self) -> Vec<PathBuf> {
        let mut readable: Vec<(u64, u128, &str)> = self
            .entries
            .iter()
            .filter_map(|entry| match &entry.manifest {
                Status::Read {
                    created, written, ..
                } => Some((*created, *written, entry.id.as_str())),
                Status::Unreadable | Status::Missing => None,
            })
            .collect();
        readable.sort_by(|a, b| b.cmp(a));
        readable
            .into_iter()
            .map(|(_, _, id)| self.home_runs.join(id))
            .collect()
    }

    /// The directory of the newest run named `name`, if any is.
    pub(crate) fn named(&self, name: &str) -> Option<PathBuf> {
        let mut best: Option<(u64, u128, &str)> = None;
        for entry in &self.entries {
            let Status::Read {
                name: named,
                created,
                written,
            } = &entry.manifest
            else {
                continue;
            };
            if named != name {
                continue;
            }
            let candidate = (*created, *written, entry.id.as_str());
            if best.is_none_or(|b| candidate > b) {
                best = Some(candidate);
            }
        }
        best.map(|(_, _, id)| self.home_runs.join(id))
    }
}

/// Whether `entries` name exactly the directories `dirs`, each once.
fn covers(entries: &[Entry], dirs: &[String]) -> bool {
    if entries.len() != dirs.len() {
        return false;
    }
    let indexed: HashSet<&str> = entries.iter().map(|e| e.id.as_str()).collect();
    indexed.len() == dirs.len() && dirs.iter().all(|d| indexed.contains(d.as_str()))
}

/// Reads every manifest in `dirs`.
fn scan(home: &Home, dirs: &[String]) -> Vec<Entry> {
    let runs = home.runs();
    dirs.iter()
        .map(|id| Entry {
            id: id.clone(),
            manifest: status_of(&runs.join(id)),
        })
        .collect()
}

/// Holds the index's lock until the result is dropped.
fn lock(home: &Home) -> Result<fs::File> {
    let path = home.root().join("cache").join(LOCK);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = rewind_store::open_lock(&path)?;
    file.lock()
        .with_context(|| format!("locking {}", path.display()))?;
    Ok(file)
}

/// Replaces the index with `entries`, whole or not at all. The caller
/// holds the lock.
fn write(home: &Home, entries: &[Entry]) -> Result<()> {
    let file = File {
        version: VERSION,
        runs: entries.to_vec(),
    };
    crate::image::write_atomic(&Index::path(home), &serde_json::to_vec(&file)?)
}

/// Changes the entries of the index on disk under its lock: loads them,
/// rebuilding when stale, hands them to `change`, and writes them back.
fn update(home: &Home, change: impl FnOnce(&mut Vec<Entry>)) -> Result<()> {
    let _held = lock(home)?;
    let dirs = run_dirs(home)?;
    let mut entries = match Index::read(home).filter(|entries| covers(entries, &dirs)) {
        Some(entries) => entries,
        None => scan(home, &dirs),
    };
    change(&mut entries);
    write(home, &entries)
}

/// Brings the index up to date with the manifest in the run directory
/// `dir`, which a writer calls after writing one. A failure leaves no
/// index, so the next reader rebuilds it rather than trust a stale one.
pub(crate) fn note(home: &Home, dir: &Path) -> Result<()> {
    let id = dir
        .file_name()
        .context("a run directory has a name")?
        .to_string_lossy()
        .into_owned();
    let manifest = status_of(dir);
    let noted = update(home, |entries| {
        entries.retain(|e| e.id != id);
        entries.push(Entry { id, manifest });
    });
    if noted.is_err() {
        let _ = fs::remove_file(Index::path(home));
    }
    noted
}

/// Takes the run `id` out of the index, which a writer calls after
/// removing its directory. A failure leaves no index, as `note` does.
pub(crate) fn forget(home: &Home, id: &str) -> Result<()> {
    let forgotten = update(home, |entries| entries.retain(|e| e.id != id));
    if forgotten.is_err() {
        let _ = fs::remove_file(Index::path(home));
    }
    forgotten
}

#[cfg(test)]
mod tests {
    // The index over homes in temporary directories: what a lookup finds
    // through it, when it is rebuilt, and that writers keep it right.
    use super::*;
    use crate::run::Run;
    use crate::run::tests::manifest;

    /// A fresh home for one test.
    fn home(name: &str) -> (PathBuf, Home) {
        let root =
            std::env::temp_dir().join(format!("rewind-run-index-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        (root.clone(), Home::at(root).unwrap())
    }

    /// Writes a run directory for `id` named `name`, made at `created`,
    /// without telling the index, as another build would.
    fn write_run(home: &Home, id: &str, name: &str, created: u64) -> PathBuf {
        let dir = home.runs().join(id);
        fs::create_dir_all(&dir).unwrap();
        let m = manifest(id, name, created);
        fs::write(dir.join(MANIFEST), serde_json::to_vec(&m).unwrap()).unwrap();
        dir
    }

    /// The id of the run at `dir`.
    fn id_of(dir: Option<PathBuf>) -> String {
        dir.unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn a_lookup_reads_the_index_and_not_every_manifest() {
        // Three runs are indexed. Their manifests are then made unreadable
        // behind the index's back, and a lookup by name and by age still
        // finds them, since the index holds what it needs. A run written
        // after, which the index does not name, makes the next load
        // rebuild it from the manifests, so the run is found and the
        // unreadable ones are not.
        let (root, home) = home("reads-index");
        write_run(&home, "aaaa000000000000", "mylib", 1);
        write_run(&home, "bbbb000000000000", "other", 2);
        write_run(&home, "cccc000000000000", "mylib", 3);
        let index = Index::load(&home).unwrap();
        assert_eq!(id_of(index.named("mylib")), "cccc000000000000");
        assert!(Index::path(&home).is_file());

        for id in ["aaaa000000000000", "bbbb000000000000", "cccc000000000000"] {
            fs::write(home.runs().join(id).join(MANIFEST), b"{}").unwrap();
        }
        let index = Index::load(&home).unwrap();
        assert_eq!(id_of(index.named("mylib")), "cccc000000000000");
        assert_eq!(id_of(index.named("other")), "bbbb000000000000");
        let newest: Vec<String> = index
            .newest_first()
            .into_iter()
            .map(|d| id_of(Some(d)))
            .collect();
        assert_eq!(
            newest,
            ["cccc000000000000", "bbbb000000000000", "aaaa000000000000"]
        );

        write_run(&home, "dddd000000000000", "late", 4);
        let index = Index::load(&home).unwrap();
        assert_eq!(id_of(index.named("late")), "dddd000000000000");
        assert!(index.named("mylib").is_none());
        assert_eq!(index.newest_first().len(), 1);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_removed_or_replaced_run_makes_the_index_rebuild() {
        // A run removed behind the index's back is not found afterwards,
        // and the others are. An index file of another format, or none, is
        // rebuilt too.
        let (root, home) = home("rebuild");
        write_run(&home, "aaaa000000000000", "a", 1);
        write_run(&home, "bbbb000000000000", "b", 2);
        drop(Index::load(&home).unwrap());

        fs::remove_dir_all(home.runs().join("bbbb000000000000")).unwrap();
        let index = Index::load(&home).unwrap();
        assert!(index.named("b").is_none());
        assert_eq!(id_of(index.named("a")), "aaaa000000000000");

        fs::write(Index::path(&home), b"{\"version\": 0, \"runs\": []}").unwrap();
        assert_eq!(
            id_of(Index::load(&home).unwrap().named("a")),
            "aaaa000000000000"
        );
        fs::remove_file(Index::path(&home)).unwrap();
        assert_eq!(
            id_of(Index::load(&home).unwrap().named("a")),
            "aaaa000000000000"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn writers_keep_the_index_right_without_a_rebuild() {
        // A run noted after its manifest is written, one noted again under
        // another name, and one forgotten after its directory goes: each
        // lookup answers from the index as it stands, which the manifests
        // made unreadable behind it show.
        let (root, home) = home("writers");
        write_run(&home, "aaaa000000000000", "a", 1);
        drop(Index::load(&home).unwrap());

        let dir = write_run(&home, "bbbb000000000000", "b", 2);
        note(&home, &dir).unwrap();
        fs::write(dir.join(MANIFEST), b"{}").unwrap();
        assert_eq!(
            id_of(Index::load(&home).unwrap().named("b")),
            "bbbb000000000000"
        );

        let dir = write_run(&home, "bbbb000000000000", "renamed", 2);
        note(&home, &dir).unwrap();
        fs::write(dir.join(MANIFEST), b"{}").unwrap();
        let index = Index::load(&home).unwrap();
        assert!(index.named("b").is_none());
        assert_eq!(id_of(index.named("renamed")), "bbbb000000000000");

        fs::remove_dir_all(&dir).unwrap();
        forget(&home, "bbbb000000000000").unwrap();
        let index = Index::load(&home).unwrap();
        assert!(index.named("renamed").is_none());
        assert_eq!(index.newest_first().len(), 1);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_directory_without_a_manifest_is_read_again_until_it_has_one() {
        // A run executing for the first time has a directory and no
        // manifest yet. The index names the directory, so the manifest
        // written later, by a writer that does not note it, is read on the
        // next load without a rebuild, and the run is found.
        let (root, home) = home("missing");
        write_run(&home, "aaaa000000000000", "a", 1);
        let making = home.runs().join("bbbb000000000000");
        fs::create_dir_all(&making).unwrap();
        let index = Index::load(&home).unwrap();
        assert_eq!(index.newest_first().len(), 1);
        assert!(
            Index::read(&home)
                .unwrap()
                .iter()
                .any(|e| e.manifest == Status::Missing)
        );

        write_run(&home, "bbbb000000000000", "b", 2);
        let index = Index::load(&home).unwrap();
        assert_eq!(id_of(index.named("b")), "bbbb000000000000");
        assert_eq!(index.newest_first().len(), 2);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn writers_noting_at_once_keep_every_run() {
        // Sixteen threads each write a run and note it at once, as a
        // check's workers do: every run is in the index afterwards, and
        // the file is whole.
        const THREADS: usize = 16;
        let (root, home) = home("at-once");
        drop(Index::load(&home).unwrap());
        let start = std::sync::Arc::new(std::sync::Barrier::new(THREADS));
        let writers: Vec<_> = (0..THREADS)
            .map(|i| {
                let (root, start) = (root.clone(), start.clone());
                std::thread::spawn(move || {
                    let home = Home::at(root).unwrap();
                    let id = format!("{i:016x}");
                    let dir = write_run(&home, &id, &format!("run-{i}"), i as u64);
                    start.wait();
                    note(&home, &dir).unwrap();
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }
        let entries = Index::read(&home).unwrap();
        assert_eq!(entries.len(), THREADS);
        let index = Index::load(&home).unwrap();
        for i in 0..THREADS {
            assert_eq!(id_of(index.named(&format!("run-{i}"))), format!("{i:016x}"));
        }
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn find_checks_the_run_it_opens_against_the_index() {
        // The index names a run under a name its manifest no longer has,
        // as a writer that does not know the index leaves it: a lookup by
        // the old name finds nothing rather than that run, and by the new
        // name finds it, after rebuilding the index.
        let (root, home) = home("stale-name");
        write_run(&home, "aaaa000000000000", "old", 1);
        drop(Index::load(&home).unwrap());
        write_run(&home, "aaaa000000000000", "new", 1);
        let err = Run::find(&home, "old").err().unwrap().to_string();
        assert!(err.contains("no run matches"), "{err}");
        assert_eq!(
            Run::find(&home, "new").unwrap().manifest.id,
            "aaaa000000000000"
        );
        fs::remove_dir_all(&root).unwrap();
    }
}
