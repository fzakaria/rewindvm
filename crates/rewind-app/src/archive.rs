//! Exported runs: `.rwd` files.
//!
//! `rewind export` writes a run as one file: a tar archive compressed with
//! zstd holding `manifest.json` and `trace.bin`, and for a replayable
//! export also keyframes, memory pages and the run's inputs (see
//! crates/rewind-core/src/export.rs). The scrubber needs only the manifest
//! and the trace, so the app unpacks those two into the user's cache,
//! under the run's id, and opens that directory like any other run. The
//! rest stays in the file for `rewind import`, which can replay it.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::run::{MANIFEST_FILE, TRACE_FILE};

/// The first four bytes of every zstd frame.
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];

/// Where imported runs are unpacked, under the XDG cache directory.
const CACHE_SUBDIR: &str = "rewind/imported";
const XDG_CACHE_ENV: &str = "XDG_CACHE_HOME";
const HOME_CACHE_DIR: &str = ".cache";

/// The name used for a run whose manifest has no id.
const NO_ID: &str = "unnamed";

/// Whether a file is a `.rwd` export: a zstd stream, whatever its name.
pub fn is_export(path: &Path) -> bool {
    let mut magic = [0u8; ZSTD_MAGIC.len()];
    let read = File::open(path).and_then(|mut f| f.read_exact(&mut magic));
    read.is_ok() && magic == ZSTD_MAGIC
}

/// The directory runs are unpacked into: $XDG_CACHE_HOME/rewind/imported,
/// else ~/.cache/rewind/imported.
pub fn cache_dir() -> Option<PathBuf> {
    let cache = std::env::var_os(XDG_CACHE_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(HOME_CACHE_DIR)))?;
    Some(cache.join(CACHE_SUBDIR))
}

/// Unpacks an export file into the cache and returns the run's directory.
pub fn import_file(path: &Path) -> Result<PathBuf> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let into = cache_dir().context("no HOME to unpack the run into")?;
    import(file, &into).with_context(|| format!("unpacking {}", path.display()))
}

/// Unpacks an export held in memory, like the examples compiled into the
/// app, into the cache.
pub fn import_bytes(bytes: &[u8]) -> Result<PathBuf> {
    let into = cache_dir().context("no HOME to unpack the run into")?;
    import(bytes, &into)
}

/// Unpacks the manifest and trace of an export read from `reader` into
/// `<into>/<manifest id>/`. A run already unpacked there is reused as it
/// is: its id is the hash of its inputs, and runs are deterministic, so
/// the same id means the same run.
pub fn import(reader: impl Read, into: &Path) -> Result<PathBuf> {
    fs::create_dir_all(into).with_context(|| format!("creating {}", into.display()))?;

    // Unpack into a staging directory first; the run's id is known only
    // once the manifest has been read, and it may not come first.
    let staging = into.join(format!(".staging-{}-{}", std::process::id(), unique()));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;
    let result = unpack(reader, &staging).and_then(|()| place(&staging, into));
    let _ = fs::remove_dir_all(&staging);
    result
}

/// A number that differs between calls in one process, for staging names.
fn unique() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Writes the archive's manifest and trace into `staging`. Every other
/// entry is skipped; a path that is not plain names inside the archive is
/// refused.
fn unpack(reader: impl Read, staging: &Path) -> Result<()> {
    let decoder = zstd::Decoder::new(reader).context("not a zstd stream")?;
    let mut archive = tar::Archive::new(decoder);
    for entry in archive.entries().context("not a tar archive")? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        if path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        {
            bail!("the archive holds an unsafe path {}", path.display());
        }
        let wanted = path == Path::new(MANIFEST_FILE) || path == Path::new(TRACE_FILE);
        if !wanted {
            continue;
        }
        let mut out = File::create(staging.join(&path))?;
        io::copy(&mut entry, &mut out)?;
    }
    for name in [MANIFEST_FILE, TRACE_FILE] {
        if !staging.join(name).is_file() {
            bail!("the archive has no {name}");
        }
    }
    Ok(())
}

/// Moves a staged run to `<into>/<id>`, or keeps the copy already there.
fn place(staging: &Path, into: &Path) -> Result<PathBuf> {
    let manifest: Value = serde_json::from_slice(&fs::read(staging.join(MANIFEST_FILE))?)
        .context("the archive's manifest.json is not JSON")?;
    let id = manifest
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| is_safe_name(id))
        .unwrap_or(NO_ID)
        .to_string();
    let dir = into.join(&id);
    if dir.join(MANIFEST_FILE).is_file() && dir.join(TRACE_FILE).is_file() {
        return Ok(dir);
    }
    let _ = fs::remove_dir_all(&dir);
    fs::rename(staging, &dir).with_context(|| format!("moving the run to {}", dir.display()))?;
    Ok(dir)
}

