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
#[cfg(not(target_family = "wasm"))]
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::bookmarks::{BOOKMARKS_FILE, MAX_BYTES as BOOKMARKS_MAX_BYTES};
use rewind_trace::export;
use rewind_trace::manifest::{MANIFEST, RunId, TRACE};

/// The binary units sizes are said in.
const KIBIBYTE: u64 = 1 << 10;
const MEBIBYTE: u64 = 1 << 20;
const GIBIBYTE: u64 = 1 << 30;

/// A size in words, in the largest binary unit that divides it: 16 GiB,
/// 512 MiB, 100 bytes.
fn size_words(bytes: u64) -> String {
    let units = [(GIBIBYTE, "GiB"), (MEBIBYTE, "MiB"), (KIBIBYTE, "KiB")];
    for (unit, name) in units {
        if bytes >= unit && bytes.is_multiple_of(unit) {
            return format!("{} {name}", bytes / unit);
        }
    }
    format!("{bytes} bytes")
}

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
#[cfg(not(target_family = "wasm"))]
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
/// returns the file. A file of the same length already downloaded from
/// the same URL is taken as that download, so opening a URL twice
/// fetches it once.
#[cfg(not(target_family = "wasm"))]
pub fn download(url: &str) -> Result<PathBuf> {
    download_within(url, DOWNLOAD_LIMITS)
}

/// What a download is held to, so a server that stops answering or never
/// stops sending cannot hold the download's thread or fill the disk.
#[cfg(not(target_family = "wasm"))]
#[derive(Clone, Copy, Debug)]
struct DownloadLimits {
    /// How long finding the host and connecting to it may take.
    connect: Duration,
    /// How long the server may take to answer once the request is sent.
    response: Duration,
    /// How long the whole file may take to arrive.
    body: Duration,
    /// The most bytes a download may be.
    bytes: u64,
}

/// The limits downloads are held to: a replayable export of a long build
/// is a few gigabytes, and a slow connection moves one in under an hour.
#[cfg(not(target_family = "wasm"))]
const DOWNLOAD_LIMITS: DownloadLimits = DownloadLimits {
    connect: Duration::from_secs(20),
    response: Duration::from_secs(60),
    body: Duration::from_secs(60 * 60),
    bytes: 16 * GIBIBYTE,
};

/// Downloads `url` as [`download`] does, within `limits`.
#[cfg(not(target_family = "wasm"))]
fn download_within(url: &str, limits: DownloadLimits) -> Result<PathBuf> {
    // Each URL gets a directory of its own, so two URLs whose files share
    // a name never pass for each other.
    let dir = downloads_dir()
        .context("no HOME to download the run into")?
        .join(url_key(url));
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let name = download_name(url);
    let file = dir.join(name);

    // ureq waits without end unless told how long to wait.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_resolve(Some(limits.connect))
        .timeout_connect(Some(limits.connect))
        .timeout_recv_response(Some(limits.response))
        .timeout_recv_body(Some(limits.body))
        .build()
        .into();
    let response = agent
        .get(url)
        .call()
        .with_context(|| format!("downloading {url}"))?;
    let length = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let too_large = || {
        anyhow::anyhow!(
            "{url} is larger than {}, the largest export Rewind downloads",
            size_words(limits.bytes)
        )
    };
    if length.is_some_and(|length| length > limits.bytes) {
        return Err(too_large());
    }
    let have = fs::metadata(&file).ok().map(|m| m.len());
    if length.is_some() && have == length {
        return Ok(file);
    }

    // Written under another name and renamed whole, so a download cut
    // short never passes for the file. One byte past the limit is read, to
    // tell a file of the limit's size from a larger one.
    let partial = dir.join(format!(".{name}.{}-{}", std::process::id(), unique()));
    let mut body = response.into_body().into_reader().take(limits.bytes + 1);
    let written = File::create(&partial).and_then(|mut out| io::copy(&mut body, &mut out));
    let kept = match written {
        Ok(bytes) if bytes > limits.bytes => Err(too_large()),
        Ok(_) => fs::rename(&partial, &file).with_context(|| format!("downloading {url}")),
        Err(e) => Err(e).with_context(|| format!("downloading {url}")),
    };
    if kept.is_err() {
        let _ = fs::remove_file(&partial);
    }
    kept.map(|()| file)
}

/// The browser build has no disk to download to: the page fetches an
/// export and hands its bytes to [`read_bytes`].
#[cfg(target_family = "wasm")]
pub fn download(url: &str) -> Result<PathBuf> {
    bail!("{url}: the browser viewer opens exports the page fetched")
}

