//! Nix derivations as runs.
//!
//! A derivation already names everything its build may see: a builder,
//! its arguments and environment, and the store paths it depends on. So a
//! derivation is a run spec almost as it stands. The input closure becomes
//! the image the guest mounts at /nix/store, the builder becomes the job,
//! and the environment is set up the way the Nix sandbox sets it up, so
//! the builder cannot tell the difference.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use rewind_init::{Job, JobFile, Root};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const STORE: &str = "/nix/store";

/// The sandbox's paths and ids, as nix-daemon uses them.
pub const BUILD_DIR: &str = "/build";
pub const HOMELESS: &str = "/homeless-shelter";
pub const BUILDER_UID: u32 = 1000;
pub const BUILDER_GID: u32 = 100;

/// The NIX_BUILD_CORES a build sees unless asked otherwise: the VM's one
/// vCPU.
pub const DEFAULT_BUILD_CORES: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Derivation {
    pub path: PathBuf,
    pub name: String,
    pub system: String,
    pub builder: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// Output names and their store paths.
    pub outputs: Vec<(String, PathBuf)>,
    pub input_srcs: Vec<PathBuf>,
    /// Input derivations and the outputs of each this one uses.
    pub input_drvs: Vec<(PathBuf, Vec<String>)>,
}

/// A store path from either a full path or, as `nix derivation show`
/// prints them since format version 4, a base name.
fn store_path(s: &str) -> PathBuf {
    if s.starts_with('/') {
        PathBuf::from(s)
    } else {
        Path::new(STORE).join(s)
    }
}

/// The derivation an installable names: a .drv path, or anything
/// `nix path-info --derivation` accepts, such as `nixpkgs#hello`.
pub fn resolve(installable: &str) -> Result<PathBuf> {
    if installable.ends_with(".drv") && Path::new(installable).exists() {
        return Ok(PathBuf::from(installable));
    }
    let out = nix(&["path-info", "--derivation", installable])?;
    let drv = out
        .lines()
        .next()
        .with_context(|| format!("{installable} names no derivation"))?;
    Ok(PathBuf::from(drv.trim()))
}

pub fn show(drv: &Path) -> Result<Derivation> {
    let text = nix(&["derivation", "show", &drv.to_string_lossy()])?;
    let json: Value = serde_json::from_str(&text).context("parsing nix derivation show")?;
    // Format 4 wraps the derivations in an object with a version; older
    // formats are the map of derivations itself.
    let drvs = json.get("derivations").unwrap_or(&json);
    let (_, d) = drvs
        .as_object()
        .and_then(|m| m.iter().next())
        .context("nix derivation show printed no derivation")?;

    let strings = |v: &Value| -> Vec<String> {
        v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    };
    let env: BTreeMap<String, String> = d["env"]
        .as_object()
        .context("derivation has no env")?
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
        .collect();

    let mut outputs = Vec::new();
    for (name, o) in d["outputs"]
        .as_object()
        .context("derivation has no outputs")?
    {
        let path = o["path"].as_str().with_context(|| {
            format!("output {name} has no fixed path; content-addressed derivations are not supported yet")
        })?;
        outputs.push((name.clone(), store_path(path)));
    }

    // Inputs moved under "inputs" in format 4.
    let (srcs, drvs_in) = match d.get("inputs") {
        Some(inputs) => (&inputs["srcs"], &inputs["drvs"]),
        None => (&d["inputSrcs"], &d["inputDrvs"]),
    };
    let input_srcs = strings(srcs).iter().map(|s| store_path(s)).collect();
    let mut input_drvs = Vec::new();
    for (path, spec) in drvs_in.as_object().into_iter().flatten() {
        // Older formats list the outputs directly, newer ones under
        // "outputs".
        let outs = match spec.get("outputs") {
            Some(o) => strings(o),
            None => strings(spec),
        };
        input_drvs.push((store_path(path), outs));
    }

    Ok(Derivation {
        path: drv.to_path_buf(),
        name: d["name"].as_str().unwrap_or_default().to_string(),
        system: d["system"].as_str().unwrap_or_default().to_string(),
        builder: d["builder"]
            .as_str()
            .context("derivation has no builder")?
            .to_string(),
        args: strings(&d["args"]),
        env,
        outputs,
        input_srcs,
        input_drvs,
    })
}