/// A run id that is safe as one path component.
fn is_safe_name(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// One cache directory for every test in this binary that opens exports,
/// set once: tests run in parallel and share the process environment, so
/// each setting XDG_CACHE_HOME to its own directory would race.
#[cfg(test)]
pub(crate) fn test_cache() -> PathBuf {
    static ONCE: std::sync::Once = std::sync::Once::new();
    let dir = std::env::temp_dir().join(format!("rewind-app-cache-{}", std::process::id()));
    ONCE.call_once(|| {
        // SAFETY: this is the only place the tests set the variable, once,
        // before any test reads it through cache_dir.
        unsafe { std::env::set_var(XDG_CACHE_ENV, &dir) };
    });
    dir
}

#[cfg(test)]
mod tests {
    // Archives built in the test the way the engine builds them: a tar of
    // named entries compressed with zstd, unpacked into a temporary
    // directory.
    use super::*;

    /// zstd's default level, as the engine uses.
    const LEVEL: i32 = 3;

    fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let encoder = zstd::Encoder::new(Vec::new(), LEVEL).unwrap();
        let mut tar = tar::Builder::new(encoder);
        for (name, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, name, *data).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap()
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rewind-app-archive-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_export_unpacks_under_its_id_without_the_replay_data() {
        // The manifest comes after the trace, and pages, keyframes and
        // inputs are left in the archive.
        let dir = temp_dir("unpack");
        let bytes = archive(&[
            ("trace.bin", b"trace"),
            ("keyframes/0000000000000100.kf", b"kf"),
            ("pages/00ff", &[0u8; 16]),
            ("inputs/kernel", b"kernel"),
            ("manifest.json", br#"{"id": "3e7358ddd62c45d3"}"#),
        ]);
        let run = import(&bytes[..], &dir).unwrap();
        assert_eq!(run, dir.join("3e7358ddd62c45d3"));
        assert_eq!(fs::read(run.join("trace.bin")).unwrap(), b"trace");
        assert!(!run.join("pages").exists() && !run.join("inputs").exists());
        let left: Vec<_> = fs::read_dir(&dir).unwrap().collect();
        assert_eq!(left.len(), 1, "staging left behind");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_run_already_unpacked_is_reused() {
        // A second import of the same id keeps the first copy.
        let dir = temp_dir("reuse");
        let first = archive(&[
            ("manifest.json", br#"{"id": "abc"}"#),
            ("trace.bin", b"one"),
        ]);
        let second = archive(&[
            ("manifest.json", br#"{"id": "abc"}"#),
            ("trace.bin", b"two"),
        ]);
        import(&first[..], &dir).unwrap();
        let run = import(&second[..], &dir).unwrap();
        assert_eq!(fs::read(run.join("trace.bin")).unwrap(), b"one");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn broken_and_hostile_archives_are_refused() {
        // A missing trace, a path that climbs out, and bytes that are not
        // zstd at all; an id that is not a plain name is replaced.
        let dir = temp_dir("refuse");
        let no_trace = archive(&[("manifest.json", br#"{"id": "x"}"#)]);
        assert!(import(&no_trace[..], &dir).is_err());
        // tar refuses to write "..", so that name goes into the header raw.
        let climbing = {
            let mut tar = tar::Builder::new(zstd::Encoder::new(Vec::new(), LEVEL).unwrap());
            let mut header = tar::Header::new_gnu();
            header.as_gnu_mut().unwrap().name[..7].copy_from_slice(b"../evil");
            header.set_size(1);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append(&header, &b"x"[..]).unwrap();
            tar.into_inner().unwrap().finish().unwrap()
        };
        assert!(import(&climbing[..], &dir).is_err());
        assert!(import(&b"plain text"[..], &dir).is_err());
        let odd_id = archive(&[
            ("manifest.json", br#"{"id": "../up"}"#),
            ("trace.bin", b"t"),
        ]);
        assert_eq!(import(&odd_id[..], &dir).unwrap(), dir.join(NO_ID));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn exports_are_known_by_their_first_bytes() {
        // A zstd stream is an export whatever it is called; a trace is not.
        let dir = temp_dir("magic");
        fs::write(dir.join("run.bin"), archive(&[])).unwrap();
        fs::write(dir.join("trace.rwd"), b"not zstd").unwrap();
        assert!(is_export(&dir.join("run.bin")));
        assert!(!is_export(&dir.join("trace.rwd")));
        fs::remove_dir_all(&dir).unwrap();
    }
}
