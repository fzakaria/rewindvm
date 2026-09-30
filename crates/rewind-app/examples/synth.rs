//! Writes two synthetic runs for developing the app: a failing build of a
//! C library and a passing run that diverges from it at a thread race.
//!
//!     cargo run --example synth -- <out-dir> [--small]
//!
//! then
//!
//!     cargo run -- <out-dir>/fail --compare <out-dir>/pass

use std::path::PathBuf;
use std::process::ExitCode;

use rewind_app::synth::{SynthConfig, Variant, write_run};

fn main() -> ExitCode {
    let mut out: Option<PathBuf> = None;
    let mut small = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--small" => small = true,
            path => out = Some(PathBuf::from(path)),
        }
    }
    let Some(out) = out else {
        eprintln!("usage: synth <out-dir> [--small]");
        return ExitCode::FAILURE;
    };

    let config = |variant| {
        if small {
            SynthConfig::small(variant)
        } else {
            SynthConfig::large(variant)
        }
    };
    let runs = [
        ("fail", "run #3", Variant::Failing),
        ("pass", "run #2", Variant::Passing),
    ];
    for (dir, name, variant) in runs {
        let path = out.join(dir);
        if let Err(e) = write_run(&path, &config(variant), name) {
            eprintln!("synth: writing {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
        println!("{}", path.display());
    }
    ExitCode::SUCCESS
}