/// An export's manifest and trace, read into memory.
pub struct InMemory {
    pub manifest: Vec<u8>,
    pub trace: Vec<u8>,
    /// The bookmarks the desktop app made, when the export carries them.
    pub bookmarks: Option<Vec<u8>>,
    /// Whether it holds the keyframes, pages and inputs replay needs.
    pub replayable: bool,
}

/// The most bytes of an export's manifest and of its trace the app reads.
/// The archive is compressed, so a small file can claim members of any
/// size, and the app holds a run's whole trace in memory.
#[derive(Clone, Copy, Debug)]
struct MemberLimits {
    manifest: u64,
    trace: u64,
}

/// The limits exports are read within. A manifest is a few kilobytes of
/// JSON. A trace is read whole into memory: the desktop app has the
/// machine's, and the browser viewer has a 32-bit address space that
/// also holds the export's own bytes.
const MEMBER_LIMITS: MemberLimits = MemberLimits {
    manifest: 16 * MEBIBYTE,
    trace: if cfg!(target_family = "wasm") {
        512 * MEBIBYTE
    } else {
        8 * GIBIBYTE
    },
};

impl MemberLimits {
    /// The most bytes the member at `path` may be, for the manifest and
    /// the trace.
    fn of(&self, path: &Path) -> Option<u64> {
        if path == Path::new(MANIFEST) {
            return Some(self.manifest);
        }
        (path == Path::new(TRACE)).then_some(self.trace)
    }

    /// The member at `path`, `size` bytes long by its header, as far as
    /// the app reads it: all of it, or for a manifest or trace past its
    /// limit a refusal that names the limit.
    fn reader<'a, R: Read>(
        &self,
        entry: &'a mut R,
        path: &Path,
        size: u64,
    ) -> Result<io::Take<&'a mut R>> {
        let limit = self.of(path).unwrap_or(size);
        if size > limit {
            bail!(
                "the archive's {} is {}, and Rewind reads one of at most {}",
                path.display(),
                size_words(size),
                size_words(limit)
            );
        }
        Ok(entry.take(limit))
    }
}

/// Reads the manifest and trace of an export held in memory, without a
/// disk to unpack them to, as the browser build opens exports.
pub fn read_bytes(bytes: &[u8]) -> Result<InMemory> {
    read_bytes_within(bytes, MEMBER_LIMITS)
}

/// Reads an export as [`read_bytes`] does, within `limits`.
fn read_bytes_within(bytes: &[u8], limits: MemberLimits) -> Result<InMemory> {
    let mut archive = tar::Archive::new(decompress(bytes)?);
    let mut manifest = None;
    let mut trace = None;
    let mut bookmarks = None;
    let mut replayable = false;
    for entry in archive.entries().context("not a tar archive")? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();

        // The members only replay needs mark the export replayable, and
        // are not read.
        if export::REPLAY_DIRS.iter().any(|d| path.starts_with(d)) {
            replayable = true;
            continue;
        }

        // The manifest, the trace and the bookmarks, as a regular file no
        // larger than the engine writes, are kept; every other member is
        // skipped. A manifest or trace past its limit refuses the export
        // before any of the member is read into memory.
        let carried = path == Path::new(BOOKMARKS_FILE)
            && entry.header().entry_type().is_file()
            && entry.size() <= BOOKMARKS_MAX_BYTES;
        let slot = if path == Path::new(MANIFEST) {
            &mut manifest
        } else if path == Path::new(TRACE) {
            &mut trace
        } else if carried {
            &mut bookmarks
        } else {
            continue;
        };
        let size = entry.size();
        let mut bytes = Vec::new();
        limits
            .reader(&mut entry, &path, size)?
            .read_to_end(&mut bytes)?;
        *slot = Some(bytes);
    }
    let manifest = manifest.with_context(|| format!("the archive has no {MANIFEST}"))?;
    let trace = trace.with_context(|| format!("the archive has no {TRACE}"))?;
    Ok(InMemory {
        manifest,
        trace,
        bookmarks,
        replayable,
    })
}

/// The tar archive inside an export's zstd frame.
#[cfg(not(target_family = "wasm"))]
fn decompress<R: Read>(reader: R) -> Result<impl Read> {
    zstd::Decoder::new(reader).context("not a zstd stream")
}

/// The tar archive inside an export's zstd frame, decoded in pure Rust:
/// zstd's C library has no wasm32 build here.
#[cfg(target_family = "wasm")]
fn decompress<R: Read>(reader: R) -> Result<impl Read> {
    ruzstd::decoding::StreamingDecoder::new(reader)
        .map_err(|e| anyhow::anyhow!("not a zstd stream: {e}"))
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
    import_within(reader, into, MEMBER_LIMITS)
}

