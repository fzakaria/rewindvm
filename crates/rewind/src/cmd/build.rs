//! `rewind build`: one build, as nix-daemon describes it, run in the VM
//! with its outputs copied back into the store.
//!
//! Nix hands a build to this command two ways. Its external-builders
//! setting starts the command in place of the Nix sandbox, for every
//! derivation whose `system` the setting names. Or a derivation's own
//! builder, running inside the Nix sandbox, writes the same description of
//! the derivation it wraps and starts the command (nix/in-rewind.sh).
//! Either way the builder becomes the job `rewind nix` would make of it.
//!
//! The machine comes from the derivation, never from this machine: the
//! `rewindFlags` attribute holds the options `rewind run` takes, so the
//! derivation's hash covers the clock, the seed and the schedule as it
//! covers the compiler.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::Parser;
use rewind_core::inspect::{self, Sent};
use rewind_core::nix::Description;
use rewind_core::pmu::Vendor;
use rewind_core::tree::Unpack;
use rewind_core::{Guest, Home, Run};

use super::record::{self, MachineArgs, Report, Workload};

/// The arguments of `rewind build`.
#[derive(clap::Args)]
pub(crate) struct BuildArgs {
    /// The build's description: the JSON file Nix's external-builders
    /// setting starts its program with, version 1.
    pub(crate) description: PathBuf,
    /// Say the build is ready the way Nix's external-builders setting
    /// waits to hear, before anything else is printed.
    ///
    /// Give it in the setting's `args`, after `build`. Everything the
    /// command prints after is the build's log.
    #[arg(long)]
    pub(crate) external_builder: bool,
}

/// The derivation attribute that holds the machine: the options `rewind
/// run` takes for one, and --vendor, separated by spaces.
const FLAGS_ATTRIBUTE: &str = "rewindFlags";

/// What nix-daemon waits to read from an external builder before taking
/// the rest of what the builder prints as the build's log.
const READY: &str = "\x02\n";

/// How a `system` that asks for Rewind starts; the CPU vendor follows.
const SYSTEM_PREFIX: &str = "x86_64-rewind-";

/// The directory, in the build's scratch directory, the build's run is
/// kept in when REWIND_HOME names no other.
const SCRATCH_HOME: &str = "rewind";

/// The CPU vendors a build can be for.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum VendorArg {
    Amd,
    Intel,
}

impl VendorArg {
    /// The vendor as a `system` and --vendor spell it.
    fn name(self) -> &'static str {
        match self {
            VendorArg::Amd => "amd",
            VendorArg::Intel => "intel",
        }
    }

    /// The vendor as people write it.
    fn words(self) -> &'static str {
        match self {
            VendorArg::Amd => "AMD",
            VendorArg::Intel => "Intel",
        }
    }

    /// Whether this machine's CPU, `vendor`, is of this vendor.
    fn is(self, vendor: &Vendor) -> bool {
        matches!(
            (self, vendor),
            (VendorArg::Amd, Vendor::Amd { .. }) | (VendorArg::Intel, Vendor::Intel)
        )
    }
}

/// A derivation's `rewindFlags` attribute, read as a command line's
/// options are.
#[derive(Parser)]
#[command(
    no_binary_name = true,
    disable_help_flag = true,
    disable_version_flag = true
)]
struct MachineFlags {
    #[command(flatten)]
    machine: MachineArgs,
    /// The CPU vendor the build is for. Runs of the same inputs are the
    /// same on every machine of one vendor and differ between vendors, so
    /// a build refuses a machine of another.
    #[arg(long, value_enum)]
    vendor: Option<VendorArg>,
}

pub(crate) fn build(args: BuildArgs) -> Result<ExitCode> {
    let BuildArgs {
        description: path,
        external_builder,
    } = args;

    // nix-daemon takes nothing an external builder prints as the build's
    // log until this line, a failure's reason included, so the line goes
    // out before anything can fail.
    if external_builder {
        let mut out = std::io::stdout();
        out.write_all(READY.as_bytes())?;
        out.flush()?;
    }

    // The build and its machine, both from the derivation.
    let description = Description::read(&path)?;
    let flags = machine_flags(&description)?;
    let vendor = vendor_asked(&description.system, flags.vendor)?;
    if let Some(vendor) = vendor {
        refuse_other_vendor(vendor, &Vendor::detect()?)?;
    }

    // The run, with the builder's output as the build's log.
    let home = home_for(&description)?;
    let _in_use = home.in_use()?;
    let guest = Guest::from_env()?;
    let workload = Workload::Described {
        description: Box::new(description.clone()),
        path,
    };
    let machine = flags.machine.read_at_exit();
    let run = record::run_workload(&home, &guest, &workload, &machine, Report::Text)?;
    let status = run.manifest.outcome.as_ref().and_then(|o| o.status);
    if status != Some(0) {
        return Ok(record::exit_status(&run));
    }

    copy_outputs(&home, &run, &description)?;
    Ok(ExitCode::SUCCESS)
}

