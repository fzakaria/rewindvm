//! Nix derivations as runs.
//!
//! A derivation already names everything its build may see: a builder,
//! its arguments and environment, and the store paths it depends on. So a
//! derivation is a run spec almost as it stands. The input closure becomes
//! the image the guest mounts at /nix/store, the builder becomes the job,
//! and the environment is set up the way the Nix sandbox sets it up, so
//! the builder cannot tell the difference.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use nix_derivation::store_path::hash_placeholder;
use nix_derivation::{NixHash, StorePath, StructuredAttrsFiles, nixbase32};
use rewind_init::{Job, JobFile, Root};
use serde_json::{Value, json};
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
    /// Whether the output is fixed by a hash given up front, as a
    /// fetcher's is.
    pub fixed_output: bool,
    /// The store paths whose closures exportReferencesGraph asks for, by
    /// the name the build reads each closure under.
    pub reference_graphs: BTreeMap<String, BTreeSet<PathBuf>>,
    /// The derivation as Nix wrote it, which the .attrs.json and .attrs.sh
    /// of `__structuredAttrs = true` are written from.
    parsed: nix_derivation::Derivation,
}

/// What a store holds about one path of a reference graph's closure, as
/// nix-daemon hands it to the build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathInfo {
    pub path: PathBuf,
    /// The hash of the path's NAR serialisation, as `sha256:<nix32>`.
    pub nar_hash: String,
    pub nar_size: u64,
    pub references: BTreeSet<PathBuf>,
    /// The path's content address, for a content-addressed path.
    pub ca: Option<String>,
    /// The NAR size of the path's whole closure.
    pub closure_size: u64,
}

/// Each closure exportReferencesGraph asks for, by name, in path order.
pub type ReferenceGraphs = BTreeMap<String, Vec<PathInfo>>;

/// The attribute naming the closures a derivation wants described.
const EXPORT_REFERENCES_GRAPH: &str = "exportReferencesGraph";

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

    let fixed_output = parsed.is_fixed_output()?;
    let reference_graphs = requested_graphs(&parsed, &env)
        .with_context(|| format!("reading {name}'s {EXPORT_REFERENCES_GRAPH}"))?;

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
        fixed_output,
        reference_graphs,
        parsed,
    })
}

/// The closures exportReferencesGraph asks for: with structured
/// attributes, an object from each name to store paths, in lists nested
/// any depth; otherwise, pairs of a file name and a store path in the
/// environment. nix-daemon ignores the attribute when the object is not
/// one.
fn requested_graphs(
    parsed: &nix_derivation::Derivation,
    env: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, BTreeSet<PathBuf>>> {
    let mut graphs = BTreeMap::new();

    if let Some(attrs) = parsed.structured_attrs() {
        let Some(Value::Object(requested)) = attrs.get(EXPORT_REFERENCES_GRAPH) else {
            return Ok(graphs);
        };
        for (name, value) in requested {
            let mut roots = BTreeSet::new();
            flatten_store_paths(value, &mut roots).with_context(|| format!("graph {name}"))?;
            graphs.insert(name.clone(), roots);
        }
        return Ok(graphs);
    }

    let Some(value) = env.get(EXPORT_REFERENCES_GRAPH) else {
        return Ok(graphs);
    };
    let tokens: Vec<&str> = value.split_whitespace().collect();
    let (pairs, odd) = tokens.as_chunks::<2>();
    if !odd.is_empty() {
        bail!("odd number of tokens in {value:?}");
    }
    for [name, root] in pairs {
        if !is_graph_file_name(name) {
            bail!("invalid file name {name}");
        }
        graphs.insert(name.to_string(), BTreeSet::from([store_path(root)?]));
    }
    Ok(graphs)
}

/// Collects the store paths in `value`, a store path or lists of them.
fn flatten_store_paths(value: &Value, out: &mut BTreeSet<PathBuf>) -> Result<()> {
    match value {
        Value::String(s) => {
            out.insert(store_path(s)?);
        }
        Value::Array(values) => {
            for v in values {
                flatten_store_paths(v, out)?;
            }
        }
        _ => bail!("{value} is not a store path"),
    }
    Ok(())
}

/// `s` as a store path, refusing anything else, a path inside one
/// included.
fn store_path(s: &str) -> Result<PathBuf> {
    StorePath::from_absolute_path(s.as_bytes())
        .with_context(|| format!("{s} is not a store path"))?;
    Ok(PathBuf::from(s))
}

/// Whether `name` may name a reference graph's file, as nix-daemon checks
/// it: an ASCII letter or `_`, then ASCII letters, digits, `_`, `.` and
/// `-`.
fn is_graph_file_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b) if b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
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

