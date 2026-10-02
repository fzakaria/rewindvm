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
use nix_derivation::store_path::hash_placeholder;
use nix_derivation::{StorePath, StructuredAttrsFiles, nixbase32};
use rewind_init::{Job, JobFile, Root};
use sha2::{Digest, Sha256};

pub const STORE: &str = "/nix/store";

/// The sandbox's paths and ids, as nix-daemon uses them.
pub const BUILD_DIR: &str = "/build";
pub const HOMELESS: &str = "/homeless-shelter";
pub const BUILDER_UID: u32 = 1000;
pub const BUILDER_GID: u32 = 100;

/// The files a structured-attributes build reads its attributes from, in
/// the build directory, and the variables that name them.
const ATTRS_SH: &str = StructuredAttrsFiles::SHELL_FILE_NAME;
const ATTRS_JSON: &str = StructuredAttrsFiles::JSON_FILE_NAME;
const ATTRS_SH_VAR: &str = "NIX_ATTRS_SH_FILE";
const ATTRS_JSON_VAR: &str = "NIX_ATTRS_JSON_FILE";

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
    /// The .attrs.json and .attrs.sh a derivation with
    /// `__structuredAttrs = true` gives its build in place of `env`.
    pub structured_attrs: Option<StructuredAttrsFiles>,
    /// Whether the output is fixed by a hash given up front, as a
    /// fetcher's is.
    pub fixed_output: bool,
}

/// The prefix of a builder nix-daemon runs itself, such as
/// builtin:fetchurl, rather than executing.
const BUILTIN_BUILDER_PREFIX: &str = "builtin:";

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

/// The derivation in a .drv file, read as Nix wrote it.
pub fn show(drv: &Path) -> Result<Derivation> {
    let aterm = std::fs::read(drv).with_context(|| format!("reading {}", drv.display()))?;
    parse(drv, &aterm)
}

