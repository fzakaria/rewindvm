//! Names this rewind for `rewind --version` and the manifests of the runs
//! it records, as REWIND_VERSION: the package's version, and the commit it
//! is built from when one is known. That is REWIND_COMMIT when the build
//! sets it, as the Nix packages do from the flake's revision, else the
//! commit git has checked out.

use std::process::Command;

/// The variable the Nix packages set.
const COMMIT_ENV: &str = "REWIND_COMMIT";

/// The variable the crate reads its version from.
const VERSION_ENV: &str = "REWIND_VERSION";

/// How many hex digits of a commit's hash the version shows.
const COMMIT_DIGITS: &str = "12";

fn main() {
    println!("cargo:rerun-if-env-changed={COMMIT_ENV}");

    // A checkout's commit moves with HEAD, which jj moves on every commit
    // too, so the build script runs again when it does.
    let git_dir = Command::new("git")
        .args(["rev-parse", "--absolute-git-dir"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    if let Some(dir) = &git_dir {
        println!("cargo:rerun-if-changed={dir}/HEAD");
    }

    let given = std::env::var(COMMIT_ENV).ok().filter(|c| !c.is_empty());
    let commit = given.or_else(|| {
        git_dir.as_ref()?;
        let out = Command::new("git")
            .args(["rev-parse", &format!("--short={COMMIT_DIGITS}"), "HEAD"])
            .output()
            .ok()
            .filter(|o| o.status.success())?;
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    });
    let version = std::env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");
    let named = match commit {
        Some(commit) => format!("{version} ({commit})"),
        None => version,
    };
    println!("cargo:rustc-env={VERSION_ENV}={named}");
}
