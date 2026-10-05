//! The `rewind` subcommands, a module per kind of work: each takes its
//! arguments and the home, and returns the exit code.

pub mod inspect;
pub mod record;
pub mod remove;
pub mod setup;
pub mod transfer;
pub mod view;
