//! The engine behind the `rewind` command and the desktop app: building
//! inputs, executing runs, and finding them again.

// Only rewind-vmm and the command touch memory and the kernel directly.
#![forbid(unsafe_code)]

pub mod accesses;
pub mod bisect;
pub mod check;
pub mod compare;
pub mod cpio;
pub mod debug;
pub mod debuginfod_cache;
pub mod export;
pub mod gc;
pub mod gdb_regs;
pub mod guest_path;
pub mod home;
pub mod host_instructions;
pub mod image;
pub mod inspect;
pub mod keyframes;
pub mod link_map;
pub mod maps;
pub mod nix;
pub mod pmu;
pub mod progress;
pub mod prune;
pub mod run;
mod run_index;
#[cfg(test)]
mod settle;
pub mod source_cache;
pub mod source_lists;
pub mod state;
pub mod threads;
pub mod tree;

pub use home::{Guest, Home};

/// This rewind, as `rewind --version` and run manifests name it: its
/// version, and in parentheses the commit it was built from when the
/// build knew it, as in `0.4.1 (5a6d5c6b072e)`.
pub const VERSION: &str = env!("REWIND_VERSION");
pub use run::{
    Created, Echo, Execution, Keyframes, Manifest, Run, RunOutcome, Source, Spec, SpecExt,
    TimeLimit,
};

/// The error for a state the engine should never reach, such as a replay
/// that stopped for a reason nothing asked for: `doing` says what was
/// going on and `came_to` is the state, in its Rust form, for the bug
/// report. A person cannot act on the state, so the message says the
/// fault is Rewind's and asks for a report.
pub fn internal_error(doing: &str, came_to: impl std::fmt::Debug) -> anyhow::Error {
    anyhow::anyhow!(
        "internal error: {doing} came to {came_to:?}. This is a gap in Rewind, not in the \
         program: please report it with `rewind bugreport <run>`"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_internal_error_names_the_state_and_asks_for_a_report() {
        // The message is built from what was going on and the state the
        // engine came to, and sends the reader to rewind bugreport.
        let error = internal_error("a single step replayed", Some(7));
        assert_eq!(
            error.to_string(),
            "internal error: a single step replayed came to Some(7). This is a gap in Rewind, \
             not in the program: please report it with `rewind bugreport <run>`"
        );
    }
}
