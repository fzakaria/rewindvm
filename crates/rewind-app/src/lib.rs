//! The Rewind desktop app: a scrubber over a recorded run.
//!
//! The model (`model`, `describe`, `run`) turns a trace into tables the UI
//! reads per frame; `ui` draws them with GPUI; `engine` runs the engine's
//! commands for the actions that need the machine itself, whose answers
//! `viewer` and `source` lay out, and `syntax` highlights; `terminal` runs
//! the shell and gdb in a pty; `selection` is the text selection over every
//! text surface; `synth` writes synthetic runs for development and tests.

pub mod archive;
pub mod describe;
pub mod engine;
pub mod examples;
pub mod family;
pub mod license;
pub mod model;
pub mod run;
pub mod selection;
pub mod sideways;
pub mod source;
pub mod syntax;
pub mod synth;
pub mod terminal;
pub mod theme;
pub mod tour;
pub mod ui;
pub mod viewer;
