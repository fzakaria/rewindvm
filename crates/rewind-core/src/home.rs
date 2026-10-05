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

/// A process's hold on the home while it packs images, executes runs or
/// mounts an image in a shell: `rewind gc` refuses while any is held, since
/// an image just packed is named by no run until the run's manifest is
/// written. Let go when dropped.
pub struct InUse {
    _lock: File,
}

/// The home held by one process alone, as `rewind gc` holds it while it
/// removes what no run uses. Let go when dropped.
pub struct Alone {
    _lock: File,
}

pub struct Home {
    root: PathBuf,
}

impl Home {
    pub fn open() -> Result<Home> {
        let root = match std::env::var_os(ENV_HOME) {
            Some(dir) => PathBuf::from(dir),
            None => {
                let data = std::env::var_os("XDG_DATA_HOME")
                    .map(PathBuf::from)
                    .or_else(|| {
                        std::env::var_os("HOME").map(|h| Path::new(&h).join(".local/share"))
                    })
                    .context("neither XDG_DATA_HOME nor HOME is set")?;
                data.join("rewind")
            }
        };
        Home::at(root)
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

    fn lock_file(&self) -> Result<File> {
        let path = self.root.join(LOCK);
        File::create(&path).with_context(|| format!("creating {}", path.display()))
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

/// The guest pieces named by the environment.
pub struct Guest {
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    pub kernel_debug: Option<PathBuf>,
}

impl Guest {
    pub fn from_env() -> Result<Guest> {
        let var = |name: &str| {
            std::env::var_os(name).map(PathBuf::from).with_context(|| {
                format!("{name} is not set; run rewind from its Nix package or `nix develop`")
            })
        };
        Ok(Guest {
            kernel: var(ENV_KERNEL)?,
            initrd: var(ENV_INITRD)?,
            kernel_debug: std::env::var_os(ENV_KERNEL_DEBUG).map(PathBuf::from),
        })
    }
}
