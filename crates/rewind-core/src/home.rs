//! Where Rewind keeps runs, images and keyframes, and where it finds the
//! guest it boots.

use std::fs::{self, File};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The guest kernel and initramfs. The Nix package sets these to store
/// paths, the release tarball's launcher to the files beside it, and in a
/// checkout `nix develop` does.
pub const ENV_KERNEL: &str = "REWIND_KERNEL";
pub const ENV_INITRD: &str = "REWIND_INITRD";

/// The kernel package's `debug` output, its DWARF by build ID, for
/// `rewind gdb`; optional.
pub const ENV_KERNEL_DEBUG: &str = "REWIND_KERNEL_DEBUG";

/// How images are built, for the directory they are cached in: 2 since
/// image builds ignore SOURCE_DATE_EPOCH, which before then made an image
/// built in a Nix development shell differ from one built outside.
const IMAGES: &str = "2";

/// Overrides the data directory, which is otherwise under XDG_DATA_HOME.
pub const ENV_HOME: &str = "REWIND_HOME";

/// The file in the home that a process adding images or runs holds a
/// shared lock on, and `rewind gc` an exclusive one. The kernel drops a
/// lock when its process dies, however it dies.
const LOCK: &str = "lock";

/// The file in the home that a process registering a run, writing its
/// directory and manifest, holds a shared lock on, and one removing runs
/// an exclusive one from listing them to deleting them. A fork registered
/// between the two would name a parent the removal deletes.
const RUNS_LOCK: &str = "runs.lock";

/// A process's hold on the home while it packs images, executes runs or
/// mounts an image in a shell: `rewind gc` refuses while any is held, since
/// an image just packed is named by no run until the run's manifest is
/// written. Let go when dropped.
#[derive(Debug)]
pub struct InUse {
    _lock: File,
}

/// A process's hold on the runs while it registers one: its directory,
/// its executing lock and its manifest. Let go when dropped.
#[derive(Debug)]
pub struct Registering {
    _lock: File,
}

/// The runs held by one process alone while it lists and removes them,
/// so no run is registered between the listing and the deletion. Let go
/// when dropped.
#[derive(Debug)]
pub struct Removing {
    _lock: File,
}

/// The home held by one process alone, as `rewind gc` holds it while it
/// removes what no run uses. Let go when dropped.
#[derive(Debug)]
pub struct Alone {
    _lock: File,
}

#[derive(Debug)]
pub struct Home {
    root: PathBuf,
}

/// The variable naming the data directory the default home is under.
const ENV_XDG_DATA: &str = "XDG_DATA_HOME";

/// The user's home directory, under which the data directory is when
/// XDG_DATA_HOME names none.
const ENV_USER_HOME: &str = "HOME";

/// The data directory under the user's home, as the XDG spec defaults it.
const USER_DATA_DIR: &str = ".local/share";

/// The default home's directory under the data directory.
const DEFAULT_HOME_DIR: &str = "rewind";

/// Where the home is: REWIND_HOME, else rewind under XDG_DATA_HOME, else
/// under ~/.local/share, each variable read through `var`. A variable set
/// to nothing counts as unset, as the XDG spec has it: an empty path would
/// otherwise name the working directory, and a script whose variable was
/// never set would make a home wherever it runs.
fn root_from(var: impl Fn(&str) -> Option<std::ffi::OsString>) -> Result<PathBuf> {
    let set = |name: &str| var(name).filter(|value| !value.is_empty());
    if let Some(dir) = set(ENV_HOME) {
        return Ok(PathBuf::from(dir));
    }
    let data = set(ENV_XDG_DATA)
        .map(PathBuf::from)
        .or_else(|| set(ENV_USER_HOME).map(|h| Path::new(&h).join(USER_DATA_DIR)))
        .context("neither XDG_DATA_HOME nor HOME is set")?;
    Ok(data.join(DEFAULT_HOME_DIR))
}

impl Home {
    pub fn open() -> Result<Home> {
        Home::at(root_from(|name| std::env::var_os(name))?)
    }

