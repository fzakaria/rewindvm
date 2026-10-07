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

pub mod answers;
pub mod archive;
pub mod bookmarks;
pub mod compare;
pub mod describe;
pub mod engine;
pub mod examples;
pub mod family;
pub mod history;
pub mod jobs;
pub mod lanes;
pub mod license;
pub mod memo;
pub mod model;
pub mod request;
pub mod run;
pub mod search;
pub mod selection;
pub mod shown_line;
pub mod sideways;
pub mod source;
pub mod step_entry;
pub mod stride;
pub mod sweep;
pub mod syntax;
pub mod terminal;
pub mod theme;
pub mod tour;
pub mod ui;
pub mod view;
pub mod viewer;