/// The derivation `aterm`, the contents of the .drv file `drv`.
fn parse(drv: &Path, aterm: &[u8]) -> Result<Derivation> {
    // Nix keeps a derivation's name out of its ATerm, in its file name.
    let file_name = drv
        .file_name()
        .with_context(|| format!("{} names no file", drv.display()))?;
    let store_path = StorePath::from_basename(file_name.as_encoded_bytes())
        .with_context(|| format!("{} is not a store path", drv.display()))?;
    let name = store_path
        .name()
        .strip_suffix(".drv")
        .with_context(|| format!("{} is not a .drv file", drv.display()))?;
    let parsed = nix_derivation::Derivation::from_aterm_bytes(aterm, name)
        .with_context(|| format!("parsing {}", drv.display()))?;

    // Each output's path. A content-addressed output has one only once it
    // is built.
    let mut outputs = Vec::new();
    for (out, output) in parsed.outputs() {
        let Some(path) = output.path(name, out)? else {
            bail!(
                "output {out} of {name} has no fixed path; content-addressed derivations are not supported yet"
            );
        };
        outputs.push((out.clone(), PathBuf::from(path.to_absolute_path())));
    }

    // The environment as text, which is all a job's environment can hold.
    let mut env = BTreeMap::new();
    for (k, v) in parsed.environment() {
        let v = String::from_utf8(v.clone())
            .with_context(|| format!("{k} in {name}'s environment is not UTF-8"))?;
        env.insert(k.clone(), v);
    }

    let input_srcs = parsed
        .input_sources()
        .iter()
        .map(|p| PathBuf::from(p.to_absolute_path()))
        .collect();
    let mut input_drvs = Vec::new();
    for (path, input) in parsed.input_derivations() {
        if input.is_dynamic() {
            bail!("{name} uses outputs of dynamic derivations, which are not supported yet");
        }
        input_drvs.push((
            PathBuf::from(path.to_absolute_path()),
            input.outputs().iter().cloned().collect(),
        ));
    }

    let structured_attrs = parsed
        .structured_attrs_files()
        .with_context(|| format!("writing {name}'s structured attributes"))?;
    let fixed_output = parsed.is_fixed_output()?;

    Ok(Derivation {
        path: drv.to_path_buf(),
        name: name.to_string(),
        system: parsed.system().to_string(),
        builder: parsed.builder().to_string(),
        args: parsed.arguments().to_vec(),
        env,
        outputs,
        input_srcs,
        input_drvs,
        structured_attrs,
        fixed_output,
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
/// NIX_BUILD_CORES the build sees, the run's CPU count.
pub fn job(drv: &Derivation, cores: u32) -> Result<Job> {
    if drv.builder.starts_with(BUILTIN_BUILDER_PREFIX) {
        bail!(
            "{} is built by {}, which runs inside nix-daemon and has no program for a run to execute",
            drv.name,
            drv.builder
        );
    }

    let mut env: BTreeMap<String, String> = BTreeMap::new();
    env.insert("PATH".into(), "/path-not-set".into());
    env.insert("HOME".into(), HOMELESS.into());
    env.insert("NIX_STORE".into(), STORE.into());
    env.insert("NIX_BUILD_CORES".into(), cores.to_string());

    // The derivation's own attributes: as environment variables, or as
    // files when the derivation uses structured attributes.
    let files = match &drv.structured_attrs {
        Some(attrs) => structured_files(attrs, &mut env)?,
        None => env_files(drv, &mut env),
    };

    for var in ["NIX_BUILD_TOP", "TMPDIR", "TEMPDIR", "TMP", "TEMP", "PWD"] {
        env.insert(var.into(), BUILD_DIR.into());
    }

    // A fixed-output build is told its output's hash will be checked, so a
    // fetcher can skip checking it itself.
    if drv.fixed_output {
        env.insert("NIX_OUTPUT_CHECKED".into(), "1".into());
    }

    env.insert("NIX_LOG_FD".into(), "2".into());
    env.insert("TERM".into(), "xterm-256color".into());

    // Output placeholders become the outputs' paths everywhere the build
    // can see them: the args, the environment and the files.
    let rewrites: Vec<(String, String)> = drv
        .outputs
        .iter()
        .map(|(name, path)| (hash_placeholder(name), path.to_string_lossy().into_owned()))
        .collect();
    let mut argv = vec![drv.builder.clone()];
    argv.extend(drv.args.iter().map(|a| rewrite(a, &rewrites)));
    let env = env
        .into_iter()
        .map(|(k, v)| (k, rewrite(&v, &rewrites)))
        .collect();
    let files = files
        .into_iter()
        .map(|f| JobFile {
            contents: rewrite(&f.contents, &rewrites),
            ..f
        })
        .collect();

    Ok(Job {
        argv,
        env,
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

/// The derivation's env, set directly in the build's environment or, for
/// the attributes passAsFile names, written to a file in the build
/// directory and replaced by a variable holding the file's path.
fn env_files(drv: &Derivation, env: &mut BTreeMap<String, String>) -> Vec<JobFile> {
    let pass_as_file: Vec<&str> = drv
        .env
        .get("passAsFile")
        .map(|s| s.split_whitespace().collect())
        .unwrap_or_default();
    let mut files = Vec::new();
    for (k, v) in &drv.env {
        if !pass_as_file.contains(&k.as_str()) {
            env.insert(k.clone(), v.clone());
            continue;
        }
        let path = format!(
            "{BUILD_DIR}/.attr-{}",
            nixbase32::encode(&Sha256::digest(k.as_bytes()))
        );
        env.insert(format!("{k}Path"), path.clone());
        files.push(JobFile {
            path,
            contents: v.clone(),
        });
    }
    files
}

/// Structured attributes as nix-daemon hands them to the build: written
/// to .attrs.json and, as bash declarations, to .attrs.sh, with "outputs"
/// replaced by each output's path. The env is not set at all; stdenv
/// exports the attributes it needs from .attrs.sh.
fn structured_files(
    attrs: &StructuredAttrsFiles,
    env: &mut BTreeMap<String, String>,
) -> Result<Vec<JobFile>> {
    let sh = format!("{BUILD_DIR}/{ATTRS_SH}");
    let json = format!("{BUILD_DIR}/{ATTRS_JSON}");
    env.insert(ATTRS_SH_VAR.into(), sh.clone());
    env.insert(ATTRS_JSON_VAR.into(), json.clone());
    Ok(vec![
        JobFile {
            path: sh,
            contents: String::from_utf8(attrs.shell.clone()).context(".attrs.sh is not UTF-8")?,
        },
        JobFile {
            path: json,
            contents: String::from_utf8(attrs.json.clone()).context(".attrs.json is not UTF-8")?,
        },
    ])
}

/// Replaces each output's placeholder in `s` with the output's path.
fn rewrite(s: &str, rewrites: &[(String, String)]) -> String {
    rewrites
        .iter()
        .fold(s.to_string(), |s, (from, to)| s.replace(from, to))
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
            nixbase32::encode(&digest),
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
            structured_attrs: None,
            fixed_output: false,
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

    #[test]
    fn placeholder_matches_nix() {
        // `nix eval --raw --expr 'builtins.placeholder "out"'`
        assert_eq!(
            hash_placeholder("out"),
            "/1rz4g4znpzjwh1xymhjpm42vipw92pr73vdgl6xs1hycac8kf2n9"
        );
    }

    // Store paths for the .drv fixtures. The parser checks their form, so
    // these are real-looking paths rather than /nix/store/aaa-x.
    const FIXTURE_DRV: &str = "/nix/store/0000000000000000000000000000000a-x.drv";
    const FIXTURE_OUT: &str = "/nix/store/0000000000000000000000000000000b-x";
    const FIXTURE_DEP: &str = "/nix/store/0000000000000000000000000000000c-dep.drv";
    const FIXTURE_SRC: &str = "/nix/store/0000000000000000000000000000000d-src";

    /// A string as Nix writes one in a .drv file.
    fn aterm_string(s: &str) -> String {
        let escaped = s
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t");
        format!("\"{escaped}\"")
    }

    /// A .drv file's contents for a derivation named x with one
    /// input-addressed output, one input derivation, one input source and
    /// the environment `env`.
    fn fixture(env: &[(&str, &str)]) -> Vec<u8> {
        let output = format!("(\"out\",\"{FIXTURE_OUT}\",\"\",\"\")");
        fixture_with(&output, "/bin/sh", env)
    }

    /// A .drv file's contents for a derivation named x with the output
    /// `output`, in ATerm syntax, the builder `builder` and the
    /// environment `env`.
    fn fixture_with(output: &str, builder: &str, env: &[(&str, &str)]) -> Vec<u8> {
        let mut env: BTreeMap<&str, &str> = env.iter().copied().collect();
        env.insert("out", FIXTURE_OUT);
        let env: Vec<String> = env
            .iter()
            .map(|(k, v)| format!("({},{})", aterm_string(k), aterm_string(v)))
            .collect();
        format!(
            "Derive([{output}],[(\"{FIXTURE_DEP}\",[\"out\"])],[\"{FIXTURE_SRC}\"],\"x86_64-linux\",{},[\"-e\",\"builder.sh\"],[{}])",
            aterm_string(builder),
            env.join(",")
        )
        .into_bytes()
    }

    /// The output of a fixed-output derivation, in ATerm syntax.
    fn fixed_output() -> String {
        let hash = "0".repeat(64);
        format!("(\"out\",\"{FIXTURE_OUT}\",\"sha256\",\"{hash}\")")
    }

    #[test]
    fn fixed_output_builds_are_told_the_output_is_checked() {
        // nix-daemon sets NIX_OUTPUT_CHECKED=1 for a fixed-output
        // derivation, and only for one, so fetchers can skip checking the
        // hash themselves. A fixed-output fixture's job has it; an
        // input-addressed one's does not.
        let fixed = parse(
            Path::new(FIXTURE_DRV),
            &fixture_with(&fixed_output(), "/bin/sh", &[]),
        )
        .unwrap();
        let env: BTreeMap<_, _> = job(&fixed, 1).unwrap().env.into_iter().collect();
        assert_eq!(env["NIX_OUTPUT_CHECKED"], "1");

        let plain = parse(Path::new(FIXTURE_DRV), &fixture(&[])).unwrap();
        let env: BTreeMap<_, _> = job(&plain, 1).unwrap().env.into_iter().collect();
        assert!(!env.contains_key("NIX_OUTPUT_CHECKED"));
    }

    #[test]
    fn builtin_builders_are_refused() {
        // A builder such as builtin:fetchurl runs inside nix-daemon, not as
        // a program, so a run has nothing to execute. Rewind says so.
        let drv = parse(
            Path::new(FIXTURE_DRV),
            &fixture_with(&fixed_output(), "builtin:fetchurl", &[]),
        )
        .unwrap();
        let err = job(&drv, 1).unwrap_err().to_string();
        assert!(err.contains("builtin:fetchurl"), "{err}");
    }

    fn sample_structured(attrs: serde_json::Value) -> Derivation {
        let json = attrs.to_string();
        parse(Path::new(FIXTURE_DRV), &fixture(&[("__json", &json)])).unwrap()
    }

    #[test]
    fn show_reads_the_drv_file() {
        // A .drv file parses to its name, builder, args, env, outputs and
        // inputs.
        let drv = parse(Path::new(FIXTURE_DRV), &fixture(&[("pname", "x")])).unwrap();
        assert_eq!(drv.name, "x");
        assert_eq!(drv.system, "x86_64-linux");
        assert_eq!(drv.builder, "/bin/sh");
        assert_eq!(drv.args, vec!["-e", "builder.sh"]);
        assert_eq!(drv.env["pname"], "x");
        assert_eq!(drv.env["out"], FIXTURE_OUT);
        assert_eq!(
            drv.outputs,
            vec![("out".to_string(), PathBuf::from(FIXTURE_OUT))]
        );
        assert_eq!(drv.input_srcs, vec![PathBuf::from(FIXTURE_SRC)]);
        assert_eq!(
            drv.input_drvs,
            vec![(PathBuf::from(FIXTURE_DEP), vec!["out".to_string()])]
        );
        assert_eq!(drv.structured_attrs, None);
    }

    #[test]
    fn placeholders_become_output_paths() {
        // An input-addressed derivation keeps `placeholder "out"` in its
        // env and args; nix-daemon swaps in the output path when the build
        // starts, in the env, the args and the files it writes. A job built
        // from a derivation using the placeholder in each of those places
        // sees the output path in each.
        let out = hash_placeholder("out");
        let mut drv = sample();
        drv.args.push(format!("--prefix={out}"));
        drv.env.insert("flags".into(), format!("-DLIB={out}/lib"));
        drv.env.insert("text".into(), format!("{out}/share"));
        let plain = job(&drv, 1).unwrap();
        let env: BTreeMap<_, _> = plain.env.iter().cloned().collect();
        assert_eq!(env["flags"], "-DLIB=/nix/store/aaa-x/lib");
        assert_eq!(plain.argv.last().unwrap(), "--prefix=/nix/store/aaa-x");
        assert_eq!(plain.files[0].contents, "/nix/store/aaa-x/share");

        let drv = sample_structured(serde_json::json!({"outputs": ["out"], "prefix": out}));
        let structured = job(&drv, 1).unwrap();
        for file in &structured.files {
            assert!(!file.contents.contains("1rz4g4zn"), "{}", file.contents);
        }
        let prefix = format!("declare prefix='{FIXTURE_OUT}'");
        assert!(structured.files[0].contents.contains(&prefix));
    }

    #[test]
    fn structured_attrs_reach_the_build_as_files() {
        // A derivation with structured attributes gets its attributes as
        // .attrs.sh and .attrs.json in the build directory, named by
        // NIX_ATTRS_SH_FILE and NIX_ATTRS_JSON_FILE, and none of its env.
        // .attrs.sh is held to the bytes Nix's StructuredAttrs::writeShell
        // writes for the same attributes.
        let drv = sample_structured(serde_json::json!({
            "bad-name": "skipped",
            "doCheck": true,
            "env": {"FOO": "bar"},
            "list": ["a", "b'c"],
            "n": 3,
            "nested": {"a": ["x"]},
            "outputs": ["out"],
            "stdenv": "/nix/store/ddd-stdenv",
        }));
        let job = job(&drv, 1).unwrap();
        let env: BTreeMap<_, _> = job.env.iter().cloned().collect();
        assert_eq!(env["NIX_ATTRS_SH_FILE"], "/build/.attrs.sh");
        assert_eq!(env["NIX_ATTRS_JSON_FILE"], "/build/.attrs.json");
        assert_eq!(env["HOME"], HOMELESS);
        assert!(!env.contains_key("out"));
        assert!(!env.contains_key("__json"));
        assert!(!env.contains_key("FOO"));

        let files: BTreeMap<_, _> = job
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.contents.as_str()))
            .collect();
        assert_eq!(
            files["/build/.attrs.sh"],
            format!(
                "declare doCheck=1\n\
                 declare -A env=(['FOO']='bar' )\n\
                 declare -a list=('a' 'b'\\''c' )\n\
                 declare n=3\n\
                 declare -A outputs=(['out']='{FIXTURE_OUT}' )\n\
                 declare stdenv='/nix/store/ddd-stdenv'\n"
            )
        );
        let json: serde_json::Value = serde_json::from_str(files["/build/.attrs.json"]).unwrap();
        assert_eq!(json["outputs"], serde_json::json!({"out": FIXTURE_OUT}));
        assert_eq!(json["stdenv"], "/nix/store/ddd-stdenv");
    }
}
