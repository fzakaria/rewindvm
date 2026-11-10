//! Paths inside the VM, as paths on this machine.
//!
//! Source paths in a program's DWARF and the paths of files the VM maps
//! come from the run, which a build controls. Rewind writes copies of
//! those files under a directory of its own, so a path that climbs above
//! the VM's root must not climb out of that directory.

use std::path::{Component, Path, PathBuf};

/// `path`, a path inside the VM, as the same path under `dir` on this
/// machine. A `..` that stays inside the VM's root is kept as it is, since
/// gdb opens a source path such as /build/x/tests/../src/a.c with its
/// `..` and needs the directories on the way. None when a `..` would climb
/// above the root, which would land outside `dir`.
pub fn under(dir: &Path, path: &str) -> Option<PathBuf> {
    let mut depth = 0usize;
    for component in Path::new(path).components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::ParentDir => depth = depth.checked_sub(1)?,
            Component::RootDir | Component::CurDir => {}
            Component::Prefix(_) => return None,
        }
    }
    Some(dir.join(path.trim_start_matches('/')))
}

/// The Nix store on this machine.
const NIX_STORE: &str = "/nix/store";

/// `path`, a path inside the VM that names a store file, as that store
/// file on this machine: the path from its `/nix/store/` on, when every
/// name below the store is a plain one and the file is there and
/// resolves under this machine's store. A path through `..`, such as
/// `/nix/store/../../etc/passwd`, which a forged run could name, is no
/// store file. `prefix` is what the VM has before `/nix/store`, such as a
/// root filesystem's mount, or nothing.
pub fn in_store(path: &str, prefix: &str) -> Option<PathBuf> {
    let store = path.strip_prefix(prefix)?;
    if !store.starts_with(NIX_STORE) {
        return None;
    }
    let below = Path::new(store).strip_prefix(NIX_STORE).ok()?;
    let mut depth = 0usize;
    for component in below.components() {
        match component {
            Component::Normal(_) => depth += 1,
            _ => return None,
        }
    }
    if depth == 0 {
        return None;
    }
    let here = PathBuf::from(store);
    let resolved = std::fs::canonicalize(&here).ok()?;
    resolved.starts_with(NIX_STORE).then_some(here)
}

#[cfg(test)]
mod tests {
    // Paths a VM could name, mapped under a session directory, with no
    // file touched.
    use super::*;

    #[test]
    fn a_store_path_is_only_one_with_plain_names_that_resolves_in_the_store() {
        // The store file this test runs from is one; the same path
        // through a root's prefix is too, and only with that prefix. A
        // path through `..` or `.`, the store itself, and a path outside
        // the store are none.
        let exe = std::fs::read_link("/proc/self/exe").unwrap();
        let exe = exe.to_str().unwrap();
        if !exe.starts_with(NIX_STORE) {
            eprintln!("the test runs from outside the store; skipped");
            return;
        }
        assert_eq!(in_store(exe, ""), Some(PathBuf::from(exe)));
        let in_root = format!("/root{exe}");
        assert_eq!(in_store(&in_root, "/root"), Some(PathBuf::from(exe)));
        assert_eq!(in_store(&in_root, ""), None);
        assert_eq!(in_store("/nix/store/../../proc/self/exe", ""), None);
        assert_eq!(in_store("/nix/store/./x/bin/x", ""), None);
        assert_eq!(in_store("/nix/store", ""), None);
        assert_eq!(in_store("/nix/store/", ""), None);
        assert_eq!(in_store("/proc/self/exe", ""), None);
    }

    #[test]
    fn a_guest_path_lands_under_its_directory() {
        // Absolute and relative paths land under the directory; a `..`
        // that stays inside the root is kept for gdb to resolve; one that
        // climbs above the root, at the start or partway, gives none.
        let dir = Path::new("/tmp/session");
        let under = |path: &str| under(dir, path);
        assert_eq!(
            under("/build/mylib/src/pool.c"),
            Some(dir.join("build/mylib/src/pool.c"))
        );
        assert_eq!(
            under("/build/mylib/tests/../src/pool.h"),
            Some(dir.join("build/mylib/tests/../src/pool.h"))
        );
        assert_eq!(under("src/pool.c"), Some(dir.join("src/pool.c")));
        assert_eq!(under("/../etc/passwd"), None);
        assert_eq!(under("/build/../../home/.bashrc"), None);
        assert_eq!(under("../x"), None);
    }
}
