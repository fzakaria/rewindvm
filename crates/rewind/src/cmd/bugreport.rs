//! `rewind bugreport`: what a bug report about a run needs, gathered into
//! a directory, and a link that opens a GitHub issue filled in with it.
//!
//! The report names this build of rewind, the host's CPU, kernel and KVM,
//! what `rewind doctor` finds, the run's inputs, and what replaying the
//! run finds, with this machine's home directory and rewind's home left
//! out of every path. Beside it goes the run's trace-only .rwd, small
//! enough to attach, and with --replayable the export another machine can
//! replay, which carries the guest's kernel and memory and is far past
//! what an issue takes. A link cannot attach a file, and rewind uploads
//! nothing: the person attaches what they choose to share.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result};
use rewind_core::{Home, Run, export};
use rewind_trace::ending;

use crate::{RUN_HELP, RUN_LONG_HELP, color, show};

/// Where issues about rewind go.
pub(crate) const NEW_ISSUE: &str = "https://github.com/fzakaria/rewindvm/issues/new";

/// What a replay's verdict says when it found a difference.
const DIFFERENT: &str = "DIFFERENT";

/// The largest file GitHub takes as an issue's attachment, in bytes.
/// GitHub states the limit as "25 MB" without saying which megabyte, so
/// this is the decimal reading, the smaller of the two: a file under it
/// fits either way.
const ATTACHMENT_MAX_BYTES: u64 = 25 * 1000 * 1000;

/// The report's own file in the directory.
const REPORT: &str = "report.md";

/// How much of the report the link carries in the issue's body. Browsers
/// and GitHub refuse links much past 8 KiB once encoded.
const BODY_IN_LINK: usize = 5000;

/// What stands in the report for rewind's home and this machine's home
/// directory, which name the person.
const REWIND_HOME_WORDS: &str = "$REWIND_HOME";
const HOME_WORDS: &str = "~";

/// The lines of /proc/cpuinfo that say which CPU the host is.
const CPU_FIELDS: [&str; 6] = [
    "vendor_id",
    "model name",
    "cpu family",
    "model",
    "stepping",
    "microcode",
];

/// The KVM module parameters that change what a VM sees.
const KVM_PARAMETERS: [&str; 2] = [
    "/sys/module/kvm/parameters/enable_pmu",
    "/sys/module/kvm/parameters/ignore_msrs",
];

/// The arguments of `rewind bugreport`.
#[derive(clap::Args)]
pub(crate) struct BugreportArgs {
    #[arg(help = RUN_HELP, long_help = RUN_LONG_HELP)]
    pub(crate) run: String,
    /// The directory to write; rewind-bugreport-<id> by default.
    #[arg(long, short)]
    pub(crate) output: Option<PathBuf>,
    /// Also find the instruction the replay went another way at, with
    /// `rewind replay --bisect`, and include what it says.
    #[arg(long)]
    pub(crate) bisect: bool,
    /// Also write the export another machine can replay, with the VM's
    /// kernel, input image and keyframes: tens of megabytes or more, too
    /// large to attach to an issue, for when a maintainer asks for it.
    #[arg(long, conflicts_with = "no_run")]
    pub(crate) replayable: bool,
    /// Leave out the run's .rwd, which holds its command, environment,
    /// events and output: the report then says only what rewind found.
    #[arg(long)]
    pub(crate) no_run: bool,
    /// Open the issue's page in the browser, filled in with the report.
    #[arg(long)]
    pub(crate) open: bool,
}

