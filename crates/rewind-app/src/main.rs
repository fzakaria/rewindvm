//! `rewind-app`: the desktop scrubber.
//!
//!     rewind-app [<run>] [--compare <run>] [--step <n>]
//!
//! A run is a run directory, a .rwd export, an http or https URL of one,
//! or a bare trace file.
//!
//! Without a run, the window opens on an empty state with an "Open run"
//! button.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use rewind_app::engine::CliEngine;
use rewind_app::run::Session;
use rewind_app::ui::{self, Launch};

const USAGE: &str = "usage: rewind-app [<run>] [--compare <run>] [--step <n>]\n\na run is a run directory, a .rwd export, an https URL of one, or a bare trace file";

/// The command line, parsed.
struct Args {
    run: Option<PathBuf>,
    compare: Option<PathBuf>,
    step: Option<u64>,
}

/// What the command line asked for.
enum Parsed {
    Run(Args),
    Help,
    Version,
}

/// Set to a level (error, warn, info, debug, trace) to print what GPUI and
/// the graphics stack log to standard error, for reporting problems.
const LOG_ENV: &str = "REWIND_APP_LOG";

/// Prints log records at or above a level to standard error.
struct StderrLog(log::LevelFilter);

impl log::Log for StderrLog {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= self.0
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            eprintln!(
                "rewind-app: {} {}: {}",
                record.level(),
                record.target(),
                record.args()
            );
        }
    }

    fn flush(&self) {}
}

/// Turns on logging when REWIND_APP_LOG names a level.
fn init_log() {
    let Some(level) = std::env::var(LOG_ENV)
        .ok()
        .and_then(|v| v.parse::<log::LevelFilter>().ok())
    else {
        return;
    };
    if log::set_boxed_logger(Box::new(StderrLog(level))).is_ok() {
        log::set_max_level(level);
    }
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Parsed, String> {
    let mut parsed = Args {
        run: None,
        compare: None,
        step: None,
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Parsed::Help),
            "-V" | "--version" => return Ok(Parsed::Version),
            "--compare" => {
                let value = args.next().ok_or("--compare needs a run")?;
                parsed.compare = Some(PathBuf::from(value));
            }
            "--step" => {
                let value = args.next().ok_or("--step needs a number")?;
                let step = value
                    .replace(',', "")
                    .parse()
                    .map_err(|_| format!("--step: not a step number: {value}"))?;
                parsed.step = Some(step);
            }
            flag if flag.starts_with("--") => return Err(format!("unknown option {flag}")),
            path => {
                if parsed.run.is_some() {
                    return Err(format!("more than one run given: {path}"));
                }
                parsed.run = Some(PathBuf::from(path));
            }
        }
    }
    if parsed.compare.is_some() && parsed.run.is_none() {
        return Err("--compare needs a run to compare with".into());
    }
    Ok(Parsed::Run(parsed))
}

fn main() -> ExitCode {
    init_log();
    let args = match parse(std::env::args().skip(1)) {
        Ok(Parsed::Run(args)) => args,
        Ok(Parsed::Help) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Ok(Parsed::Version) => {
            println!("rewind-app {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("rewind-app: {e}\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };

    // Runs named on the command line are read before the window opens, so
    // a bad path fails here with a message rather than in an empty window.
    for url in [&args.run, &args.compare]
        .into_iter()
        .flatten()
        .filter(|p| rewind_app::archive::is_url(p))
    {
        eprintln!("rewind-app: downloading {}", url.display());
    }
    let session = match &args.run {
        None => None,
        Some(run) => match Session::open(run, args.compare.as_deref()) {
            Ok(session) => Some(session),
            Err(e) => {
                eprintln!("rewind-app: {e:#}");
                return ExitCode::FAILURE;
            }
        },
    };

    ui::run(Launch {
        session,
        step: args.step,
        engine: Arc::new(CliEngine::from_env()),
    });
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    // Command line parsing: each test hands `parse` an argument list and
    // checks what the app would open, or that the list is refused.
    use super::*;

    fn parse_list(list: &[&str]) -> Result<Parsed, String> {
        parse(list.iter().map(|s| s.to_string()))
    }

    #[test]
    fn a_run_a_comparison_and_a_step() {
        // All three together, the step written with thousands separators.
        let Ok(Parsed::Run(args)) =
            parse_list(&["runs/fail", "--compare", "runs/pass", "--step", "1,204"])
        else {
            panic!("refused");
        };
        assert_eq!(args.run, Some(PathBuf::from("runs/fail")));
        assert_eq!(args.compare, Some(PathBuf::from("runs/pass")));
        assert_eq!(args.step, Some(1_204));
    }

    #[test]
    fn no_arguments_opens_the_empty_state() {
        // Nothing to open, which the window shows as its empty state.
        let Ok(Parsed::Run(args)) = parse_list(&[]) else {
            panic!("refused");
        };
        assert_eq!(args.run, None);
    }

    #[test]
    fn mistakes_are_refused() {
        // A comparison without a run, two runs, a step that is not a
        // number, and an option the app does not have.
        assert!(parse_list(&["--compare", "b"]).is_err());
        assert!(parse_list(&["a", "b"]).is_err());
        assert!(parse_list(&["a", "--step", "soon"]).is_err());
        assert!(parse_list(&["a", "--fast"]).is_err());
        assert!(matches!(parse_list(&["--help"]), Ok(Parsed::Help)));
    }
}
