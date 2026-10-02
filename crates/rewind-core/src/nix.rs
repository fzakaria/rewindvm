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
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

pub const STORE: &str = "/nix/store";

/// The sandbox's paths and ids, as nix-daemon uses them.
pub const BUILD_DIR: &str = "/build";
pub const HOMELESS: &str = "/homeless-shelter";
pub const BUILDER_UID: u32 = 1000;
pub const BUILDER_GID: u32 = 100;

/// The files a structured-attributes build reads its attributes from, in
/// the build directory, and the variables that name them.
const ATTRS_SH: &str = ".attrs.sh";
const ATTRS_JSON: &str = ".attrs.json";
const ATTRS_SH_VAR: &str = "NIX_ATTRS_SH_FILE";
const ATTRS_JSON_VAR: &str = "NIX_ATTRS_JSON_FILE";

/// The env entry older formats of `nix derivation show` carry structured
/// attributes in, as JSON text.
const LEGACY_ATTRS_KEY: &str = "__json";

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
    /// The attributes of a derivation with `__structuredAttrs = true`.
    /// The build sees these as files, and none of `env`.
    pub structured_attrs: Option<Map<String, Value>>,
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
    parse(drv, &json)
}

/// The derivation in the output of `nix derivation show`.
fn parse(drv: &Path, json: &Value) -> Result<Derivation> {
    // Format 4 wraps the derivations in an object with a version; older
    // formats are the map of derivations itself.
    let drvs = json.get("derivations").unwrap_or(json);
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

    // Structured attributes moved out of env.__json into their own
    // "structuredAttrs" object in format 4.
    let structured_attrs = match (d.get("structuredAttrs"), env.get(LEGACY_ATTRS_KEY)) {
        (Some(attrs), _) => Some(
            attrs
                .as_object()
                .context("structuredAttrs is not an object")?
                .clone(),
        ),
        (None, Some(text)) => Some(
            serde_json::from_str(text).context("parsing the derivation's structured attributes")?,
        ),
        (None, None) => None,
    };

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
        structured_attrs,
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
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    env.insert("PATH".into(), "/path-not-set".into());
    env.insert("HOME".into(), HOMELESS.into());
    env.insert("NIX_STORE".into(), STORE.into());
    env.insert("NIX_BUILD_CORES".into(), cores.to_string());

    // The derivation's own attributes: as environment variables, or as
    // files when the derivation uses structured attributes.
    let files = match &drv.structured_attrs {
        Some(attrs) => structured_files(drv, attrs, &mut env)?,
        None => env_files(drv, &mut env),
    };

    for var in ["NIX_BUILD_TOP", "TMPDIR", "TEMPDIR", "TMP", "TEMP", "PWD"] {
        env.insert(var.into(), BUILD_DIR.into());
    }
    env.insert("NIX_LOG_FD".into(), "2".into());
    env.insert("TERM".into(), "xterm-256color".into());

    // Output placeholders become the outputs' paths everywhere the build
    // can see them: the args, the environment and the files.
    let rewrites: Vec<(String, String)> = drv
        .outputs
        .iter()
        .map(|(name, path)| (placeholder(name), path.to_string_lossy().into_owned()))
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
        let path = format!("{BUILD_DIR}/.attr-{}", nix32(&Sha256::digest(k.as_bytes())));
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
    drv: &Derivation,
    attrs: &Map<String, Value>,
    env: &mut BTreeMap<String, String>,
) -> Result<Vec<JobFile>> {
    let mut attrs = attrs.clone();
    let outputs: Map<String, Value> = drv
        .outputs
        .iter()
        .map(|(name, path)| (name.clone(), Value::from(path.to_string_lossy())))
        .collect();
    attrs.insert("outputs".into(), Value::Object(outputs));

    let sh = format!("{BUILD_DIR}/{ATTRS_SH}");
    let json = format!("{BUILD_DIR}/{ATTRS_JSON}");
    env.insert(ATTRS_SH_VAR.into(), sh.clone());
    env.insert(ATTRS_JSON_VAR.into(), json.clone());
    Ok(vec![
        JobFile {
            path: sh,
            contents: attrs_sh(&attrs),
        },
        JobFile {
            path: json,
            contents: serde_json::to_string(&attrs)?,
        },
    ])
}

