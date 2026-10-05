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

#[cfg(test)]
mod tests {
    // Paths a VM could name, mapped under a session directory, with no
    // file touched.
    use super::*;

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