/// The closures the derivation's exportReferencesGraph asks for, from the
/// host's store. Each root must be in `input_closure`, as nix-daemon
/// requires.
pub fn reference_graphs(drv: &Derivation, input_closure: &[PathBuf]) -> Result<ReferenceGraphs> {
    if drv.reference_graphs.is_empty() {
        return Ok(ReferenceGraphs::new());
    }

    // What the store holds about every root's closure.
    let roots: BTreeSet<&PathBuf> = drv.reference_graphs.values().flatten().collect();
    let mut infos = query_path_infos(roots.iter().copied())?;

    // nix-daemon adds the closure of the outputs of each derivation in a
    // graph, so a graph of build inputs also describes what they build.
    let mut drv_outputs = BTreeMap::new();
    for path in infos
        .keys()
        .filter(|p| p.extension().is_some_and(|e| e == "drv"))
    {
        let outputs: Vec<PathBuf> = show(path)?.outputs.into_iter().map(|(_, p)| p).collect();
        drv_outputs.insert(path.clone(), outputs);
    }
    let outputs: BTreeSet<&PathBuf> = drv_outputs.values().flatten().collect();
    if !outputs.is_empty() {
        infos.extend(query_path_infos(outputs.into_iter())?);
    }

    closures(drv, input_closure, &infos, &drv_outputs)
}

/// Each requested graph's closure, given what the store holds about every
/// path in them and the outputs of each derivation among them, as
/// nix-daemon's exportReferences computes it.
fn closures(
    drv: &Derivation,
    input_closure: &[PathBuf],
    infos: &BTreeMap<PathBuf, PathInfo>,
    drv_outputs: &BTreeMap<PathBuf, Vec<PathBuf>>,
) -> Result<ReferenceGraphs> {
    let inputs: BTreeSet<&PathBuf> = input_closure.iter().collect();
    let mut graphs = ReferenceGraphs::new();
    for (name, roots) in &drv.reference_graphs {
        if let Some(root) = roots.iter().find(|r| !inputs.contains(r)) {
            bail!(
                "cannot export references of {} because it is not in the input closure of {}",
                root.display(),
                drv.name
            );
        }

        let mut closure = closure_of(roots.iter().cloned(), infos)?;
        let outputs: Vec<PathBuf> = closure
            .iter()
            .filter_map(|p| drv_outputs.get(p))
            .flatten()
            .cloned()
            .collect();
        closure.extend(closure_of(outputs.into_iter(), infos)?);
        graphs.insert(
            name.clone(),
            closure.iter().map(|p| infos[p].clone()).collect(),
        );
    }
    Ok(graphs)
}

/// The paths `roots` refer to, directly or not, and the roots themselves.
fn closure_of(
    roots: impl Iterator<Item = PathBuf>,
    infos: &BTreeMap<PathBuf, PathInfo>,
) -> Result<BTreeSet<PathBuf>> {
    let mut closure = BTreeSet::new();
    let mut todo: Vec<PathBuf> = roots.collect();
    while let Some(path) = todo.pop() {
        if !closure.insert(path.clone()) {
            continue;
        }
        let info = infos
            .get(&path)
            .with_context(|| format!("the store has nothing on {}", path.display()))?;
        todo.extend(info.references.iter().cloned());
    }
    Ok(closure)
}

