//! The engine behind the `rewind` command and the desktop app: building
//! inputs, executing runs, and finding them again.

pub mod cpio;
pub mod home;
pub mod image;
pub mod nix;
pub mod run;

pub use home::{Guest, Home};
pub use run::{Echo, Manifest, Run, RunOutcome, Source, Spec};
