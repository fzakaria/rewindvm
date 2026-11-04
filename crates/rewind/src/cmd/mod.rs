//! The `rewind` subcommands, a module per kind of work: each takes its
//! arguments and the home, and returns the exit code.

pub(crate) mod blame;
pub(crate) mod blame_memory;
pub(crate) mod bugreport;
pub(crate) mod inspect;
pub(crate) mod record;
pub(crate) mod remove;
pub(crate) mod setup;
pub(crate) mod transfer;
pub(crate) mod view;
