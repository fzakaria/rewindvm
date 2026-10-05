//! The `rewind` subcommands about this machine: its performance counters,
//! what rewind needs of it, and the completions and man pages the packages
//! install.

use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::CommandFactory;
use rewind_core::{Guest, Home};

use crate::{Cli, Generate, PmuAction, doctor};

pub fn doctor(home: &Home) -> Result<ExitCode> {
    println!("rewind {}", rewind_core::VERSION);
    let found = doctor::findings(home);
    for finding in &found {
        println!("{}", finding.line());
    }
    let failed = found.iter().any(|f| f.level == doctor::Level::Fail);
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// The arguments of `rewind pmu`.
#[derive(clap::Args)]
pub struct PmuArgs {
    #[command(subcommand)]
    pub(crate) action: PmuAction,
}

pub fn pmu(home: &Home, args: PmuArgs) -> Result<ExitCode> {
    let PmuArgs { action } = args;
    let vendor = rewind_core::pmu::Vendor::detect()?;
    match action {
        PmuAction::Enable => pmu_enable(),
        PmuAction::Status => {
            println!("cpu: {vendor}");
            let needs = vendor.needs_workaround();
            let known = needs && rewind_core::pmu::workaround_known();
            if needs {
                let state = match (rewind_core::pmu::workaround_set(), known) {
                    (Some(true), _) => "set",
                    (Some(false), _) => "not set",
                    (None, true) => "set by `rewind pmu enable` this boot",
                    (None, false) => "not known to be set this boot",
                };
                println!("amd workaround (MSR 0xc0011020 bit 54): {state}");
            }
            if let Some(p) = rewind_core::pmu::perf_event_paranoid() {
                println!("perf_event_paranoid: {p}");
            }
            let guest = Guest::from_env()?;
            let t = rewind_core::pmu::selftest(&guest)?;
            rewind_core::pmu::remember(home, known, t.exact)?;
            let usable = rewind_core::pmu::counter_usable(needs, known, t.exact);
            println!(
                "self-test: {} and {} branches, {}",
                t.counts[0],
                t.counts[1],
                if t.exact {
                    "exact at every exit"
                } else {
                    "NOT exact"
                }
            );
            // On AMD an exact self-test without the workaround is
            // luck, and says so.
            if t.exact && !usable {
                println!(
                    "the self-test agreed this time, but without the workaround this \
                         CPU's counter is not reliably exact; set it with `sudo rewind pmu enable`"
                );
            }
            if usable {
                println!("runs will use counter time");
            } else {
                println!(
                    "runs will use exit time; see {}",
                    rewind_core::pmu::DOCS_URL
                );
            }
            Ok(if usable {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
    }
}

/// The arguments of `rewind generate`.
#[derive(clap::Args)]
pub struct GenerateArgs {
    #[command(subcommand)]
    pub(crate) what: Generate,
}

pub fn generate(args: GenerateArgs) -> Result<ExitCode> {
    let GenerateArgs { what } = args;
    write_generated(what)
}

/// `rewind generate`: completions on standard output, or man pages in a
/// directory.
fn write_generated(what: Generate) -> Result<ExitCode> {
    match what {
        Generate::Completions { shell } => completions(shell, &mut std::io::stdout()),
        Generate::Man { dir } => {
            std::fs::create_dir_all(&dir)?;
            clap_mangen::generate_to(Cli::command(), &dir)
                .with_context(|| format!("writing man pages to {}", dir.display()))?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `shell`'s completions for every command and option, written to `out`.
fn completions(shell: clap_complete::Shell, out: &mut dyn std::io::Write) {
    clap_complete::generate(shell, &mut Cli::command(), "rewind", out);
}

/// `rewind pmu enable`: sets the AMD branch counter workaround on every
/// CPU, until reboot, where the CPU needs it.
pub fn pmu_enable() -> Result<ExitCode> {
    let vendor = rewind_core::pmu::Vendor::detect()?;
    if !vendor.needs_workaround() {
        println!("{vendor}: no workaround needed");
        return Ok(ExitCode::SUCCESS);
    }
    let n = rewind_core::pmu::enable_workaround()?;
    println!("set the branch counter workaround on {n} CPUs, until reboot");
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    // The completions and man pages the packages install.
    use super::*;

    #[test]
    fn completions_and_man_pages_cover_every_command() {
        // The man pages are one for rewind and one for each command it
        // shows; bash's completions name a command.
        let dir = std::env::temp_dir().join(format!("rewind-man-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_generated(Generate::Man { dir: dir.clone() }).unwrap();
        for page in ["rewind.1", "rewind-fork.1", "rewind-show.1"] {
            assert!(dir.join(page).exists(), "{page}");
        }
        assert!(!dir.join("rewind-generate.1").exists());
        std::fs::remove_dir_all(&dir).unwrap();

        let mut bash = Vec::new();
        completions(clap_complete::Shell::Bash, &mut bash);
        assert!(String::from_utf8(bash).unwrap().contains("fork"));
    }
}