/// The machine `description`'s derivation asks for, from its
/// `rewindFlags` attribute. An error when the derivation has none, when
/// the attribute holds anything but the machine's options, or when it
/// leaves out an option whose default would come from this machine or
/// this day, which the derivation's hash would not cover.
fn machine_flags(description: &Description) -> Result<MachineFlags> {
    let name = &description.build.name;
    let Some(words) = description.attribute(FLAGS_ATTRIBUTE) else {
        bail!(
            "{name} has no {FLAGS_ATTRIBUTE} attribute; a derivation built in Rewind names its \
             machine there, as in {FLAGS_ATTRIBUTE} = \"--epoch 1790985600 --cpu v3-compat \
             --clock exits\""
        );
    };
    let flags = match MachineFlags::try_parse_from(words.split_whitespace()) {
        Ok(flags) => flags,
        Err(e) => bail!(
            "{name}'s {FLAGS_ATTRIBUTE}, {words:?}: {}",
            first_line(&e.to_string())
        ),
    };

    let unpinned = flags.machine.unpinned();
    if !unpinned.is_empty() {
        bail!(
            "{name}'s {FLAGS_ATTRIBUTE}, {words:?}, must also give {}: without, a run takes \
             today's date, this machine's CPU model or the clock this machine can keep, and \
             the derivation's hash covers none of them",
            unpinned.join(", ")
        );
    }
    Ok(flags)
}

/// What clap says is wrong with a command line, without the usage that
/// follows it.
fn first_line(error: &str) -> &str {
    let line = error.lines().next().unwrap_or(error);
    line.strip_prefix("error: ").unwrap_or(line)
}

/// The vendor a build is for: the one its `system` ends with when the
/// system asks for Rewind, as `x86_64-rewind-amd` does, and the one
/// --vendor names. None when neither names one, and an error when they
/// name two.
fn vendor_asked(system: &str, flag: Option<VendorArg>) -> Result<Option<VendorArg>> {
    use clap::ValueEnum;

    let Some(name) = system.strip_prefix(SYSTEM_PREFIX) else {
        return Ok(flag);
    };
    let Some(platform) = VendorArg::value_variants()
        .iter()
        .copied()
        .find(|v| v.name() == name)
    else {
        bail!(
            "the system {system} names no CPU vendor rewind knows: {SYSTEM_PREFIX}amd or {SYSTEM_PREFIX}intel"
        );
    };
    match flag {
        Some(flag) if flag != platform => bail!(
            "the system {system} is for {} and --vendor says {}",
            platform.name(),
            flag.name()
        ),
        _ => Ok(Some(platform)),
    }
}

/// Refuses to build for `asked` on a machine whose CPU is `host`'s. The
/// guest's fixed CPU model keeps the host's vendor, so the two vendors
/// make different runs, and outputs, of one derivation.
fn refuse_other_vendor(asked: VendorArg, host: &Vendor) -> Result<()> {
    if asked.is(host) {
        return Ok(());
    }
    bail!(
        "this build is for {} CPUs and this machine's is {host}; a run differs between \
         vendors, so the output would not be the one the derivation names",
        asked.words()
    )
}

/// The home a build's run is kept in: REWIND_HOME when set, as for any
/// command, and otherwise a directory in the build's scratch directory,
/// which nix-daemon removes with the build. Never the user's own home: a
/// build under nix-daemon runs as a build user, or in a sandbox, that has
/// none.
fn home_for(description: &Description) -> Result<Home> {
    if std::env::var_os(rewind_core::home::ENV_HOME).is_some() {
        return Home::open();
    }
    Home::at(description.scratch.join(SCRATCH_HOME))
}

