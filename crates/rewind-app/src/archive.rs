//! Exported runs: `.rwd` files.
//!
//! `rewind export` writes a run as one file: a tar archive compressed with
//! zstd holding `manifest.json`, `trace.bin` and any `bookmarks.json`, and
//! for a replayable export also keyframes, memory pages and the run's
//! inputs (see crates/rewind-core/src/export.rs). The scrubber needs only
//! the manifest, the trace and the bookmarks, so the app unpacks those into
//! the user's cache, under the run's id, and opens that directory like any
//! other run. The rest stays in the file for `rewind import`, which can
//! replay it.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::bookmarks::{BOOKMARKS_FILE, MAX_BYTES as BOOKMARKS_MAX_BYTES};
use rewind_trace::export;
use rewind_trace::manifest::{MANIFEST, RunId, TRACE};

/// The first four bytes of every zstd frame.
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];

/// Where imported runs are unpacked, under the XDG cache directory.
const CACHE_SUBDIR: &str = "rewind/imported";
const XDG_CACHE_ENV: &str = "XDG_CACHE_HOME";
const HOME_CACHE_DIR: &str = ".cache";

/// What a run can be named by instead of a path: http and https URLs of
/// .rwd files.
const URL_SCHEMES: &[&str] = &["https://", "http://"];

/// Where downloaded exports are kept, next to the unpacked runs, so the
/// engine can import one without downloading it again.
const DOWNLOADS_SUBDIR: &str = "rewind/downloads";

/// An export unpacked into the cache.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unpacked {
    /// The run's directory, with its manifest and trace.
    pub dir: PathBuf,
    /// Whether the export also holds what replay needs, which the engine
    /// can import.
    pub replayable: bool,
}

/// The name used for a run whose manifest has no id.
const NO_ID: &str = "unnamed";

/// The name a download goes by when its URL ends in no usable file name.
const DOWNLOAD_NAME: &str = "download.rwd";

/// Whether a file is a `.rwd` export: a zstd stream, whatever its name.
pub fn is_export(path: &Path) -> bool {
    let mut magic = [0u8; ZSTD_MAGIC.len()];
    let read = File::open(path).and_then(|mut f| f.read_exact(&mut magic));
    read.is_ok() && magic == ZSTD_MAGIC
}

/// The directory runs are unpacked into: $XDG_CACHE_HOME/rewind/imported,
/// else ~/.cache/rewind/imported.
pub fn cache_dir() -> Option<PathBuf> {
    Some(cache_root()?.join(CACHE_SUBDIR))
}

/// The directory downloaded exports are kept in.
pub fn downloads_dir() -> Option<PathBuf> {
    Some(cache_root()?.join(DOWNLOADS_SUBDIR))
}

/// $XDG_CACHE_HOME, else ~/.cache.
fn cache_root() -> Option<PathBuf> {
    std::env::var_os(XDG_CACHE_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(HOME_CACHE_DIR)))
}

/// Unpacks an export file into the cache.
pub fn import_file(path: &Path) -> Result<Unpacked> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let into = cache_dir().context("no HOME to unpack the run into")?;
    import(file, &into).with_context(|| format!("unpacking {}", path.display()))
}

/// Whether `path` is a URL of an export rather than a path on disk.
pub fn is_url(path: &Path) -> bool {
    let name = path.to_string_lossy();
    URL_SCHEMES.iter().any(|scheme| name.starts_with(scheme))
}

/// Downloads an export from `url` into the downloads directory and
/// returns the file. A file of the same name and length already there is
/// taken as that download, so opening a URL twice fetches it once.
pub fn download(url: &str) -> Result<PathBuf> {
    let dir = downloads_dir().context("no HOME to download the run into")?;
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let name = download_name(url);
    let file = dir.join(name);

    let response = ureq::get(url)
        .call()
        .with_context(|| format!("downloading {url}"))?;
    let length = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let have = fs::metadata(&file).ok().map(|m| m.len());
    if length.is_some() && have == length {
        return Ok(file);
    }

    // Written under another name and renamed whole, so a download cut
    // short never passes for the file.
    let partial = dir.join(format!(".{name}.{}-{}", std::process::id(), unique()));
    let written = File::create(&partial)
        .and_then(|mut out| io::copy(&mut response.into_body().into_reader(), &mut out))
        .and_then(|_| fs::rename(&partial, &file));
    if let Err(e) = written {
        let _ = fs::remove_file(&partial);
        return Err(e).with_context(|| format!("downloading {url}"));
    }
    Ok(file)
}

/// Unpacks an export held in memory, like the examples compiled into the
/// app, into the cache.
pub fn import_bytes(bytes: &[u8]) -> Result<Unpacked> {
    let into = cache_dir().context("no HOME to unpack the run into")?;
    import(bytes, &into)
}

