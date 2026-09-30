//! Where Rewind keeps runs, images and keyframes, and where it finds the
//! guest it boots.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The guest kernel and initramfs. The Nix package sets these to store
/// paths, the release tarball's launcher to the files beside it, and in a
/// checkout `nix develop` does.
pub const ENV_KERNEL: &str = "REWIND_KERNEL";
pub const ENV_INITRD: &str = "REWIND_INITRD";

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
        std::fs::create_dir_all(root.join("runs"))?;
        std::fs::create_dir_all(root.join("images"))?;
        Ok(Home { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn runs(&self) -> PathBuf {
        self.root.join("runs")
    }

    pub fn images(&self) -> PathBuf {
        self.root.join("images")
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
        })
    }
}
