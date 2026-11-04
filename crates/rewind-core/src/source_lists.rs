//! The source files each program or library names, kept between lookups.
//!
//! `rewind where` and `rewind gdb` ask a gdb of their own for the source
//! files each of a process's files names outside the store, to know which
//! ones to read out of the VM. That gdb loads every file's symbols, which
//! took 0.45 s for the 13 files of a Rust compiler process. What a file
//! names is in the file, so the list is kept in the home under
//! `cache/source-lists`, by the file's build ID and length: a stripped
//! copy keeps the build ID of the file it was stripped from and names
//! no sources, and the length tells the two apart. A file with no build
//! ID, as many in the store are, is kept by its store path instead, and
//! one outside the store, as a program the VM built, by its contents.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::home::Home;

/// Where Nix keeps store paths, whose contents never change.
const NIX_STORE: &str = "/nix/store";

/// What a file's list is kept by, with the file's length in bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Key {
    /// The file's GNU build ID.
    BuildId { build_id: String, len: u64 },
    /// For a file with no build ID, the hash of its path in the VM, a
    /// store path.
    StorePath { hash: String, len: u64 },
    /// For a file with neither, the hash of its contents.
    Contents { hash: String, len: u64 },
}

impl Key {
    /// The key of the file read from `path`, which the VM had at `in_vm`:
    /// by its build ID, else by `in_vm` when that is in the store, else by
    /// its contents. None when it cannot be read.
    pub fn of(path: &Path, in_vm: &Path) -> Option<Key> {
        let mut file = fs::File::open(path).ok()?;
        let len = file.metadata().ok()?.len();
        if let Some(build_id) = crate::maps::build_id(&mut file) {
            return Some(Key::BuildId { build_id, len });
        }
        if in_vm.starts_with(NIX_STORE) {
            let hash = blake3::hash(in_vm.as_os_str().as_encoded_bytes())
                .to_hex()
                .to_string();
            return Some(Key::StorePath { hash, len });
        }
        let hash = crate::image::hash_file(path).ok()?;
        Some(Key::Contents { hash, len })
    }

    /// The name of the key's file in the cache.
    fn file_name(&self) -> String {
        match self {
            Key::BuildId { build_id, len } => format!("{build_id}-{len}.json"),
            Key::StorePath { hash, len } => format!("store-{hash}-{len}.json"),
            Key::Contents { hash, len } => format!("contents-{hash}-{len}.json"),
        }
    }
}

/// The cache of source lists in one home.
#[derive(Debug)]
pub struct SourceLists {
    dir: PathBuf,
}

impl SourceLists {
    /// The cache in `home`.
    pub fn of(home: &Home) -> SourceLists {
        SourceLists {
            dir: home.source_lists(),
        }
    }

    /// The source files kept for `key`, None when none are kept.
    pub fn get(&self, key: &Key) -> Option<Vec<String>> {
        let bytes = fs::read(self.dir.join(key.file_name())).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Keeps `sources` as the files the file with `key` names.
    pub fn put(&self, key: &Key, sources: &[String]) -> Result<()> {
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        let path = self.dir.join(key.file_name());
        let bytes = serde_json::to_vec(sources)?;
        crate::image::write_atomic(&path, &bytes)
            .with_context(|| format!("writing {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    // The cache over a temporary home, with keys made up rather than read
    // from files, and the keys of files that have no build ID.
    use super::*;

    /// A fresh home for one test.
    fn home(name: &str) -> Home {
        let root =
            std::env::temp_dir().join(format!("rewind-source-lists-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        Home::at(root).unwrap()
    }

    /// A list kept for a key is found by that key alone: the same build ID
    /// at another length is a miss, as for a stripped copy, and so is a
    /// key nothing was kept for. An empty list is kept as a list.
    #[test]
    fn a_list_is_found_by_its_build_id_and_length() {
        let home = home("hit");
        let lists = SourceLists::of(&home);
        let key = Key::BuildId {
            build_id: "ab12".into(),
            len: 4096,
        };
        let stripped = Key::BuildId {
            build_id: "ab12".into(),
            len: 1024,
        };
        assert_eq!(lists.get(&key), None);

        let sources = vec!["/build/mylib/src/pool.c".to_string()];
        lists.put(&key, &sources).unwrap();
        assert_eq!(lists.get(&key), Some(sources));
        assert_eq!(lists.get(&stripped), None);

        lists.put(&stripped, &[]).unwrap();
        assert_eq!(lists.get(&stripped), Some(Vec::new()));
        fs::remove_dir_all(home.root()).unwrap();
    }

    /// A file with no build ID is kept by its path in the VM when that is
    /// a store path, whose contents never change: this test's own source
    /// file is not ELF, so it has no build ID.
    #[test]
    fn a_file_without_a_build_id_is_kept_by_its_store_path() {
        let file = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src/source_lists.rs"));
        let in_store = Path::new("/nix/store/abc-mylib/lib/libmylib.so");
        let key = Key::of(file, in_store).expect("a store path's key");
        assert_eq!(Key::of(file, in_store), Some(key.clone()));
        assert_ne!(
            Key::of(file, Path::new("/nix/store/def-mylib/lib/libmylib.so")),
            Some(key)
        );
        assert_eq!(Key::of(Path::new("/no/such/file"), in_store), None);
    }

    /// A file outside the store with no build ID, as a program the VM
    /// built, is kept by its contents: two copies of the same bytes share
    /// a key wherever the VM had them, and other bytes do not.
    #[test]
    fn a_file_outside_the_store_is_kept_by_its_contents() {
        let home = home("contents");
        fs::create_dir_all(home.root()).unwrap();
        let (a, b, c) = (
            home.root().join("a"),
            home.root().join("b"),
            home.root().join("c"),
        );
        fs::write(&a, b"int main;").unwrap();
        fs::write(&b, b"int main;").unwrap();
        fs::write(&c, b"int main!").unwrap();
        let built = Path::new("/build/mylib/tests/test_pool");
        let key = Key::of(&a, built).expect("a key by contents");
        assert_eq!(Key::of(&b, Path::new("/src/elsewhere")), Some(key.clone()));
        assert_ne!(Key::of(&c, built), Some(key));
        fs::remove_dir_all(home.root()).unwrap();
    }
}