/// Unpacks the manifest and trace of an export read from `reader` into
/// `<into>/<manifest id>/`. A run already unpacked there is reused as it
/// is: its id is the hash of its inputs, and runs are deterministic, so
/// the same id means the same run.
pub fn import(reader: impl Read, into: &Path) -> Result<Unpacked> {
    fs::create_dir_all(into).with_context(|| format!("creating {}", into.display()))?;

    // Unpack into a staging directory first; the run's id is known only
    // once the manifest has been read, and it may not come first.
    let staging = into.join(format!(".staging-{}-{}", std::process::id(), unique()));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;
    let result = unpack(reader, &staging).and_then(|replayable| {
        let dir = place(&staging, into)?;
        Ok(Unpacked { dir, replayable })
    });
    let _ = fs::remove_dir_all(&staging);
    result
}

/// A number that differs between calls in one process, for staging names.
fn unique() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Writes the archive's manifest and trace into `staging`, and says
/// whether it holds what replay needs. Every other entry is skipped; a
/// path that is not plain names inside the archive is refused.
fn unpack(reader: impl Read, staging: &Path) -> Result<bool> {
    let decoder = zstd::Decoder::new(reader).context("not a zstd stream")?;
    let mut archive = tar::Archive::new(decoder);
    let mut replayable = false;
    for entry in archive.entries().context("not a tar archive")? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        if path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        {
            bail!("the archive holds an unsafe path {}", path.display());
        }
        if export::REPLAY_DIRS.iter().any(|d| path.starts_with(d)) {
            replayable = true;
        }
        // The run's bookmarks come too, as a regular file no larger than
        // the engine writes.
        let bookmarks = path == Path::new(BOOKMARKS_FILE)
            && entry.header().entry_type().is_file()
            && entry.size() <= BOOKMARKS_MAX_BYTES;
        let wanted = path == Path::new(MANIFEST) || path == Path::new(TRACE) || bookmarks;
        if !wanted {
            continue;
        }
        let mut out = File::create(staging.join(&path))?;
        io::copy(&mut entry, &mut out)?;
    }
    for name in [MANIFEST, TRACE] {
        if !staging.join(name).is_file() {
            bail!("the archive has no {name}");
        }
    }
    Ok(replayable)
}

/// Moves a staged run to `<into>/<id>`, or keeps the copy already there.
fn place(staging: &Path, into: &Path) -> Result<PathBuf> {
    let manifest: Value = serde_json::from_slice(&fs::read(staging.join(MANIFEST))?)
        .context("the archive's manifest.json is not JSON")?;
    let id = manifest
        .get("id")
        .and_then(Value::as_str)
        .and_then(RunId::parse)
        .map_or_else(|| NO_ID.to_string(), |id| id.to_string());
    let dir = into.join(&id);
    if is_complete(&dir) {
        return Ok(dir);
    }

    // The rename is atomic and fails onto a directory that has entries, so
    // when another import of the same run wins the race, its copy stays
    // and this one is dropped.
    if fs::rename(staging, &dir).is_ok() || is_complete(&dir) {
        return Ok(dir);
    }

    // What is in the way is a partial copy no import made, since renames
    // place whole runs; replace it.
    let _ = fs::remove_dir_all(&dir);
    fs::rename(staging, &dir).with_context(|| format!("moving the run to {}", dir.display()))?;
    Ok(dir)
}

/// Whether `dir` holds an unpacked run: its manifest and its trace.
fn is_complete(dir: &Path) -> bool {
    dir.join(MANIFEST).is_file() && dir.join(TRACE).is_file()
}