/// Structured attributes as bash declarations, following Nix's
/// StructuredAttrs::writeShell: strings, whole numbers, booleans and null
/// as scalars, arrays and objects of those as indexed and associative
/// arrays, and anything else, or any name that is not a shell variable's,
/// left out.
fn attrs_sh(attrs: &Map<String, Value>) -> String {
    let mut out = String::new();
    for (key, value) in attrs {
        if !is_shell_name(key) {
            continue;
        }

        // A scalar is declared as it is.
        if let Some(s) = shell_scalar(value) {
            out += &format!("declare {key}={s}\n");
            continue;
        }

        // An array or object is declared only if every element is a
        // scalar. Nix leaves a trailing space after the last element.
        match value {
            Value::Array(items) => {
                let Some(items) = items.iter().map(shell_scalar).collect::<Option<Vec<_>>>() else {
                    continue;
                };
                let body: String = items.iter().map(|s| format!("{s} ")).collect();
                out += &format!("declare -a {key}=({body})\n");
            }
            Value::Object(entries) => {
                let Some(body) = entries
                    .iter()
                    .map(|(k, v)| shell_scalar(v).map(|s| format!("[{}]={s} ", shell_quote(k))))
                    .collect::<Option<String>>()
                else {
                    continue;
                };
                out += &format!("declare -A {key}=({body})\n");
            }
            _ => {}
        }
    }
    out
}

/// A JSON scalar as a bash word, as Nix's writeShell renders one: a
/// string quoted, a whole number in decimal, true as 1, false and null
/// as empty. Fractions, arrays and objects have no rendering.
fn shell_scalar(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(shell_quote(s)),
        Value::Bool(true) => Some("1".into()),
        Value::Bool(false) => Some("".into()),
        Value::Null => Some("''".into()),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                return Some(i.to_string());
            }
            let f = n.as_f64()?;
            if f.fract() != 0.0 {
                return None;
            }
            Some(format!("{f:.0}"))
        }
        Value::Array(_) | Value::Object(_) => None,
    }
}

