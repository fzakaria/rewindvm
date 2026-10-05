//! The engine behind the `rewind` command and the desktop app: building
//! inputs, executing runs, and finding them again.

pub mod compare;
pub mod cpio;
pub mod debug;
pub mod export;
pub mod gc;
pub mod guest_path;
pub mod home;
pub mod image;
pub mod inspect;
pub mod keyframes;
pub mod maps;
pub mod nix;
pub mod pmu;
pub mod prune;
pub mod run;
#[cfg(test)]
mod settle;
pub mod source_cache;
pub mod threads;

pub use home::{Guest, Home};

/// This rewind, as `rewind --version` and run manifests name it: its
/// version, and in parentheses the commit it was built from when the
/// build knew it, as in `0.4.1 (5a6d5c6b072e)`.
pub const VERSION: &str = env!("REWIND_VERSION");
pub use run::{
    Echo, Execution, Keyframes, Manifest, Run, RunOutcome, Source, Spec, SpecExt, TimeLimit,
};