/// Builds or substitutes every input and returns the closure the build may
/// see. The sandbox shell is not part of the closure: the initramfs carries
/// its own copy at /bin/sh.
pub fn input_closure(drv: &Derivation) -> Result<Vec<PathBuf>> {
    let mut roots: Vec<String> = drv
        .input_srcs
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();

    if !drv.input_drvs.is_empty() {
        let installables: Vec<String> = drv
            .input_drvs
            .iter()
            .map(|(path, outs)| format!("{}^{}", path.display(), outs.join(",")))
            .collect();
        let mut args = vec!["build", "--no-link", "--print-out-paths"];
        args.extend(installables.iter().map(String::as_str));
        let built = nix(&args)?;
        roots.extend(built.lines().map(|l| l.trim().to_string()));
    }

    let mut args = vec!["path-info", "--recursive"];
    args.extend(roots.iter().map(String::as_str));
    let closure = nix(&args)?;
    let mut paths: Vec<PathBuf> = closure.lines().map(|l| PathBuf::from(l.trim())).collect();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// The builder as a job, with the environment the Nix sandbox gives it.
/// The order of operations follows nix-daemon's initEnv, so a derivation
/// that overrides one of these sees its own value. `cores` is the
/// NIX_BUILD_CORES the build sees, which can exceed the VM's one vCPU: the
/// jobs it starts then interleave on that vCPU instead of running in
/// parallel.
pub fn job(drv: &Derivation, cores: u32) -> Result<Job> {
    if drv.env.contains_key("__json") {
        bail!(
            "{} uses structured attributes, which are not supported yet",
            drv.name
        );
    }

    let mut env: BTreeMap<String, String> = BTreeMap::new();
    env.insert("PATH".into(), "/path-not-set".into());
    env.insert("HOME".into(), HOMELESS.into());
    env.insert("NIX_STORE".into(), STORE.into());
    env.insert("NIX_BUILD_CORES".into(), cores.to_string());

    // passAsFile: each named attribute is written to a file in the build
    // directory and replaced by a variable holding the file's path.
    let pass_as_file: Vec<&str> = drv
        .env
        .get("passAsFile")
        .map(|s| s.split_whitespace().collect())
        .unwrap_or_default();
    let mut files = Vec::new();
    for (k, v) in &drv.env {
        if pass_as_file.contains(&k.as_str()) {
            let path = format!("{BUILD_DIR}/.attr-{}", nix32(&Sha256::digest(k.as_bytes())));
            env.insert(format!("{k}Path"), path.clone());
            files.push(JobFile {
                path,
                contents: v.clone(),
            });
        } else {
            env.insert(k.clone(), v.clone());
        }
    }

    for var in ["NIX_BUILD_TOP", "TMPDIR", "TEMPDIR", "TMP", "TEMP", "PWD"] {
        env.insert(var.into(), BUILD_DIR.into());
    }
    env.insert("NIX_LOG_FD".into(), "2".into());
    env.insert("TERM".into(), "xterm-256color".into());

    let mut argv = vec![drv.builder.clone()];
    argv.extend(drv.args.iter().cloned());
    Ok(Job {
        argv,
        env: env.into_iter().collect(),
        cwd: BUILD_DIR.into(),
        uid: BUILDER_UID,
        gid: BUILDER_GID,
        hostname: "localhost".into(),
        root: Root::Store,
        files,
        outputs: drv
            .outputs
            .iter()
            .map(|(_, p)| p.to_string_lossy().into_owned())
            .collect(),
    })
}

/// Nix's base-32, as it names .attr files and store paths.
pub fn nix32(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"0123456789abcdfghijklmnpqrsvwxyz";
    let len = (bytes.len() * 8 - 1) / 5 + 1;
    let mut out = String::with_capacity(len);
    for n in (0..len).rev() {
        let bit = n * 5;
        let (i, j) = (bit / 8, bit % 8);
        let lo = (bytes[i] as u16) >> j;
        let hi = bytes.get(i + 1).map_or(0, |b| (*b as u16) << (8 - j));
        out.push(ALPHABET[((lo | hi) & 0x1f) as usize] as char);
    }
    out
}

/// Runs a nix command and returns its standard output.
/// Builds or substitutes `installables` and returns their outputs and the
/// closure of those outputs, sorted.
pub fn packages(installables: &[String]) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let mut args = vec!["build", "--no-link", "--print-out-paths"];
    args.extend(installables.iter().map(String::as_str));
    let outputs: Vec<PathBuf> = nix(&args)?
        .lines()
        .map(|l| PathBuf::from(l.trim()))
        .collect();

    let mut args = vec!["path-info", "--recursive"];
    let names: Vec<String> = outputs.iter().map(|p| p.display().to_string()).collect();
    args.extend(names.iter().map(String::as_str));
    let mut closure: Vec<PathBuf> = nix(&args)?
        .lines()
        .map(|l| PathBuf::from(l.trim()))
        .collect();
    closure.sort();
    closure.dedup();
    Ok((outputs, closure))
}

