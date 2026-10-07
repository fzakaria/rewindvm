//! `rewind doctor`: whether this machine can record and look inside runs,
//! and what to do where it cannot.

use std::path::{Path, PathBuf};
use std::process::Command;

use rewind_core::run::Unreadable;
use rewind_core::{Guest, Home, Run};

use crate::show;

/// How a finding stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Ok,
    /// Something works less well, or one command will not.
    Warn,
    /// Runs cannot be recorded or replayed.
    Fail,
}

impl Level {
    fn word(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Warn => "warn",
            Level::Fail => "fail",
        }
    }
}

/// One thing doctor looked at, and what it found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub level: Level,
    pub topic: &'static str,
    pub text: String,
}

impl Finding {
    fn new(level: Level, topic: &'static str, text: impl Into<String>) -> Finding {
        Finding {
            level,
            topic,
            text: text.into(),
        }
    }

    /// The finding as a line: its level and topic in columns, then what
    /// was found.
    pub fn line(&self) -> String {
        format!(
            "{:<LEVEL_WIDTH$}{:<TOPIC_WIDTH$}{}",
            self.level.word(),
            self.topic,
            self.text
        )
    }
}

/// The columns a line gives the level and the topic.
const LEVEL_WIDTH: usize = 6;
const TOPIC_WIDTH: usize = 12;

/// Everything doctor looks at, in the order it says it.
pub fn findings(home: &Home) -> Vec<Finding> {
    let mut found = vec![kvm()];
    let guest = Guest::from_env();
    found.push(match &guest {
        Ok(g) => Finding::new(
            Level::Ok,
            "guest",
            format!(
                "kernel {}, initramfs {}",
                g.kernel.display(),
                g.initrd.display()
            ),
        ),
        Err(e) => Finding::new(Level::Fail, "guest", format!("{e:#}")),
    });
    if let Ok(guest) = &guest {
        found.push(clock(home, guest));
    }
    found.extend([
        gdb(),
        debuginfod(),
        on_path(
            "nix",
            NIX_PROGRAM,
            "`rewind nix` and `rewind check` of a derivation need it",
        ),
        on_path(
            "mkfs.erofs",
            MKFS_EROFS,
            "packing a run's image, a Nix build's included, needs it",
        ),
        disk(home),
    ]);
    match Run::list_all(home) {
        Ok(listing) => found.push(unreadable(&listing.unreadable)),
        Err(e) => found.push(Finding::new(Level::Warn, "runs", format!("{e:#}"))),
    }
    found
}

/// The programs doctor looks for on PATH.
const NIX_PROGRAM: &str = "nix";
const MKFS_EROFS: &str = "mkfs.erofs";
const GDB_PROGRAM: &str = "gdb";

/// The debuginfod server `rewind gdb` starts, as gdb.rs finds it.
const ENV_DEBUGINFOD: &str = "REWIND_DEBUGINFOD";
const DEBUGINFOD_PROGRAM: &str = "nixseparatedebuginfod2";

fn kvm() -> Finding {
    match rewind_vmm::kvm::open() {
        Ok(_) => Finding::new(Level::Ok, "kvm", "/dev/kvm opens"),
        Err(e) => Finding::new(Level::Fail, "kvm", format!("{e:#}")),
    }
}

/// What moves a run's clock here: the branch counter when this boot's
/// self-test found it exact, as runs decide with `--clock auto`.
fn clock(home: &Home, guest: &Guest) -> Finding {
    use rewind_core::pmu::{self, Vendor};
    let vendor = match Vendor::detect() {
        Ok(v) => v,
        Err(e) => return Finding::new(Level::Warn, "clock", format!("{e:#}")),
    };
    let usable = vendor.event().is_some()
        && pmu::counter_usable_this_boot(home, guest, &vendor).unwrap_or(false);
    if usable {
        return Finding::new(Level::Ok, "clock", format!("counter time on {vendor}"));
    }
    Finding::new(Level::Warn, "clock", pmu::exit_time_warning(&vendor))
}

/// gdb, for `rewind gdb`, and its Python, for `rewind where` and the
/// desktop app's source panel.
fn gdb() -> Finding {
    let Ok(version) = Command::new(GDB_PROGRAM).arg("--version").output() else {
        return Finding::new(
            Level::Warn,
            "gdb",
            "not on PATH; `rewind gdb` and `rewind where` need it",
        );
    };
    let name = String::from_utf8_lossy(&version.stdout)
        .lines()
        .next()
        .unwrap_or("gdb")
        .to_string();
    let python = Command::new(GDB_PROGRAM)
        .args(["-nx", "-batch", "-ex", "python print(1)"])
        .output()
        .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "1");
    if python {
        return Finding::new(Level::Ok, "gdb", format!("{name}, with Python"));
    }
    Finding::new(
        Level::Warn,
        "gdb",
        format!("{name}, without Python, which `rewind where` and the app's source panel need"),
    )
}

/// The debuginfod server that gives gdb the DWARF and sources of what ran
/// in the VM.
fn debuginfod() -> Finding {
    if let Some(named) = std::env::var_os(ENV_DEBUGINFOD).map(PathBuf::from) {
        if named.exists() {
            return Finding::new(Level::Ok, "debuginfod", named.display().to_string());
        }
        return Finding::new(
            Level::Ok,
            "debuginfod",
            format!(
                "{}, fetched the first time `rewind gdb` runs",
                named.display()
            ),
        );
    }
    on_path(
        "debuginfod",
        DEBUGINFOD_PROGRAM,
        "gdb gets no DWARF or sources for the programs in the VM without it",
    )
}

