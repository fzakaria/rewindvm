//! The engine behind the `rewind` command and the desktop app: building
//! inputs, executing runs, and finding them again.

pub mod cpio;
pub mod debug;
pub mod export;
pub mod home;
pub mod image;
pub mod inspect;
pub mod keyframes;
pub mod maps;
pub mod nix;
pub mod pmu;
pub mod run;

pub use home::{Guest, Home};
pub use run::{Echo, Keyframes, Manifest, Run, RunOutcome, Source, Spec};