fn nix(args: &[&str]) -> Result<String> {
    let out = Command::new("nix")
        .args(["--extra-experimental-features", "nix-command flakes"])
        .args(args)
        .output()
        .context("running nix; is it on PATH?")?;
    if !out.status.success() {
        bail!(
            "nix {} failed:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8(out.stdout)?)
}

#[cfg(test)]
mod tests {
    // nix32 against hashes Nix itself printed, and the job's environment
    // against the rules of nix-daemon's initEnv.
    use super::*;

    #[test]
    fn nix32_matches_nix() {
        // `nix hash convert --to nix32 sha256:$(printf abc | sha256sum)`
        let digest = Sha256::digest(b"abc");
        assert_eq!(
            nix32(&digest),
            "1b8m03r63zqhnjf7l5wnldhh7c134ap5vpj0850ymkq1iyzicy5s"
        );
    }

    fn sample() -> Derivation {
        let mut env = BTreeMap::new();
        env.insert("out".into(), "/nix/store/aaa-x".into());
        env.insert("text".into(), "hello".into());
        env.insert("passAsFile".into(), "text".into());
        env.insert("HOME".into(), "/overridden".into());
        Derivation {
            path: "/nix/store/bbb-x.drv".into(),
            name: "x".into(),
            system: "x86_64-linux".into(),
            builder: "/nix/store/ccc-bash/bin/bash".into(),
            args: vec!["-e".into(), "builder.sh".into()],
            env,
            outputs: vec![("out".into(), "/nix/store/aaa-x".into())],
            input_srcs: vec![],
            input_drvs: vec![],
        }
    }

    #[test]
    fn job_env_gives_the_build_the_cores_asked_for() {
        // A build asked to use 4 cores sees NIX_BUILD_CORES=4, which stdenv
        // passes to make, ninja and the test runners as their job count.
        let job = job(&sample(), 4).unwrap();
        let env: BTreeMap<_, _> = job.env.iter().cloned().collect();
        assert_eq!(env["NIX_BUILD_CORES"], "4");
    }

    #[test]
    fn job_env_follows_the_sandbox() {
        let job = job(&sample(), 1).unwrap();
        let env: BTreeMap<_, _> = job.env.iter().cloned().collect();
        assert_eq!(env["PATH"], "/path-not-set");
        assert_eq!(env["HOME"], "/overridden");
        assert_eq!(env["TMPDIR"], "/build");
        assert_eq!(env["NIX_BUILD_CORES"], "1");
        assert!(!env.contains_key("text"));
        let file = &job.files[0];
        assert_eq!(env["textPath"], file.path);
        assert_eq!(file.contents, "hello");
        assert!(file.path.starts_with("/build/.attr-"));
        assert_eq!(
            job.argv,
            vec!["/nix/store/ccc-bash/bin/bash", "-e", "builder.sh"]
        );
        assert_eq!(job.outputs, vec!["/nix/store/aaa-x".to_string()]);
    }
}
