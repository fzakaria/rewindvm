//! Where Rewind keeps runs, images and keyframes, and where it finds the
//! guest it boots.

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
        std::fs::create_dir_all(root.join("runs"))?;
        std::fs::create_dir_all(root.join("images").join(IMAGES))?;
        Ok(Home { root })
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
        self.root.join("images").join(IMAGES)
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
