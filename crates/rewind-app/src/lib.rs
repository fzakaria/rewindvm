//! The Rewind desktop app: a scrubber over a recorded run.
//!
//! The model (`model`, `describe`, `run`) turns a trace into tables the UI
//! reads per frame; `ui` draws them with GPUI; `engine` runs the engine's
//! commands for the actions that need the machine itself, whose answers
//! `viewer` and `source` lay out, and `syntax` highlights; `terminal` runs
//! the shell and gdb in a pty; `selection` is the text selection over every
//! text surface; `request` numbers background work so a late answer is
//! told apart, and `jobs` runs the engine's calls, which wait on a child
//! process, on threads of their own.

pub mod address;
pub mod answers;
pub mod archive;
pub mod blame;
pub mod blame_jobs;
pub mod bookmarks;
pub mod clock;
pub mod compare;
pub mod density;
pub mod describe;
pub mod diff_view;
pub mod engine;
pub mod examples;
pub mod family;
#[cfg(test)]
mod frame_cost;
pub mod history;
pub mod jobs;
pub mod lanes;
pub mod license;
pub mod listing;
pub mod memo;
pub mod model;
pub mod proc_card;
pub mod request;
pub mod run;
pub mod search;
pub mod selection;
pub mod settings;
pub mod shown_line;
pub mod sideways;
pub mod source;
pub mod step_entry;
pub mod stride;
pub mod sweep;
pub mod syntax;
#[cfg(not(target_family = "wasm"))]
pub mod terminal;
pub mod theme;
pub mod tour;
pub mod ui;
pub mod view;
pub mod viewer;
pub mod waiting;
#[cfg(target_family = "wasm")]
pub mod web;