/// Unpacks an export as [`import`] does, within `limits`.
fn import_within(reader: impl Read, into: &Path, limits: MemberLimits) -> Result<Unpacked> {
    fs::create_dir_all(into).with_context(|| format!("creating {}", into.display()))?;

    // Unpack into a staging directory first; the run's id is known only
    // once the manifest has been read, and it may not come first.
    let staging = into.join(format!(".staging-{}-{}", std::process::id(), unique()));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;
    let result = unpack(reader, &staging, limits).and_then(|replayable| {
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
/// path that is not plain names inside the archive is refused, and so is
/// a manifest or trace larger than `limits` allow.
fn unpack(reader: impl Read, staging: &Path, limits: MemberLimits) -> Result<bool> {
    let mut archive = tar::Archive::new(decompress(reader)?);
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

        // A manifest or trace past its limit refuses the export before
        // any of the member is written to the cache.
        let size = entry.size();
        let mut member = limits.reader(&mut entry, &path, size)?;
        let mut out = File::create(staging.join(&path))?;
        io::copy(&mut member, &mut out)?;
    }
    for name in [MANIFEST, TRACE] {
        if !staging.join(name).is_file() {
            bail!("the archive has no {name}");
        }
    }
    Ok(replayable)
}

/// Gives `dir`, a copy of the run unpacked before, the bookmarks the
/// export staged in `staging` carried, when the copy has none: the copy
/// may be from an export made before the run was bookmarked. Bookmarks
/// the copy has are the user's latest and stay. The link fails where a
/// bookmarks file exists, so a file saved at the same moment is not
/// replaced.
fn carry_bookmarks(staging: &Path, dir: &Path) {
    let staged = staging.join(BOOKMARKS_FILE);
    if staged.is_file() {
        let _ = fs::hard_link(&staged, dir.join(BOOKMARKS_FILE));
    }
}

/// Moves a staged run to `<into>/<id>`, or keeps the copy already there,
/// with the staged bookmarks when that copy has none.
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
        carry_bookmarks(staging, &dir);
        return Ok(dir);
    }

    // The rename is atomic and fails onto a directory that has entries, so
    // when another import of the same run wins the race, its copy stays
    // and this one is dropped.
    if fs::rename(staging, &dir).is_ok() {
        return Ok(dir);
    }
    if is_complete(&dir) {
        carry_bookmarks(staging, &dir);
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

/// How many hex digits of a URL's SHA-256 name its downloads directory.
#[cfg(not(target_family = "wasm"))]
const URL_KEY_DIGITS: usize = 16;

/// The name of the directory downloads of `url` are kept in: the start
/// of the URL's SHA-256, in hex.
#[cfg(not(target_family = "wasm"))]
fn url_key(url: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(url.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    hex[..URL_KEY_DIGITS].to_string()
}

/// The file name a download of `url` is kept under: the URL's last path
/// segment, without a query or fragment, when it is a plain file name.
#[cfg(not(target_family = "wasm"))]
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
    fn downloads_of_one_file_name_from_two_urls_are_kept_apart() {
        // A local server answers /a/run.rwd and /b/run.rwd with different
        // bytes of the same length. Each download returns the bytes its
        // own URL served, though the second finds a file of the first's
        // name and length already downloaded.
        use std::io::{BufRead, BufReader, Write};
        const BODIES: [(&str, &[u8]); 2] = [("/a/run.rwd", b"AAAA"), ("/b/run.rwd", b"BBBB")];
        super::test_cache();
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = server.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in server.incoming() {
                let mut stream = stream.unwrap();
                let mut request = String::new();
                BufReader::new(&stream).read_line(&mut request).unwrap();
                let path = request.split(' ').nth(1).unwrap_or_default();
                let (_, body) = BODIES.iter().find(|(p, _)| *p == path).unwrap();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(body).unwrap();
            }
        });

        for (path, body) in BODIES {
            let file = download(&format!("http://127.0.0.1:{port}{path}")).unwrap();
            assert_eq!(fs::read(&file).unwrap(), body);
        }
    }

    /// Serves every connection to a local port with what `respond` writes
    /// after the request line is read, and returns the port.
    fn serve(respond: impl Fn(&mut std::net::TcpStream) + Send + Sync + 'static) -> u16 {
        use std::io::{BufRead, BufReader};
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = server.local_addr().unwrap().port();
        let respond = std::sync::Arc::new(respond);
        std::thread::spawn(move || {
            for stream in server.incoming() {
                let respond = respond.clone();
                std::thread::spawn(move || {
                    let mut stream = stream.unwrap();
                    let mut request = String::new();
                    BufReader::new(&stream).read_line(&mut request).unwrap();
                    respond(&mut stream);
                });
            }
        });
        port
    }

    /// Limits for the tests: short waits, and room for 8 bytes.
    const TEST_LIMITS: DownloadLimits = DownloadLimits {
        connect: Duration::from_secs(5),
        response: Duration::from_millis(500),
        body: Duration::from_millis(500),
        bytes: 8,
    };

    /// How long a server in these tests holds a connection it has
    /// stopped answering, and the most a download from one may take.
    const STALL: Duration = Duration::from_secs(20);
    const GIVES_UP_WITHIN: Duration = Duration::from_secs(10);

    /// The files a download left for `url`: none when it failed.
    fn downloaded(url: &str) -> Vec<PathBuf> {
        let dir = downloads_dir().unwrap().join(url_key(url));
        fs::read_dir(dir)
            .map(|entries| entries.filter_map(|e| Some(e.ok()?.path())).collect())
            .unwrap_or_default()
    }

    #[test]
    fn a_download_that_stalls_gives_up() {
        // One server answers with its headers and half the file, then
        // holds the connection; another reads the request and never
        // answers. Each download fails once its limit passes, long before
        // the server lets go, and leaves no file.
        use std::io::Write;
        super::test_cache();
        let half = serve(|stream| {
            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 8\r\n\r\nAAAA");
            let _ = stream.flush();
            std::thread::sleep(STALL);
        });
        let silent = serve(|_| std::thread::sleep(STALL));
        for port in [half, silent] {
            let url = format!("http://127.0.0.1:{port}/stall.rwd");
            let started = std::time::Instant::now();
            assert!(download_within(&url, TEST_LIMITS).is_err());
            assert!(
                started.elapsed() < GIVES_UP_WITHIN,
                "{:?}",
                started.elapsed()
            );
            assert_eq!(downloaded(&url), Vec::<PathBuf>::new());
        }
    }

    #[test]
    fn a_download_past_the_size_limit_is_refused() {
        // A file the server says is 16 bytes, and one of 16 bytes sent
        // with no length, against a limit of 8: each is refused in words
        // that name the limit, and leaves no file. A file of 8 bytes is
        // kept.
        use std::io::Write;
        super::test_cache();
        const BODY: &[u8; 16] = b"0123456789abcdef";
        let sized = serve(|stream| {
            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 16\r\n\r\n");
            let _ = stream.write_all(BODY);
        });
        let unsized_ = serve(|stream| {
            let _ = write!(stream, "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n");
            let _ = stream.write_all(BODY);
        });
        for port in [sized, unsized_] {
            let url = format!("http://127.0.0.1:{port}/big.rwd");
            let refused = download_within(&url, TEST_LIMITS).unwrap_err();
            assert!(format!("{refused:#}").contains("8 bytes"), "{refused:#}");
            assert_eq!(downloaded(&url), Vec::<PathBuf>::new());
        }
        let fits = serve(|stream| {
            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 8\r\n\r\n");
            let _ = stream.write_all(&BODY[..8]);
        });
        let url = format!("http://127.0.0.1:{fits}/fits.rwd");
        let file = download_within(&url, TEST_LIMITS).unwrap();
        assert_eq!(fs::read(file).unwrap(), &BODY[..8]);
    }

    #[test]
    fn sizes_are_said_in_the_largest_unit_that_divides_them() {
        // Limits are whole units and read as such; other sizes read in
        // bytes.
        assert_eq!(size_words(16 * GIBIBYTE), "16 GiB");
        assert_eq!(size_words(512 * MEBIBYTE), "512 MiB");
        assert_eq!(size_words(4 * KIBIBYTE), "4 KiB");
        assert_eq!(size_words(8), "8 bytes");
        assert_eq!(size_words(GIBIBYTE + 1), "1073741825 bytes");
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
    fn an_export_reads_into_memory_without_a_disk() {
        // As the browser viewer opens exports: the manifest, the trace and
        // small bookmarks come back as bytes, the replay data only marks
        // the export replayable, and an archive without a trace is refused.
        let marks = br#"{"version": 1, "marks": []}"#;
        let bytes = archive(&[
            ("trace.bin", b"trace"),
            ("keyframes/0000000000000100.kf", b"kf"),
            (crate::bookmarks::BOOKMARKS_FILE, marks),
            ("manifest.json", br#"{"id": "3e7358ddd62c45d3"}"#),
        ]);
        let read = read_bytes(&bytes).unwrap();
        assert_eq!(read.trace, b"trace");
        assert_eq!(read.manifest, br#"{"id": "3e7358ddd62c45d3"}"#);
        assert_eq!(read.bookmarks.as_deref(), Some(&marks[..]));
        assert!(read.replayable);

        let huge = vec![b' '; crate::bookmarks::MAX_BYTES as usize + 1];
        let bytes = archive(&[
            ("manifest.json", b"{}"),
            ("trace.bin", b"trace"),
            (crate::bookmarks::BOOKMARKS_FILE, &huge),
        ]);
        let read = read_bytes(&bytes).unwrap();
        assert_eq!(read.bookmarks, None);
        assert!(!read.replayable);

        let no_trace = archive(&[("manifest.json", b"{}")]);
        assert!(read_bytes(&no_trace).is_err());
        assert!(read_bytes(b"not zstd").is_err());
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

    /// Limits for the tests: a manifest of at most 32 bytes and a trace
    /// of at most 8.
    const SMALL: MemberLimits = MemberLimits {
        manifest: 32,
        trace: 8,
    };

    #[test]
    fn an_export_with_a_member_past_its_limit_is_refused() {
        // A trace of 9 bytes against a limit of 8, and a manifest of 33
        // against 32: read into memory or unpacked to disk, each is
        // refused in words that name the member and the limit, and
        // nothing is left in the cache. Members at their limits are read.
        let manifest = br#"{"id": "00000000000000c1"}"#;
        let long_manifest = br#"{"id": "00000000000000c2"}       "#;
        assert_eq!((manifest.len(), long_manifest.len()), (26, 33));
        let fits = archive(&[("manifest.json", manifest), ("trace.bin", b"12345678")]);
        let big_trace = archive(&[("manifest.json", manifest), ("trace.bin", b"123456789")]);
        let big_manifest = archive(&[("manifest.json", long_manifest), ("trace.bin", b"1")]);

        assert_eq!(read_bytes_within(&fits, SMALL).unwrap().trace, b"12345678");
        let dir = temp_dir("limits");
        let unpacked = import_within(&fits[..], &dir, SMALL).unwrap();
        fs::remove_dir_all(unpacked.dir).unwrap();

        for (bytes, member, limit) in [
            (&big_trace, "trace.bin", "8 bytes"),
            (&big_manifest, "manifest.json", "32 bytes"),
        ] {
            let read = read_bytes_within(bytes, SMALL).map(|_| ()).unwrap_err();
            let unpacked = import_within(&bytes[..], &dir, SMALL).unwrap_err();
            for refused in [read, unpacked] {
                let words = format!("{refused:#}");
                assert!(words.contains(member) && words.contains(limit), "{words}");
            }
            let left: Vec<_> = fs::read_dir(&dir).unwrap().collect();
            assert!(left.is_empty(), "{left:?}");
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn bookmarks_come_with_a_run_unpacked_before_without_any() {
        // A run is unpacked from an export without bookmarks, then opened
        // from an export of the same run that carries some: the copy
        // unpacked before is kept and takes the bookmarks. Bookmarks the
        // copy already has are not replaced by an export's.
        let dir = temp_dir("carry");
        let manifest = br#"{"id": "00000000000000d1"}"#;
        let marks = br#"{"version": 1, "marks": [{"step": 4, "note": "carried"}]}"#;
        let others = br#"{"version": 1, "marks": [{"step": 9, "note": "other"}]}"#;
        let plain = archive(&[("manifest.json", manifest), ("trace.bin", b"one")]);
        let with = |marks: &[u8]| {
            archive(&[
                ("manifest.json", manifest),
                ("trace.bin", b"two"),
                (BOOKMARKS_FILE, marks),
            ])
        };
        let run = import(&plain[..], &dir).unwrap().dir;
        assert!(!run.join(BOOKMARKS_FILE).exists());

        assert_eq!(import(&with(marks)[..], &dir).unwrap().dir, run);
        assert_eq!(fs::read(run.join("trace.bin")).unwrap(), b"one");
        assert_eq!(fs::read(run.join(BOOKMARKS_FILE)).unwrap(), marks);

        import(&with(others)[..], &dir).unwrap();
        assert_eq!(fs::read(run.join(BOOKMARKS_FILE)).unwrap(), marks);
        let left: Vec<_> = fs::read_dir(&dir).unwrap().collect();
        assert_eq!(left.len(), 1, "staging left behind");
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