pub(crate) fn bugreport(home: &Home, args: BugreportArgs) -> Result<ExitCode> {
    let BugreportArgs {
        run,
        output,
        bisect,
        replayable,
        no_run,
        open,
    } = args;
    let run = Run::find(home, &run)?;
    let id = run.manifest.id.to_string();
    let dir = output.unwrap_or_else(|| PathBuf::from(format!("rewind-bugreport-{id}")));
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let paint = color::stderr();

    // What rewind finds, each from a command of its own, without color.
    eprintln!("{} replaying run {id}", paint.prefix());
    let mut replay = vec!["replay".to_string(), id.clone()];
    if bisect {
        replay.push("--bisect".to_string());
    }
    let replayed = rewind_output(&replay)?;
    let doctor = rewind_output(&["doctor".to_string()])?;

    // The report, with the paths that name the person left out.
    let homes = Homes {
        rewind: home.root().to_path_buf(),
        user: std::env::var_os("HOME").map(PathBuf::from),
    };
    let text = homes.scrub(&report_text(&run, &replay.join(" "), &replayed, &doctor));
    let report_path = dir.join(REPORT);
    std::fs::write(&report_path, &text)
        .with_context(|| format!("writing {}", report_path.display()))?;
    eprintln!("{} wrote {}", paint.prefix(), report_path.display());

    // The run itself: the trace-only export to attach, and the replayable
    // one when asked for, to share some other way.
    let trace_only = dir.join(format!("{id}.rwd"));
    if !no_run {
        export::export(home, &run, export::Contents::View, &trace_only)?;
        let size = std::fs::metadata(&trace_only)?.len();
        eprintln!(
            "{} wrote {} ({}), the run's events and output",
            paint.prefix(),
            trace_only.display(),
            show::size(size)
        );
    }
    let full = dir.join(format!("{id}-replayable.rwd"));
    if replayable {
        eprintln!("{} exporting run {id} to replay", paint.prefix());
        export::export(home, &run, export::Contents::Replayable, &full)?;
        let size = std::fs::metadata(&full)?.len();
        eprintln!(
            "{} wrote {} ({}), with the VM's kernel, input image and memory",
            paint.prefix(),
            full.display(),
            show::size(size)
        );
    }

    let link = issue_link(&run, &text);
    println!("open an issue with the report filled in: {link}");
    if !no_run {
        let size = std::fs::metadata(&trace_only)?.len();
        println!("{}", attach_words(&trace_only, size));
    }
    if replayable {
        println!(
            "{} is too large to attach: share it some other way if a maintainer asks for it",
            full.display()
        );
    }
    if open {
        open_link(&link);
    }
    Ok(ExitCode::SUCCESS)
}

/// What to do with the trace-only export of `size` bytes at `path`: GitHub
/// takes an attachment up to [`ATTACHMENT_MAX_BYTES`], and not .rwd by name, so
/// it goes zipped, or elsewhere when it is too large even so.
fn attach_words(path: &Path, size: u64) -> String {
    let path = path.display();
    if size > ATTACHMENT_MAX_BYTES {
        return format!(
            "{path} is {} and an issue takes at most {}: share it some other way and link it",
            show::size(size),
            show::size(ATTACHMENT_MAX_BYTES)
        );
    }
    format!(
        "then attach {path} to it, zipped, since GitHub does not take .rwd by name. It holds \
         the run's command and environment, everything its programs printed, and the names \
         of the files they opened: look before you share it"
    )
}

/// The directories whose paths name the person: rewind's home, and the
/// home directory of the user running it.
struct Homes {
    rewind: PathBuf,
    user: Option<PathBuf>,
}

impl Homes {
    /// `text` with rewind's home written as $REWIND_HOME and the user's
    /// home directory as ~, rewind's first since it is often inside the
    /// other.
    fn scrub(&self, text: &str) -> String {
        let mut text = text.replace(&*self.rewind.to_string_lossy(), REWIND_HOME_WORDS);
        if let Some(user) = self.user.as_ref().filter(|u| u.as_os_str().len() > 1) {
            text = text.replace(&*user.to_string_lossy(), HOME_WORDS);
        }
        text
    }
}

/// What this rewind prints for `args`, its standard output then its
/// standard error, without color, whatever it exits with.
fn rewind_output(args: &[String]) -> Result<String> {
    let exe = std::env::current_exe().context("finding rewind's own program")?;
    let out = Command::new(exe)
        .args(["--color", "never", "--progress", "off"])
        .args(args)
        .output()
        .with_context(|| format!("running rewind {}", args.join(" ")))?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok(text)
}

/// The clock a run's virtual time followed, as the docs name it.
fn clock_words(clock: rewind_vmm::ClockSource) -> &'static str {
    match clock {
        rewind_vmm::ClockSource::Exits => "exit time",
        rewind_vmm::ClockSource::Branches(_) => "counter time",
    }
}

/// The report, in Markdown, as it goes in report.md and the issue.
fn report_text(run: &Run, command: &str, replayed: &str, doctor: &str) -> String {
    let m = &run.manifest;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "## What happened\n\n_Replace this line: what you ran, and what you expected._\n"
    );
    let _ = writeln!(
        out,
        "## Replay\n\n```console\n$ rewind {command}\n{replayed}```\n"
    );
    let _ = writeln!(out, "## The run\n");
    let _ = writeln!(out, "- id: {}", m.id);
    let _ = writeln!(out, "- recorded by: rewind {}", m.recorded_by);
    let _ = writeln!(out, "- reported by: rewind {}", rewind_core::VERSION);
    let _ = writeln!(out, "- CPU model: {}", m.spec.cpu.cli_name());
    let _ = writeln!(out, "- clock: {}", clock_words(m.spec.clock));
    let _ = writeln!(out, "- kernel: {}", m.spec.kernel.display());
    if let Some(o) = &m.outcome {
        let _ = writeln!(
            out,
            "- ended: {} at step {} ({})",
            ending::label(o, &m.verdict),
            o.step,
            o.stop
        );
    }
    let _ = writeln!(out, "\n## The host\n");
    for line in host_lines() {
        let _ = writeln!(out, "- {line}");
    }
    let _ = writeln!(out, "\n## rewind doctor\n\n```console\n{doctor}```");
    out
}

