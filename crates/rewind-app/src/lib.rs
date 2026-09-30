//! The Rewind desktop app: a scrubber over a recorded run.
//!
//! The model (`model`, `describe`, `run`) turns a trace into tables the UI
//! reads per frame; `ui` draws them with GPUI; `engine` runs the engine's
//! commands for the actions that need the machine itself; `synth` writes
//! synthetic runs for development and tests.

pub mod archive;
pub mod describe;
pub mod engine;
pub mod examples;
pub mod license;
pub mod model;
pub mod run;
pub mod synth;
pub mod theme;
pub mod tour;
pub mod ui;