    /// The home in `root`, made if it is not there yet.
    pub fn at(root: PathBuf) -> Result<Home> {
        let home = Home { root };
        fs::create_dir_all(home.runs())?;
        fs::create_dir_all(home.images())?;
        Ok(home)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn runs(&self) -> PathBuf {
        self.root.join("runs")
    }

    /// Where images are cached, by what went into them. The directory is
    /// named for how images are built, IMAGES, so an image an earlier way
    /// of building made is not taken for one this way makes; the runs that
    /// booted it still name it where it was.
    pub fn images(&self) -> PathBuf {
        self.image_cache().join(IMAGES)
    }

    /// The directory that holds the image cache of each way of building
    /// images, and the images of the first way at its top.
    pub fn image_cache(&self) -> PathBuf {
        self.root.join("images")
    }

    /// The kernel, initramfs and image of each imported replayable run,
    /// by run id.
    pub fn inputs(&self) -> PathBuf {
        self.root.join("inputs")
    }

    /// The page store every run's keyframes share.
    pub fn store(&self) -> PathBuf {
        self.root.join("store")
    }

    /// The source files read out of each run's VM, by run id (see
    /// [`crate::source_cache`]).
    pub fn source_cache(&self) -> PathBuf {
        self.root.join("cache").join("sources")
    }

    /// The source files each program or library names, by build ID (see
    /// [`crate::source_lists`]).
    pub fn source_lists(&self) -> PathBuf {
        self.root.join("cache").join("source-lists")
    }

    /// The caches of the debuginfod servers `rewind gdb` starts, one
    /// directory for each session running at once.
    pub fn debuginfod_cache(&self) -> PathBuf {
        self.root.join("cache").join("debuginfod")
    }

    fn lock_file(&self) -> Result<File> {
        let path = self.root.join(LOCK);
        rewind_store::open_lock(&path).with_context(|| format!("creating {}", path.display()))
    }

    /// Holds the home in use until the result is dropped, waiting first
    /// for a `rewind gc` that has it alone.
    pub fn in_use(&self) -> Result<InUse> {
        let lock = self.lock_file()?;
        match lock.try_lock_shared() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => {
                eprintln!("rewind: waiting for rewind gc to finish");
                lock.lock_shared()
                    .with_context(|| format!("locking {}", self.root.display()))?;
            }
            Err(fs::TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("locking {}", self.root.display()));
            }
        }
        Ok(InUse { _lock: lock })
    }

    fn runs_lock_file(&self) -> Result<File> {
        let path = self.root.join(RUNS_LOCK);
        rewind_store::open_lock(&path).with_context(|| format!("creating {}", path.display()))
    }

    /// Holds the runs while this process registers one, waiting first
    /// for a removal that has them alone.
    pub fn registering(&self) -> Result<Registering> {
        let lock = self.runs_lock_file()?;
        lock.lock_shared()
            .with_context(|| format!("locking the runs in {}", self.root.display()))?;
        Ok(Registering { _lock: lock })
    }

    /// The runs to this process alone, from listing them to deleting
    /// some, waiting first for any registration under way.
    pub fn removing(&self) -> Result<Removing> {
        let lock = self.runs_lock_file()?;
        lock.lock()
            .with_context(|| format!("locking the runs in {}", self.root.display()))?;
        Ok(Removing { _lock: lock })
    }

    /// The home to this process alone, or None while another process holds
    /// it in use or alone.
    pub fn alone(&self) -> Result<Option<Alone>> {
        let lock = self.lock_file()?;
        match lock.try_lock() {
            Ok(()) => Ok(Some(Alone { _lock: lock })),
            Err(fs::TryLockError::WouldBlock) => Ok(None),
            Err(fs::TryLockError::Error(e)) => {
                Err(e).with_context(|| format!("locking {}", self.root.display()))
            }
        }
    }
}

/// The disk space the files under `dir` take.
pub fn bytes_under(dir: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    /// st_blocks counts 512-byte blocks whatever the file system's own.
    const BLOCK: u64 = 512;
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut total = 0;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() {
            total += bytes_under(&entry.path());
            continue;
        }
        total += meta.blocks() * BLOCK;
    }
    total
}

/// The guest pieces named by the environment.
#[derive(Debug)]
pub struct Guest {
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    pub kernel_debug: Option<PathBuf>,
}

impl Guest {
    pub fn from_env() -> Result<Guest> {
        let var = |name: &str| {
            std::env::var_os(name).map(PathBuf::from).with_context(|| {
                format!(
                    "{name} is not set; run rewind from its Nix package or `nix develop`, or \
                     from the release's bin/rewind, the launcher that sets it (libexec/rewind \
                     is the bare binary)"
                )
            })
        };
        Ok(Guest {
            kernel: var(ENV_KERNEL)?,
            initrd: var(ENV_INITRD)?,
            kernel_debug: std::env::var_os(ENV_KERNEL_DEBUG).map(PathBuf::from),
        })
    }
}

#[cfg(test)]
mod tests {
    // Where the home is, from the environment as a lookup the test gives,
    // so no test sets the process's own variables.
    use super::*;
    use std::ffi::OsString;

    /// A lookup that answers the variables in `set` and no others.
    fn vars(set: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let set: Vec<(String, OsString)> = set
            .iter()
            .map(|(k, v)| (k.to_string(), OsString::from(v)))
            .collect();
        move |name| set.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
    }

    #[test]
    fn an_empty_variable_counts_as_unset() {
        // REWIND_HOME and XDG_DATA_HOME set to nothing, as a script with an
        // unset variable writes them: the home is the default under HOME,
        // not the working directory an empty path would name.
        let lookup = vars(&[("REWIND_HOME", ""), ("XDG_DATA_HOME", ""), ("HOME", "/h")]);
        assert_eq!(
            root_from(lookup).unwrap(),
            PathBuf::from("/h/.local/share/rewind")
        );
    }

    #[test]
    fn rewind_home_then_xdg_data_home_then_home() {
        // Each variable that names a directory wins over the ones after it.
        let all = vars(&[
            ("REWIND_HOME", "/r"),
            ("XDG_DATA_HOME", "/x"),
            ("HOME", "/h"),
        ]);
        assert_eq!(root_from(all).unwrap(), PathBuf::from("/r"));
        let data = vars(&[("XDG_DATA_HOME", "/x"), ("HOME", "/h")]);
        assert_eq!(root_from(data).unwrap(), PathBuf::from("/x/rewind"));
        assert!(root_from(vars(&[("HOME", "")])).is_err());
    }
}