/// The file name a download of `url` is kept under: the URL's last path
/// segment, without a query or fragment, when it is a plain file name.
fn download_name(url: &str) -> &str {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let last = path.rsplit('/').next().unwrap_or_default();
    let plain = !last.starts_with('.')
        && last
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if plain && !last.is_empty() {
        last
    } else {
        DOWNLOAD_NAME
    }
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
    #[test]
    fn a_download_keeps_the_urls_file_name_when_it_is_plain() {
        // The last segment names the file, without a query; anything that
        // could climb out of the downloads directory or hide is replaced.
        let base = "https://github.com/o/r/releases/download/t";
        assert_eq!(
            download_name(&format!("{base}/run-replayable.rwd")),
            "run-replayable.rwd"
        );
        assert_eq!(download_name(&format!("{base}/run.rwd?x=1#y")), "run.rwd");
        assert_eq!(download_name(&format!("{base}/")), DOWNLOAD_NAME);
        assert_eq!(download_name(&format!("{base}/..")), DOWNLOAD_NAME);
        assert_eq!(download_name(&format!("{base}/a%2Fb.rwd")), DOWNLOAD_NAME);
    }

    #[test]
    fn urls_are_told_from_paths() {
        // http and https name a download; anything else is a path, even
        // one that looks like a host name.
        assert!(is_url(Path::new("https://example.com/run.rwd")));
        assert!(is_url(Path::new("http://example.com/run.rwd")));
        assert!(!is_url(Path::new("runs/https/run.rwd")));
        assert!(!is_url(Path::new("example.com/run.rwd")));
    }

    // Archives built in the test the way the engine builds them: a tar of
    // named entries compressed with zstd, unpacked into a temporary
    // directory.
    use super::*;

    /// zstd's default level, as the engine uses.
    const LEVEL: i32 = rewind_trace::export::COMPRESSION_LEVEL;

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
        // inputs are left in the archive, though their being there marks
        // the export replayable.
        let dir = temp_dir("unpack");
        let bytes = archive(&[
            ("trace.bin", b"trace"),
            ("keyframes/0000000000000100.kf", b"kf"),
            ("pages/00ff", &[0u8; 16]),
            ("inputs/kernel", b"kernel"),
            ("manifest.json", br#"{"id": "3e7358ddd62c45d3"}"#),
        ]);
        let unpacked = import(&bytes[..], &dir).unwrap();
        assert!(unpacked.replayable);
        let run = unpacked.dir;
        assert_eq!(run, dir.join("3e7358ddd62c45d3"));
        assert_eq!(fs::read(run.join("trace.bin")).unwrap(), b"trace");
        assert!(!run.join("pages").exists() && !run.join("inputs").exists());
        let left: Vec<_> = fs::read_dir(&dir).unwrap().collect();
        assert_eq!(left.len(), 1, "staging left behind");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_export_s_bookmarks_come_with_it_when_they_are_small() {
        // The run's bookmarks file is unpacked beside its trace; one larger
        // than the engine would write is left in the archive.
        let dir = temp_dir("bookmarks");
        let marks = br#"[{"step": 40, "note": "here"}]"#;
        let bytes = archive(&[
            ("manifest.json", br#"{"id": "000000000000000a"}"#),
            ("trace.bin", b"trace"),
            (crate::bookmarks::BOOKMARKS_FILE, marks),
        ]);
        let run = import(&bytes[..], &dir).unwrap().dir;
        let kept = fs::read(run.join(crate::bookmarks::BOOKMARKS_FILE)).unwrap();
        assert_eq!(kept, marks);

        let huge = vec![b' '; crate::bookmarks::MAX_BYTES as usize + 1];
        let bytes = archive(&[
            ("manifest.json", br#"{"id": "000000000000000b"}"#),
            ("trace.bin", b"trace"),
            (crate::bookmarks::BOOKMARKS_FILE, &huge),
        ]);
        let run = import(&bytes[..], &dir).unwrap().dir;
        assert!(!run.join(crate::bookmarks::BOOKMARKS_FILE).exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_run_already_unpacked_is_reused() {
        // A second import of the same id keeps the first copy.
        let dir = temp_dir("reuse");
        let first = archive(&[
            ("manifest.json", br#"{"id": "0000000000000abc"}"#),
            ("trace.bin", b"one"),
        ]);
        let second = archive(&[
            ("manifest.json", br#"{"id": "0000000000000abc"}"#),
            ("trace.bin", b"two"),
        ]);
        import(&first[..], &dir).unwrap();
        let unpacked = import(&second[..], &dir).unwrap();
        assert!(!unpacked.replayable, "a trace alone is not replayable");
        assert_eq!(fs::read(unpacked.dir.join("trace.bin")).unwrap(), b"one");
        fs::remove_dir_all(&dir).unwrap();
    }

    /// How many threads import the same run at once, and how many rounds.
    const RACERS: usize = 8;
    const ROUNDS: usize = 20;

    #[test]
    fn imports_of_one_run_at_once_all_succeed() {
        // Several threads import the same archive into an empty directory
        // at the same moment, as two windows opening the same file do;
        // every import returns a complete copy of the run.
        let bytes = archive(&[
            ("manifest.json", br#"{"id": "000000000000005a"}"#),
            ("trace.bin", b"trace"),
        ]);
        for round in 0..ROUNDS {
            let dir = temp_dir(&format!("race-{round}"));
            let barrier = std::sync::Barrier::new(RACERS);
            std::thread::scope(|s| {
                for _ in 0..RACERS {
                    s.spawn(|| {
                        barrier.wait();
                        let run = import(&bytes[..], &dir).unwrap().dir;
                        assert_eq!(fs::read(run.join("trace.bin")).unwrap(), b"trace");
                    });
                }
            });
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn broken_and_hostile_archives_are_refused() {
        // A missing trace, a path that climbs out, and bytes that are not
        // zstd at all; an id that is not a run id is replaced.
        let dir = temp_dir("refuse");
        let no_trace = archive(&[("manifest.json", br#"{"id": "000000000000000c"}"#)]);
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
        assert_eq!(import(&odd_id[..], &dir).unwrap().dir, dir.join(NO_ID));
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