/// What the store holds about `paths` and everything they refer to.
fn query_path_infos<'a>(
    paths: impl Iterator<Item = &'a PathBuf>,
) -> Result<BTreeMap<PathBuf, PathInfo>> {
    let paths: Vec<String> = paths.map(|p| p.to_string_lossy().into_owned()).collect();
    let mut args = vec!["path-info", "--json", "--closure-size", "--recursive"];
    args.extend(paths.iter().map(String::as_str));
    let json: Value = serde_json::from_str(&nix(&args)?).context("parsing nix path-info --json")?;
    path_infos(&json)
}

/// The paths in the output of `nix path-info --json --closure-size`: an
/// object keyed by path since Nix 2.19, a list of objects with a "path"
/// before it. Hashes come in SRI form or as `sha256:<nix32>`.
fn path_infos(json: &Value) -> Result<BTreeMap<PathBuf, PathInfo>> {
    let entries: Vec<(&str, &Value)> = match json {
        Value::Object(map) => map.iter().map(|(k, v)| (k.as_str(), v)).collect(),
        Value::Array(list) => list
            .iter()
            .map(|v| Ok((v["path"].as_str().context("a path has no \"path\"")?, v)))
            .collect::<Result<_>>()?,
        _ => bail!("nix path-info printed neither an object nor a list"),
    };

    let mut infos = BTreeMap::new();
    for (path, v) in entries {
        let field = |key: &str| v.get(key).with_context(|| format!("{path} has no {key}"));
        let nar_hash = field("narHash")?
            .as_str()
            .context("narHash is not a string")?;
        let nar_hash = NixHash::parse(nar_hash)
            .with_context(|| format!("{path} has narHash {nar_hash}"))?
            .to_nix_nixbase32_string();
        let references = field("references")?
            .as_array()
            .context("references is not a list")?
            .iter()
            .map(|r| {
                r.as_str()
                    .map(PathBuf::from)
                    .context("a reference is not a string")
            })
            .collect::<Result<_>>()?;
        let info = PathInfo {
            path: PathBuf::from(path),
            nar_hash,
            nar_size: field("narSize")?
                .as_u64()
                .context("narSize is not a number")?,
            references,
            ca: v.get("ca").and_then(Value::as_str).map(String::from),
            closure_size: field("closureSize")?
                .as_u64()
                .context("closureSize is not a number")?,
        };
        infos.insert(info.path.clone(), info);
    }
    Ok(infos)
}