/// The host's CPU, kernel and KVM, a line each.
fn host_lines() -> Vec<String> {
    let mut lines = Vec::new();
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    for field in CPU_FIELDS {
        let value = cpuinfo.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            (name.trim() == field).then(|| value.trim().to_string())
        });
        if let Some(value) = value {
            lines.push(format!("{field}: {value}"));
        }
    }
    if let Ok(release) = std::fs::read_to_string("/proc/sys/kernel/osrelease") {
        lines.push(format!("Linux {}", release.trim()));
    }
    for path in KVM_PARAMETERS {
        if let Ok(value) = std::fs::read_to_string(path) {
            let name = Path::new(path).file_name().unwrap_or_default();
            lines.push(format!("kvm {}: {}", name.to_string_lossy(), value.trim()));
        }
    }
    lines
}

/// The link to a new issue about `run` with `text` as its body, cut short
/// to fit in a link when it is long.
fn issue_link(run: &Run, text: &str) -> String {
    let title = if text.contains(DIFFERENT) {
        format!("Run {} does not replay exactly", run.manifest.id)
    } else {
        format!("A bug report on run {}", run.manifest.id)
    };
    let body = if text.len() > BODY_IN_LINK {
        let mut cut = BODY_IN_LINK;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        format!(
            "{}\n\n_The rest is in {REPORT}; please attach it._",
            &text[..cut]
        )
    } else {
        text.to_string()
    };
    format!(
        "{NEW_ISSUE}?title={}&body={}",
        percent_encode(&title),
        percent_encode(&body)
    )
}

/// `text` with every byte but the unreserved ones percent-encoded, as a
/// link's query takes it.
fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Opens `link` in the desktop's browser, saying so when it cannot.
fn open_link(link: &str) {
    if Command::new("xdg-open").arg(link).status().is_err() {
        eprintln!(
            "{} could not start xdg-open; open the link above yourself",
            color::stderr().prefix()
        );
    }
}

#[cfg(test)]
mod tests {
    // The pieces of a bug report that need no run, VM or browser: the
    // link's encoding, the paths left out, and what to do with the export.
    use super::*;

    #[test]
    fn a_link_query_encodes_everything_but_the_unreserved_bytes() {
        // Spaces, newlines, the query's own & and = and non-ASCII text are
        // encoded; letters, digits and -_.~ are not.
        assert_eq!(percent_encode("a b"), "a%20b");
        assert_eq!(percent_encode("x=1&y\n"), "x%3D1%26y%0A");
        assert_eq!(percent_encode("é"), "%C3%A9");
        assert_eq!(percent_encode("a-b_c.d~e"), "a-b_c.d~e");
    }

    #[test]
    fn the_homes_are_left_out_of_the_report() {
        // rewind's home inside the user's home becomes $REWIND_HOME, and
        // any other path under the user's home starts with ~. A user home
        // of / is not replaced, or every path would change.
        let homes = Homes {
            rewind: PathBuf::from("/home/ada/.local/share/rewind"),
            user: Some(PathBuf::from("/home/ada")),
        };
        assert_eq!(
            homes.scrub("disk /home/ada/.local/share/rewind takes 3 MB; src /home/ada/x.c"),
            "disk $REWIND_HOME takes 3 MB; src ~/x.c"
        );
        let root = Homes {
            rewind: PathBuf::from("/var/lib/rewind"),
            user: Some(PathBuf::from("/")),
        };
        assert_eq!(
            root.scrub("/var/lib/rewind /nix/store"),
            "$REWIND_HOME /nix/store"
        );
    }

    #[test]
    fn an_export_too_large_for_an_issue_is_shared_another_way() {
        // Under GitHub's limit the export is attached zipped; past it, it
        // is linked from elsewhere.
        let path = Path::new("r/abc.rwd");
        assert!(attach_words(path, 40 << 10).contains("zipped"));
        let large = attach_words(path, ATTACHMENT_MAX_BYTES + 1);
        assert!(large.contains("some other way"), "{large}");
    }

    #[test]
    fn the_clock_is_named_as_the_docs_name_it() {
        // Exit time and counter time, not the enum's own spelling.
        assert_eq!(clock_words(rewind_vmm::ClockSource::Exits), "exit time");
        assert_eq!(
            clock_words(rewind_vmm::ClockSource::Branches(
                rewind_vmm::CounterEvent::AmdRetiredConditionalBranches
            )),
            "counter time"
        );
    }
}