/// Copies each output out of the VM as `run` left it when the builder
/// exited, into the store on this machine. The copy comes from a fork of
/// the run, so reading the outputs is no part of the build.
fn copy_outputs(home: &Home, run: &Run, description: &Description) -> Result<()> {
    let end = run.last_step()?;
    for (name, path) in &description.build.outputs {
        let file_name = path
            .file_name()
            .with_context(|| format!("output {name} has no name: {}", path.display()))?;
        let mut unpack = Unpack::new(description.real_store.join(file_name));
        let in_vm = path.to_string_lossy();
        match inspect::tree(home, run, end, None, &in_vm, &mut unpack)
            .with_context(|| format!("copying output {name} out of the VM"))?
        {
            Sent::Whole => unpack.finish()?,
            Sent::NotFound(_) => {
                bail!("the builder exited 0 without making output {name}, {in_vm}")
            }
            Sent::Failed(message) => bail!("copying output {name} out of the VM: {message}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    // The machine a derivation names, read from descriptions written by
    // hand into a temporary directory, with no VM.
    use serde_json::json;

    use super::*;

    /// A description of a build named greet for `system`, with `flags` as
    /// its rewindFlags attribute when given, read from a build directory
    /// of the test's own.
    fn described(test: &str, system: &str, flags: Option<&str>) -> Description {
        let dir = std::env::temp_dir().join(format!("rewind-build-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("build")).unwrap();
        let mut env = json!({"name": "greet"});
        if let Some(flags) = flags {
            env[FLAGS_ATTRIBUTE] = json!(flags);
        }
        let description = Description::parse(&json!({
            "version": 1,
            "builder": "/bin/sh",
            "args": [],
            "env": env,
            "inputPaths": [],
            "outputs": {"out": "/nix/store/0000000000000000000000000000000b-greet"},
            "storeDir": "/nix/store",
            "realStoreDir": "/nix/store",
            "system": system,
            "tmpDir": dir.join("build"),
            "tmpDirInSandbox": "/build",
            "topTmpDir": dir,
        }))
        .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        description
    }

    /// Why `flags` as a derivation's rewindFlags is refused.
    fn refusal(test: &str, flags: Option<&str>) -> String {
        match machine_flags(&described(test, "x86_64-linux", flags)) {
            Ok(_) => panic!("{flags:?} was taken"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn a_derivation_names_its_whole_machine() {
        // A derivation's rewindFlags with the epoch, the CPU model and
        // the clock is taken, the vendor with it. One with no attribute,
        // or without any of the three, is refused, naming what is
        // missing: each would otherwise come from this day or this
        // machine. So is an option no machine has.
        let whole = "--epoch 1790985600 --seed 3 --cpu v3-compat --clock exits --vendor amd";
        let flags = machine_flags(&described("whole", "x86_64-linux", Some(whole))).unwrap();
        assert_eq!(flags.vendor, Some(VendorArg::Amd));
        assert!(flags.machine.unpinned().is_empty());

        let none = refusal("none", None);
        assert!(none.contains("greet has no rewindFlags"), "{none}");
        let bare = refusal("bare", Some("--seed 3"));
        assert!(bare.contains("--epoch, --cpu, --clock"), "{bare}");
        let auto = refusal("auto", Some("--epoch 5 --cpu v3 --clock auto"));
        assert!(auto.contains("must also give --clock"), "{auto}");
        assert!(!auto.contains("--epoch,"), "{auto}");
        let unknown = refusal("unknown", Some("--epoch 5 --root /"));
        assert!(
            unknown.contains("unexpected argument '--root'"),
            "{unknown}"
        );
        assert!(!unknown.contains("Usage"), "{unknown}");
    }

    #[test]
    fn the_vendor_comes_from_the_system_or_the_flag() {
        // A system that asks for Rewind ends with the vendor, and an
        // ordinary one leaves the vendor to --vendor. The two together
        // must agree, and a system for a vendor rewind does not know is
        // refused. A build for one vendor is refused on the other's CPU.
        let amd = Some(VendorArg::Amd);
        let intel = Some(VendorArg::Intel);
        assert_eq!(vendor_asked("x86_64-rewind-amd", None).unwrap(), amd);
        assert_eq!(vendor_asked("x86_64-rewind-intel", intel).unwrap(), intel);
        assert_eq!(vendor_asked("x86_64-linux", intel).unwrap(), intel);
        assert_eq!(vendor_asked("x86_64-linux", None).unwrap(), None);
        assert!(vendor_asked("x86_64-rewind-amd", intel).is_err());
        assert!(vendor_asked("x86_64-rewind-via", None).is_err());

        let zen = Vendor::Amd { family: 25 };
        refuse_other_vendor(VendorArg::Amd, &zen).unwrap();
        refuse_other_vendor(VendorArg::Intel, &Vendor::Intel).unwrap();
        let err = refuse_other_vendor(VendorArg::Intel, &zen).unwrap_err();
        assert!(err.to_string().contains("for Intel CPUs"), "{err}");
        assert!(refuse_other_vendor(VendorArg::Amd, &Vendor::Other("x".into())).is_err());
    }
}
