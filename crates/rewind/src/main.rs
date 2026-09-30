//! The `rewind` command.

use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use clap::{Parser, Subcommand};
use rewind_vmm::{Config, Machine, Observer};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Boot a kernel and initramfs and print what the guest reports.
    Boot {
        #[arg(long)]
        kernel: PathBuf,
        #[arg(long)]
        initrd: Option<PathBuf>,
        #[arg(long)]
        image: Option<PathBuf>,
        /// Guest memory in MiB.
        #[arg(long, default_value_t = 512)]
        mem: u64,
        /// Extra kernel command line arguments.
        #[arg(long, default_value = "")]
        append: String,
        /// Stop at this step.
        #[arg(long)]
        until: Option<u64>,
    },
}

/// The command line every guest boots with. Each argument keeps the kernel
/// away from something that would wait on hardware time.
const BASE_CMDLINE: &str = "nolapic_timer lpj=1000000 panic=-1 rdinit=/init";

/// Prints what the guest reports, and folds every record and the step it
/// arrived on into a hash, so two runs can be compared at a glance.
struct Print {
    hash: u64,
}

impl Observer for Print {
    fn record(&mut self, step: u64, record: &[u8]) {
        if let Ok(path) = std::env::var("REWIND_TRACE") {
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap();
            let _ = writeln!(f, "{step} {record:02x?}");
        }
        for b in step.to_le_bytes().iter().chain(record) {
            self.hash = (self.hash ^ *b as u64).wrapping_mul(0x100_0000_01b3);
        }
        let kind = u16::from_le_bytes([record[4], record[5]]);
        let data = &record[20..];
        match kind {
            1 => {
                let _ = std::io::stderr().write_all(data);
            }
            2 => {
                let _ = std::io::stdout().write_all(data);
            }
            _ => eprintln!("[{step}] record kind {kind}, {} bytes", data.len()),
        }
    }

    fn serial(&mut self, _step: u64, byte: u8) {
        let _ = std::io::stderr().write_all(&[byte]);
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Boot {
            kernel,
            initrd,
            image,
            mem,
            append,
            until,
        } => {
            let config = Config {
                kernel,
                initrd: match initrd {
                    Some(p) => std::fs::read(p)?,
                    None => Vec::new(),
                },
                image,
                mem_bytes: mem << 20,
                cmdline: format!("{BASE_CMDLINE} {append}"),
                seed: [0; 32],
                epoch: 1,
                quantum: 1000,
            };
            let mut m = Machine::boot(&config)?;
            if std::env::var_os("REWIND_PROFILE").is_some() {
                m.profile = Some(Default::default());
            }
            let start = Instant::now();
            let mut print = Print {
                hash: 0xcbf2_9ce4_8422_2325,
            };
            let outcome = m.run(until, &mut print)?;
            eprintln!(
                "\n{outcome:?} at step {} ({} ns virtual) after {:?}, trace {:016x}",
                m.step(),
                m.now(),
                start.elapsed(),
                print.hash
            );
            if let Some(profile) = &m.profile {
                let mut top: Vec<_> = profile.iter().collect();
                top.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
                for ((port, rip), n) in top.iter().take(15) {
                    eprintln!("{n:>10} port {port:#06x} rip {rip:#x}");
                }
            }
        }
    }
    Ok(())
}