/// The builder as a job, with the environment the Nix sandbox gives it.
/// The order of operations follows nix-daemon's initEnv, so a derivation
/// that overrides one of these sees its own value. `cores` is the
/// NIX_BUILD_CORES the build sees, the run's CPU count. `graphs` are the
/// closures exportReferencesGraph asks for, from [`reference_graphs`].
pub fn job(drv: &Derivation, graphs: &ReferenceGraphs, cores: u32) -> Result<Job> {
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
    let files = match drv.parsed.structured_attrs() {
        Some(_) => structured_files(drv, graphs, &mut env)?,
        None => env_files(drv, graphs, &mut env),
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
/// directory and replaced by a variable holding the file's path. Each
/// reference graph is a file of its own name, in the form
/// `nix-store --register-validity` reads.
fn env_files(
    drv: &Derivation,
    graphs: &ReferenceGraphs,
    env: &mut BTreeMap<String, String>,
) -> Vec<JobFile> {
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

    for (name, closure) in graphs {
        files.push(JobFile {
            path: format!("{BUILD_DIR}/{name}"),
            contents: validity_registration(closure),
        });
    }
    files
}

/// `closure` as Nix's makeValidityRegistration writes it without hashes or
/// derivers: each path, an empty deriver line, the number of references,
/// then the references.
fn validity_registration(closure: &[PathInfo]) -> String {
    let mut out = String::new();
    for info in closure {
        out += &format!("{}\n\n{}\n", info.path.display(), info.references.len());
        for r in &info.references {
            out += &format!("{}\n", r.display());
        }
    }
    out
}

/// `closure` as nix-daemon's pathInfoToJSON writes it into .attrs.json,
/// a format Nix keeps fixed for builds' sake.
fn graph_json(closure: &[PathInfo]) -> Value {
    let paths = closure
        .iter()
        .map(|info| {
            let mut entry = json!({
                "closureSize": info.closure_size,
                "narHash": info.nar_hash,
                "narSize": info.nar_size,
                "path": info.path,
                "references": info.references,
                "valid": true,
            });
            if let Some(ca) = &info.ca {
                entry["ca"] = json!(ca);
            }
            entry
        })
        .collect();
    Value::Array(paths)
}

/// Structured attributes as nix-daemon hands them to the build: written
/// to .attrs.json and, as bash declarations, to .attrs.sh, with "outputs"
/// replaced by each output's path. The env is not set at all; stdenv
/// exports the attributes it needs from .attrs.sh.
fn structured_files(
    drv: &Derivation,
    graphs: &ReferenceGraphs,
    env: &mut BTreeMap<String, String>,
) -> Result<Vec<JobFile>> {
    let graphs: BTreeMap<String, Value> = graphs
        .iter()
        .map(|(name, closure)| (name.clone(), graph_json(closure)))
        .collect();
    let attrs: StructuredAttrsFiles = drv
        .parsed
        .structured_attrs_files_with_reference_graphs(&graphs)
        .with_context(|| format!("writing {}'s structured attributes", drv.name))?
        .context("structured attributes went missing")?;

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

/// Runs a nix command and returns its standard output.
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

    /// A derivation passing `text` as a file and overriding HOME.
    fn sample() -> Derivation {
        let env = [
            ("text", "hello"),
            ("passAsFile", "text"),
            ("HOME", "/overridden"),
        ];
        parse(Path::new(FIXTURE_DRV), &fixture(&env)).unwrap()
    }

    /// No reference graphs, for a derivation that asks for none.
    fn no_graphs() -> ReferenceGraphs {
        ReferenceGraphs::new()
    }

    #[test]
    fn job_env_gives_the_build_the_cores_asked_for() {
        // A build asked to use 4 cores sees NIX_BUILD_CORES=4, which stdenv
        // passes to make, ninja and the test runners as their job count.
        let job = job(&sample(), &no_graphs(), 4).unwrap();
        let env: BTreeMap<_, _> = job.env.iter().cloned().collect();
        assert_eq!(env["NIX_BUILD_CORES"], "4");
    }

    #[test]
    fn job_env_follows_the_sandbox() {
        let job = job(&sample(), &no_graphs(), 1).unwrap();
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
        assert_eq!(job.argv, vec!["/bin/sh", "-e", "builder.sh"]);
        assert_eq!(job.outputs, vec![FIXTURE_OUT.to_string()]);
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
    // these are real-looking paths.
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
        let env: BTreeMap<_, _> = job(&fixed, &no_graphs(), 1)
            .unwrap()
            .env
            .into_iter()
            .collect();
        assert_eq!(env["NIX_OUTPUT_CHECKED"], "1");

        let plain = parse(Path::new(FIXTURE_DRV), &fixture(&[])).unwrap();
        let env: BTreeMap<_, _> = job(&plain, &no_graphs(), 1)
            .unwrap()
            .env
            .into_iter()
            .collect();
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
        let err = job(&drv, &no_graphs(), 1).unwrap_err().to_string();
        assert!(err.contains("builtin:fetchurl"), "{err}");
    }

    fn sample_structured(attrs: Value) -> Derivation {
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
        assert!(drv.parsed.structured_attrs().is_none());
        assert!(drv.reference_graphs.is_empty());
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
        let plain = job(&drv, &no_graphs(), 1).unwrap();
        let env: BTreeMap<_, _> = plain.env.iter().cloned().collect();
        assert_eq!(env["flags"], format!("-DLIB={FIXTURE_OUT}/lib"));
        assert_eq!(
            *plain.argv.last().unwrap(),
            format!("--prefix={FIXTURE_OUT}")
        );
        assert_eq!(plain.files[0].contents, format!("{FIXTURE_OUT}/share"));

        let drv = sample_structured(serde_json::json!({"outputs": ["out"], "prefix": out}));
        let structured = job(&drv, &no_graphs(), 1).unwrap();
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
        let job = job(&drv, &no_graphs(), 1).unwrap();
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

    // More store paths for the reference graph tests: a root, what it
    // refers to, a derivation in its closure and that derivation's output.
    const GRAPH_ROOT: &str = "/nix/store/0000000000000000000000000000000f-root";
    const GRAPH_LIB: &str = "/nix/store/0000000000000000000000000000000g-lib";
    const GRAPH_DRV: &str = "/nix/store/0000000000000000000000000000000h-tool.drv";
    const GRAPH_TOOL: &str = "/nix/store/0000000000000000000000000000000i-tool";
    const NAR_HASH: &str = "sha256:1b8m03r63zqhnjf7l5wnldhh7c134ap5vpj0850ymkq1iyzicy5s";

    fn info(path: &str, references: &[&str]) -> PathInfo {
        PathInfo {
            path: PathBuf::from(path),
            nar_hash: NAR_HASH.into(),
            nar_size: 8,
            references: references.iter().map(PathBuf::from).collect(),
            ca: None,
            closure_size: 8 * (references.len() as u64 + 1),
        }
    }

    #[test]
    fn reference_graphs_are_read_from_either_form() {
        // Without structured attributes the graphs are pairs of a file name
        // and a store path; with them, an object of store paths in lists
        // nested any depth. A name nix-daemon would refuse, a path inside a
        // store path and an odd pair are errors.
        let plain = parse(
            Path::new(FIXTURE_DRV),
            &fixture(&[(
                "exportReferencesGraph",
                &format!("closure {GRAPH_ROOT}\n deps {GRAPH_LIB}"),
            )]),
        )
        .unwrap();
        assert_eq!(
            plain.reference_graphs,
            BTreeMap::from([
                (
                    "closure".to_string(),
                    BTreeSet::from([PathBuf::from(GRAPH_ROOT)])
                ),
                (
                    "deps".to_string(),
                    BTreeSet::from([PathBuf::from(GRAPH_LIB)])
                ),
            ])
        );

        let structured = sample_structured(json!({
            "exportReferencesGraph": {"closure": [GRAPH_ROOT, [[GRAPH_LIB]]]},
        }));
        assert_eq!(
            structured.reference_graphs,
            BTreeMap::from([(
                "closure".to_string(),
                BTreeSet::from([PathBuf::from(GRAPH_ROOT), PathBuf::from(GRAPH_LIB)])
            )])
        );

        for bad in [
            format!(".hidden {GRAPH_ROOT}"),
            format!("closure {GRAPH_ROOT}/bin"),
            format!("closure {GRAPH_ROOT} deps"),
        ] {
            let result = parse(
                Path::new(FIXTURE_DRV),
                &fixture(&[("exportReferencesGraph", &bad)]),
            );
            assert!(result.is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn path_infos_read_every_path_info_format() {
        // nix path-info --json prints an object keyed by path with SRI
        // hashes since Nix 2.19, and a list with "path" in each entry
        // before. Both read to the same paths, with the hash as
        // sha256:<nix32>.
        let sri = "sha256-ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0=";
        let entry = json!({
            "narHash": sri, "narSize": 8, "references": [GRAPH_LIB],
            "ca": null, "closureSize": 16,
        });
        let object = json!({GRAPH_ROOT: entry});
        let mut listed = entry.clone();
        listed["path"] = json!(GRAPH_ROOT);
        listed["narHash"] = json!(NAR_HASH);
        for json in [object, Value::Array(vec![listed])] {
            let infos = path_infos(&json).unwrap();
            assert_eq!(
                infos[&PathBuf::from(GRAPH_ROOT)],
                info(GRAPH_ROOT, &[GRAPH_LIB])
            );
        }
    }

    #[test]
    fn closures_follow_references_and_derivation_outputs() {
        // A graph holds its root's closure, plus the closure of the outputs
        // of any derivation in it, as nix-daemon's exportReferences does.
        // A root outside the input closure is refused.
        let mut drv = sample();
        drv.reference_graphs = BTreeMap::from([(
            "closure".to_string(),
            BTreeSet::from([PathBuf::from(GRAPH_ROOT)]),
        )]);
        let infos: BTreeMap<PathBuf, PathInfo> = [
            info(GRAPH_ROOT, &[GRAPH_LIB, GRAPH_DRV]),
            info(GRAPH_LIB, &[]),
            info(GRAPH_DRV, &[]),
            info(GRAPH_TOOL, &[GRAPH_LIB]),
        ]
        .into_iter()
        .map(|i| (i.path.clone(), i))
        .collect();
        let drv_outputs =
            BTreeMap::from([(PathBuf::from(GRAPH_DRV), vec![PathBuf::from(GRAPH_TOOL)])]);
        let input_closure = vec![PathBuf::from(GRAPH_ROOT), PathBuf::from(GRAPH_LIB)];

        let graphs = closures(&drv, &input_closure, &infos, &drv_outputs).unwrap();
        let paths: Vec<&Path> = graphs["closure"].iter().map(|i| i.path.as_path()).collect();
        let want: Vec<&Path> = [GRAPH_ROOT, GRAPH_LIB, GRAPH_DRV, GRAPH_TOOL]
            .map(Path::new)
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        assert_eq!(paths, want);

        let err = closures(&drv, &[], &infos, &drv_outputs)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not in the input closure"), "{err}");
    }

    #[test]
    fn reference_graphs_reach_the_build_as_nix_writes_them() {
        // Without structured attributes a graph is a file in the build
        // directory, in the form nix-store --register-validity reads; with
        // them, a list under the graph's name in .attrs.json, in the shape
        // nix-daemon's pathInfoToJSON writes.
        let closure = vec![info(GRAPH_LIB, &[]), info(GRAPH_ROOT, &[GRAPH_LIB])];
        let graphs = ReferenceGraphs::from([("closure".to_string(), closure)]);

        let plain = parse(
            Path::new(FIXTURE_DRV),
            &fixture(&[("exportReferencesGraph", &format!("closure {GRAPH_ROOT}"))]),
        )
        .unwrap();
        let job_files = job(&plain, &graphs, 1).unwrap().files;
        let file = job_files
            .iter()
            .find(|f| f.path == "/build/closure")
            .unwrap();
        assert_eq!(
            file.contents,
            format!("{GRAPH_LIB}\n\n0\n{GRAPH_ROOT}\n\n1\n{GRAPH_LIB}\n")
        );

        let structured = sample_structured(json!({
            "exportReferencesGraph": {"closure": [GRAPH_ROOT]},
            "outputs": ["out"],
        }));
        let job_files = job(&structured, &graphs, 1).unwrap().files;
        let attrs = job_files
            .iter()
            .find(|f| f.path == "/build/.attrs.json")
            .unwrap();
        assert!(
            attrs.contents.starts_with(&format!(
                r#"{{"closure":[{{"closureSize":8,"narHash":"{NAR_HASH}","narSize":8,"path":"{GRAPH_LIB}","references":[],"valid":true}},{{"closureSize":16,"narHash":"{NAR_HASH}","narSize":8,"path":"{GRAPH_ROOT}","references":["{GRAPH_LIB}"],"valid":true}}],"exportReferencesGraph""#
            )),
            "{}",
            attrs.contents
        );
    }
}