/// A string in single quotes, each quote in it closed, escaped and
/// reopened, as Nix's escapeShellArgAlways quotes it.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Whether `s` is a shell variable name: an ASCII letter or `_`, then
/// ASCII letters, digits and `_`.
fn is_shell_name(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The string `builtins.placeholder` returns for an output, which an
/// input-addressed derivation keeps in place of the output's path.
pub fn placeholder(output: &str) -> String {
    format!(
        "/{}",
        nix32(&Sha256::digest(format!("nix-output:{output}")))
    )
}

/// Replaces each output's placeholder in `s` with the output's path.
fn rewrite(s: &str, rewrites: &[(String, String)]) -> String {
    rewrites
        .iter()
        .fold(s.to_string(), |s, (from, to)| s.replace(from, to))
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
            structured_attrs: None,
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
            placeholder("out"),
            "/1rz4g4znpzjwh1xymhjpm42vipw92pr73vdgl6xs1hycac8kf2n9"
        );
    }

    #[test]
    fn placeholders_become_output_paths() {
        // An input-addressed derivation keeps `placeholder "out"` in its
        // env and args; nix-daemon swaps in the output path when the build
        // starts, in the env, the args and the files it writes. A job built
        // from a derivation using the placeholder in each of those places
        // sees the output path in each.
        let out = placeholder("out");
        let mut drv = sample();
        drv.args.push(format!("--prefix={out}"));
        drv.env.insert("flags".into(), format!("-DLIB={out}/lib"));
        drv.env.insert("text".into(), format!("{out}/share"));
        let plain = job(&drv, 1).unwrap();
        let env: BTreeMap<_, _> = plain.env.iter().cloned().collect();
        assert_eq!(env["flags"], "-DLIB=/nix/store/aaa-x/lib");
        assert_eq!(plain.argv.last().unwrap(), "--prefix=/nix/store/aaa-x");
        assert_eq!(plain.files[0].contents, "/nix/store/aaa-x/share");

        let mut drv = sample_structured();
        let attrs = drv.structured_attrs.as_mut().unwrap();
        attrs.insert("prefix".into(), Value::from(out));
        let structured = job(&drv, 1).unwrap();
        for file in &structured.files {
            assert!(!file.contents.contains("1rz4g4zn"), "{}", file.contents);
        }
        assert!(
            structured.files[0]
                .contents
                .contains("declare prefix='/nix/store/aaa-x'")
        );
    }

    fn sample_structured() -> Derivation {
        let attrs = serde_json::json!({
            "bad-name": "skipped",
            "doCheck": true,
            "env": {"FOO": "bar"},
            "list": ["a", "b'c"],
            "n": 3,
            "nested": {"a": ["x"]},
            "outputs": ["out"],
            "stdenv": "/nix/store/ddd-stdenv",
        });
        Derivation {
            structured_attrs: attrs.as_object().cloned(),
            ..sample()
        }
    }

    #[test]
    fn structured_attrs_reach_the_build_as_files() {
        // A derivation with structured attributes gets its attributes as
        // .attrs.sh and .attrs.json in the build directory, named by
        // NIX_ATTRS_SH_FILE and NIX_ATTRS_JSON_FILE, and none of its env.
        let job = job(&sample_structured(), 1).unwrap();
        let env: BTreeMap<_, _> = job.env.iter().cloned().collect();
        assert_eq!(env["NIX_ATTRS_SH_FILE"], "/build/.attrs.sh");
        assert_eq!(env["NIX_ATTRS_JSON_FILE"], "/build/.attrs.json");
        assert_eq!(env["HOME"], HOMELESS);
        assert!(!env.contains_key("out"));
        assert!(!env.contains_key("textPath"));
        assert!(!env.contains_key("FOO"));

        let files: BTreeMap<_, _> = job
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.contents.as_str()))
            .collect();
        assert_eq!(
            files["/build/.attrs.sh"],
            "declare doCheck=1\n\
             declare -A env=(['FOO']='bar' )\n\
             declare -a list=('a' 'b'\\''c' )\n\
             declare n=3\n\
             declare -A outputs=(['out']='/nix/store/aaa-x' )\n\
             declare stdenv='/nix/store/ddd-stdenv'\n"
        );
        let json: Value = serde_json::from_str(files["/build/.attrs.json"]).unwrap();
        assert_eq!(
            json["outputs"],
            serde_json::json!({"out": "/nix/store/aaa-x"})
        );
        assert_eq!(json["stdenv"], "/nix/store/ddd-stdenv");
    }

    #[test]
    fn structured_attrs_parse_from_every_format() {
        // Format 4 of `nix derivation show` carries structured attributes
        // under "structuredAttrs"; older formats carry them as JSON text in
        // env.__json. Both parse to the same attributes.
        let v4 = serde_json::json!({
            "version": 4,
            "derivations": {"bbb-x.drv": {
                "name": "x", "system": "x86_64-linux", "builder": "/bin/sh",
                "args": [], "env": {"out": "/nix/store/aaa-x"},
                "outputs": {"out": {"path": "aaa-x"}},
                "inputs": {"srcs": [], "drvs": {}},
                "structuredAttrs": {"stdenv": "/nix/store/ddd-stdenv"},
            }},
        });
        let v3 = serde_json::json!({"/nix/store/bbb-x.drv": {
            "name": "x", "system": "x86_64-linux", "builder": "/bin/sh",
            "args": [],
            "env": {"out": "/nix/store/aaa-x", "__json": "{\"stdenv\":\"/nix/store/ddd-stdenv\"}"},
            "outputs": {"out": {"path": "/nix/store/aaa-x"}},
            "inputSrcs": [], "inputDrvs": {},
        }});
        let want = serde_json::json!({"stdenv": "/nix/store/ddd-stdenv"});
        for json in [v4, v3] {
            let drv = parse(Path::new("/nix/store/bbb-x.drv"), &json).unwrap();
            assert_eq!(drv.structured_attrs.as_ref(), want.as_object());
        }
    }
}