/// Whether `program` is on PATH, and what goes without it.
fn on_path(topic: &'static str, program: &str, without: &str) -> Finding {
    let found = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(program))
            .find(|p| p.exists())
    });
    match found {
        Some(path) => Finding::new(Level::Ok, topic, path.display().to_string()),
        None => Finding::new(
            Level::Warn,
            topic,
            format!("{program} is not on PATH; {without}"),
        ),
    }
}

/// What the home takes on disk, and what its file system has left.
fn disk(home: &Home) -> Finding {
    let usage = Usage {
        runs: bytes_under(&home.runs()),
        images: bytes_under(&home.images()),
        pages: bytes_under(&home.store()),
        other: bytes_under(&home.inputs())
            + bytes_under(&home.source_cache())
            + bytes_under(&home.debuginfod_cache()),
        free: free_bytes(home.root()),
    };
    usage.finding(home.root())
}

/// The bytes the home's parts take, and the bytes free beside them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Usage {
    runs: u64,
    images: u64,
    pages: u64,
    /// Imported inputs and cached source files.
    other: u64,
    free: Option<u64>,
}

/// Less free space than this is worth a warning: one Nix build's image and
/// keyframes can take a few gigabytes.
const LOW_FREE: u64 = 5_000_000_000;

impl Usage {
    fn finding(&self, root: &Path) -> Finding {
        let total = self.runs + self.images + self.pages + self.other;
        let mut text = format!(
            "{} takes {}: runs {}, images {}, pages {}",
            root.display(),
            show::size(total),
            show::size(self.runs),
            show::size(self.images),
            show::size(self.pages)
        );
        if self.other > 0 {
            text.push_str(&format!(", inputs and sources {}", show::size(self.other)));
        }
        let Some(free) = self.free else {
            return Finding::new(Level::Ok, "disk", text);
        };
        text.push_str(&format!("; {} free", show::size(free)));
        if free < LOW_FREE {
            text.push_str("; `rewind gc` removes what no run uses");
            return Finding::new(Level::Warn, "disk", text);
        }
        Finding::new(Level::Ok, "disk", text)
    }
}

/// The disk space the files under `dir` take.
fn bytes_under(dir: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    /// st_blocks counts 512-byte blocks whatever the file system's own.
    const BLOCK: u64 = 512;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut total = 0;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() {
            total += bytes_under(&entry.path());
            continue;
        }
        total += meta.blocks() * BLOCK;
    }
    total
}

/// The bytes an unprivileged user can still write on `path`'s file system.
fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: statvfs fills the struct it is given for a NUL-terminated
    // path.
    if unsafe { libc::statvfs(c.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    Some(stat.f_bavail as u64 * stat.f_frsize as u64)
}

/// How many unreadable runs a finding names.
const UNREADABLE_SHOWN: usize = 5;

/// Runs whose manifests do not read, by id, with the command that removes
/// them.
fn unreadable(runs: &[Unreadable]) -> Finding {
    if runs.is_empty() {
        return Finding::new(Level::Ok, "runs", "every run's manifest reads");
    }
    let mut named = runs
        .iter()
        .take(UNREADABLE_SHOWN)
        .map(|u| u.id.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    if runs.len() > UNREADABLE_SHOWN {
        named.push_str(&format!(" and {} more", runs.len() - UNREADABLE_SHOWN));
    }
    Finding::new(
        Level::Warn,
        "runs",
        format!(
            "{} runs have manifests another build of rewind wrote: {named}; `rewind remove <id>` \
             removes one",
            runs.len()
        ),
    )
}

#[cfg(test)]
mod tests {
    // The findings that are worked out rather than probed: lines, the
    // disk summary and unreadable runs.
    use super::*;

    #[test]
    fn a_finding_is_a_line_of_columns() {
        // The level and the topic each take a column of their own, so the
        // findings' texts start under one another.
        let f = Finding::new(Level::Warn, "gdb", "not on PATH");
        assert_eq!(f.line(), "warn  gdb         not on PATH");
        let f = Finding::new(Level::Ok, "debuginfod", "/bin/x");
        assert_eq!(f.line(), "ok    debuginfod  /bin/x");
    }

    #[test]
    fn the_disk_finding_sums_the_home_and_warns_when_space_is_low() {
        // The home's parts in decimal units, then the free space, which
        // under LOW_FREE is a warning.
        let usage = Usage {
            runs: 2_000_000_000,
            images: 64_000_000_000,
            pages: 1_500_000,
            other: 0,
            free: Some(120_000_000_000),
        };
        let f = usage.finding(Path::new("/h"));
        assert_eq!(f.level, Level::Ok);
        assert_eq!(
            f.text,
            "/h takes 66.0 GB: runs 2.0 GB, images 64.0 GB, pages 1.5 MB; 120.0 GB free"
        );
        let low = Usage {
            free: Some(LOW_FREE - 1),
            ..usage
        };
        assert_eq!(low.finding(Path::new("/h")).level, Level::Warn);
    }

    #[test]
    fn unreadable_runs_are_named_with_how_to_remove_them() {
        // None is fine; seven name the first five and count the rest.
        assert_eq!(unreadable(&[]).level, Level::Ok);
        let runs: Vec<Unreadable> = (0..7)
            .map(|i| Unreadable {
                id: format!("{i:016}"),
                dir: PathBuf::from("/runs"),
                reason: "missing field".into(),
                written: 0,
            })
            .collect();
        let f = unreadable(&runs);
        assert_eq!(f.level, Level::Warn);
        assert_eq!(
            f.text,
            "7 runs have manifests another build of rewind wrote: 0000000000000000, \
             0000000000000001, 0000000000000002, 0000000000000003, 0000000000000004 and 2 \
             more; `rewind remove <id>` removes one"
        );
    }
}
