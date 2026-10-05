//! The Rewind desktop app: a scrubber over a recorded run.
//!
//! The model (`model`, `describe`, `run`) turns a trace into tables the UI
//! reads per frame; `ui` draws them with GPUI; `engine` runs the engine's
//! commands for the actions that need the machine itself, whose answers
//! `viewer` and `source` lay out, and `syntax` highlights; `terminal` runs
//! the shell and gdb in a pty; `selection` is the text selection over every
//! text surface; `synth` writes synthetic runs for development and tests;
//! `request` numbers background work so a late answer is told apart.

pub mod answers;
pub mod archive;
pub mod bookmarks;
pub mod describe;
pub mod engine;
pub mod examples;
pub mod family;
pub mod history;
pub mod license;
pub mod memo;
pub mod model;
pub mod request;
pub mod run;
pub mod search;
pub mod selection;
pub mod sideways;
pub mod source;
pub mod step_entry;
pub mod stride;
pub mod syntax;
pub mod synth;
pub mod terminal;
pub mod theme;
pub mod tour;
pub mod ui;
pub mod viewer;
