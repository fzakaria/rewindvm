//! Runs as single files.
//!
//! A `.rwd` file is a tar archive compressed with zstd. It always holds the
//! run's `manifest.json` and `trace.bin`, which is everything the scrubber
//! needs to show the run. A replayable export adds what another machine
//! needs to run it again: the keyframes, the pages they use, the input
//! image, and the guest kernel and initramfs the run booted.
//!
//! ```text
//! manifest.json
//! trace.bin
//! keyframes/<step>.kf          replayable only
//! pages/<hex hash>             replayable only: 4 KiB page contents
//! inputs/kernel                replayable only
//! inputs/initrd                replayable only
//! inputs/image.erofs           replayable only, when the run has an image
//! ```

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rewind_store::{Hash, Store, ZERO_PAGE, hex};

use crate::home::Home;
use crate::keyframes;
use crate::run::{MANIFEST, Manifest, Run, TRACE};

/// What an export carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Contents {
    /// The manifest and trace: enough to view and scrub the run.
    View,
    /// Also keyframes, pages and inputs: enough to replay the run on
    /// another machine with a compatible CPU.
    Replayable,
}

const PAGE_SIZE: usize = 4096;

/// zstd's default level: an export is written once and read rarely.
const COMPRESSION_LEVEL: i32 = 3;

const PAGES_DIR: &str = "pages";
const INPUTS_DIR: &str = "inputs";
const KERNEL: &str = "inputs/kernel";
const INITRD: &str = "inputs/initrd";
const IMAGE: &str = "inputs/image.erofs";

/// Writes `run` to `out` as a `.rwd` file.
pub fn export(home: &Home, run: &Run, contents: Contents, out: &Path) -> Result<()> {
    let file = File::create(out).with_context(|| format!("creating {}", out.display()))?;
    let encoder = zstd::Encoder::new(file, COMPRESSION_LEVEL)?;
    let mut tar = tar::Builder::new(encoder);
    tar.mode(tar::HeaderMode::Deterministic);

    // Keyframes a run shares with another travel in a replayable export as
    // its own, so the manifest names no other run's.
    let mut manifest = run.manifest.clone();
    manifest.shared_keyframes = None;
    append_bytes(&mut tar, MANIFEST, &serde_json::to_vec_pretty(&manifest)?)?;
    tar.append_path_with_name(run.dir.join(TRACE), TRACE)?;

    if contents == Contents::Replayable {
        let spec = &run.manifest.spec;
        tar.append_path_with_name(&spec.kernel, KERNEL)
            .with_context(|| format!("adding the kernel {}", spec.kernel.display()))?;
        tar.append_path_with_name(&spec.initrd, INITRD)
            .with_context(|| format!("adding the initramfs {}", spec.initrd.display()))?;
        if let Some(image) = &spec.image {
            tar.append_path_with_name(image, IMAGE)
                .with_context(|| format!("adding the image {}", image.display()))?;
        }

        // Every keyframe the run can seek from, wherever it is kept, and
        // every page any of them names, once.
        let store = Store::open(&home.store())?;
        let layers = run.keyframes()?;
        let mut hashes: BTreeSet<Hash> = BTreeSet::new();
        for step in layers.steps() {
            let name = format!("{}/{step:016}.kf", keyframes::DIR);
            let from = layers
                .path(step)
                .with_context(|| format!("keyframe {step} of run {}", run.manifest.id))?;
            tar.append_path_with_name(&from, &name)?;
            for (_, hash) in layers.load(step)?.pages {
                if hash != ZERO_PAGE {
                    hashes.insert(hash);
                }
            }
        }
        let mut page = vec![0u8; PAGE_SIZE];
        for hash in hashes {
            store.get(&hash, &mut page)?;
            append_bytes(&mut tar, &format!("{PAGES_DIR}/{}", hex(&hash)), &page)?;
        }
    }

    tar.into_inner()?.finish()?.sync_all()?;
    Ok(())
}

/// Adds a file with these contents, its metadata fixed so the same run
/// exports to the same bytes.
fn append_bytes<W: Write>(tar: &mut tar::Builder<W>, name: &str, bytes: &[u8]) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(0);
    header.set_cksum();
    tar.append_data(&mut header, name, bytes)?;
    Ok(())
}

/// What `rewind import` and the app take a URL by: http and https.
const URL_SCHEMES: &[&str] = &["https://", "http://"];

/// Whether `source` names a URL to download rather than a file.
pub fn is_url(source: &str) -> bool {
    URL_SCHEMES.iter().any(|scheme| source.starts_with(scheme))
}

/// Reads a `.rwd` file into the home and returns the run. A replayable
/// export's inputs land in the home too, and its manifest is rewritten to
/// point at them.
pub fn import(home: &Home, file: &Path) -> Result<Run> {
    let reader = File::open(file).with_context(|| format!("opening {}", file.display()))?;
    import_from(home, reader, &file.display().to_string())
}

/// Downloads a `.rwd` file from `url` into the home, unpacking it as it
/// arrives, and returns the run.
pub fn import_url(home: &Home, url: &str) -> Result<Run> {
    let response = ureq::get(url)
        .call()
        .with_context(|| format!("downloading {url}"))?;
    let reader = response.into_body().into_reader();
    import_from(home, reader, url)
}

/// Reads an export from `reader` into the home; `source` names it in
/// errors.
fn import_from(home: &Home, reader: impl Read, source: &str) -> Result<Run> {
    let staging = home
        .root()
        .join(format!("importing-{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;
    let result = unpack_and_place(home, reader, source, &staging);
    let _ = fs::remove_dir_all(&staging);
    result
}

fn unpack_and_place(home: &Home, reader: impl Read, source: &str, staging: &Path) -> Result<Run> {
    let decoder = zstd::Decoder::new(reader)?;
    let mut tar = tar::Archive::new(decoder);
    let mut store: Option<Store> = None;
    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        if path
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            bail!("{source} holds an unsafe path {}", path.display());
        }

        // An export holds only files and directories. A link could point
        // anywhere, and an entry under it would be written there.
        let kind = entry.header().entry_type();
        if !matches!(kind, tar::EntryType::Regular | tar::EntryType::Directory) {
            bail!(
                "{source} holds {} as a {kind:?}, which no export does",
                path.display()
            );
        }

        // Pages go straight into the store; everything else is staged. A
        // page is PAGE_SIZE bytes, so nothing longer is read into memory.
        if path.starts_with(PAGES_DIR) {
            if entry.header().size()? != PAGE_SIZE as u64 {
                bail!(
                    "page {} in {source} is not {PAGE_SIZE} bytes",
                    path.display()
                );
            }
            let mut page = vec![0u8; PAGE_SIZE];
            entry.read_exact(&mut page)?;
            let store = match &mut store {
                Some(s) => s,
                None => store.insert(Store::open(&home.store())?),
            };
            let hash = store.put(&page)?;
            let named = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            if hex(&hash) != named {
                bail!("page {named} in {source} does not match its contents");
            }
            continue;
        }
        let target = staging.join(&path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        entry.unpack(&target)?;
    }
    if let Some(store) = &store {
        store.sync()?;
    }

    // The run the export names, by an id that stays inside the runs
    // directory, and the hash of the trace it actually carries.
    let manifest_path = staging.join(MANIFEST);
    let mut manifest: Manifest = serde_json::from_slice(
        &fs::read(&manifest_path).context("the export has no manifest.json")?,
    )?;
    if !crate::run::is_run_id(&manifest.id) {
        bail!(
            "{source} names its run {:?}, which is not a run id",
            manifest.id
        );
    }
    let staged_trace = staging.join(TRACE);
    if !staged_trace.exists() {
        bail!("the export has no trace.bin");
    }
    let trace_hash = crate::image::hash_file(&staged_trace)?;

    // A copy of the run already here: none may be executing it, and a
    // finished one keeps its recording unless the export's trace is the
    // same.
    let dir = home.runs().join(&manifest.id);
    fs::create_dir_all(&dir)?;
    let _executing = crate::run::lock_executing(&dir)?;
    let recorded = Run::open(&dir)
        .ok()
        .filter(|r| r.manifest.outcome.is_some());
    if let Some(recorded) = &recorded
        && recorded.manifest.trace_hash.as_deref() != Some(trace_hash.as_str())
    {
        bail!(
            "run {} is here already with another trace, which stays; remove it to import \
             this one",
            manifest.id
        );
    }

    // Inputs move into the home, where replays will look for them.
    if staging.join(INPUTS_DIR).exists() {
        let inputs = home.inputs().join(&manifest.id);
        fs::create_dir_all(&inputs)?;
        let place = |name: &str| -> Result<Option<PathBuf>> {
            let from = staging.join(name);
            if !from.exists() {
                return Ok(None);
            }
            let to = inputs.join(Path::new(name).file_name().unwrap());
            fs::rename(&from, &to)?;
            Ok(Some(to))
        };
        if let Some(kernel) = place(KERNEL)? {
            manifest.spec.kernel = kernel;
        }
        if let Some(initrd) = place(INITRD)? {
            manifest.spec.initrd = initrd;
        }
        if let Some(image) = place(IMAGE)? {
            manifest.spec.image = Some(image);
        }
    }

    // A finished copy of the run already here keeps its keyframes, which
    // other runs here may read; the export's would only be the same states
    // again.
    let keeps_keyframes = recorded
        .as_ref()
        .filter(|_| keyframes::own_state(&dir) == keyframes::Own::Readable);
    if recorded.is_none() {
        fs::rename(&staged_trace, dir.join(TRACE))?;
    }
    let kf_from = staging.join(keyframes::DIR);
    match keeps_keyframes {
        Some(local) => manifest.shared_keyframes = local.manifest.shared_keyframes.clone(),
        None if kf_from.exists() => {
            let kf_to = dir.join(keyframes::DIR);
            let _ = fs::remove_dir_all(&kf_to);
            fs::rename(kf_from, kf_to)?;
        }
        None => {}
    }

    // The manifest goes last, whole or not at all.
    manifest.trace_hash = Some(trace_hash);
    crate::image::write_atomic(&dir.join(MANIFEST), &serde_json::to_vec_pretty(&manifest)?)?;
    Run::open(&dir)
}

#[cfg(test)]
mod tests {
    // Imports of exports built in memory into a home in a temporary
    // directory: what an import refuses, and what it keeps of a copy of
    // the run already there. No VM runs.
    use super::*;
    use crate::run::RunOutcome;
    use crate::run::tests::manifest;

    /// The id the tests' runs go by.
    const ID: &str = "0123456789abcdef";

    /// One entry of a test export.
    enum Entry<'a> {
        File(&'a str, &'a [u8]),
        Symlink(&'a str, &'a Path),
    }

    /// An export holding `entries`, compressed as `export` compresses.
    fn archive(entries: &[Entry]) -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        for entry in entries {
            match entry {
                Entry::File(name, bytes) => append_bytes(&mut tar, name, bytes).unwrap(),
                Entry::Symlink(name, target) => {
                    let mut header = tar::Header::new_gnu();
                    header.set_entry_type(tar::EntryType::Symlink);
                    header.set_size(0);
                    tar.append_link(&mut header, name, target).unwrap();
                }
            }
        }
        zstd::encode_all(&tar.into_inner().unwrap()[..], COMPRESSION_LEVEL).unwrap()
    }

    /// A manifest for run `id` that says it finished, with a trace hash
    /// that matches no trace.
    fn finished(id: &str) -> Vec<u8> {
        let mut m = manifest(id, "test", 0);
        m.outcome = Some(RunOutcome {
            stop: "poweroff".into(),
            step: 1,
            virtual_ns: 0,
            status: Some(0),
            wall_ms: 0,
        });
        m.trace_hash = Some("0".repeat(64));
        serde_json::to_vec_pretty(&m).unwrap()
    }

    /// A home in a fresh temporary directory.
    fn home(name: &str) -> Home {
        let dir = std::env::temp_dir().join(format!("rewind-import-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Home::at(dir).unwrap()
    }

    fn import_bytes(home: &Home, bytes: &[u8]) -> Result<Run> {
        import_from(home, bytes, "the test export")
    }

    fn blake3_hex(bytes: &[u8]) -> String {
        blake3::hash(bytes).to_hex().to_string()
    }

    #[test]
    fn an_import_hashes_the_trace_it_brings() {
        // An export whose manifest names a trace hash its trace does not
        // have imports with the hash of the trace it carries, which prune
        // compares runs by.
        let home = home("hash");
        let manifest = finished(ID);
        let bytes = archive(&[
            Entry::File(MANIFEST, &manifest),
            Entry::File(TRACE, b"trace"),
        ]);
        let run = import_bytes(&home, &bytes).unwrap();
        assert_eq!(run.manifest.trace_hash, Some(blake3_hex(b"trace")));
        fs::remove_dir_all(home.root()).unwrap();
    }

    #[test]
    fn an_import_refuses_a_run_id_that_is_not_one() {
        // A manifest whose id is not sixteen lowercase hex digits, such as
        // one that climbs out of the runs directory, is refused before
        // anything is placed, and the home is left with no run and nothing
        // beside its own directories.
        let home = home("bad-id");
        for id in ["../escape", "0123", "0123456789ABCDEF", "0123456789abcdeg"] {
            let manifest = finished(id);
            let bytes = archive(&[
                Entry::File(MANIFEST, &manifest),
                Entry::File(TRACE, b"trace"),
            ]);
            assert!(import_bytes(&home, &bytes).is_err(), "{id}");
        }
        assert!(!home.root().join("escape").exists());
        assert_eq!(fs::read_dir(home.runs()).unwrap().count(), 0);
        fs::remove_dir_all(home.root()).unwrap();
    }

    #[test]
    fn an_import_refuses_links() {
        // A symbolic link in an export could point anywhere, and an entry
        // under it would be written there: the import is refused and the
        // directory it points at stays empty.
        let home = home("link");
        let outside = home.root().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let manifest = finished(ID);
        let bytes = archive(&[
            Entry::File(MANIFEST, &manifest),
            Entry::File(TRACE, b"trace"),
            Entry::Symlink(keyframes::DIR, &outside),
            Entry::File("keyframes/0000000000000001.kf", b"keyframe"),
        ]);
        assert!(import_bytes(&home, &bytes).is_err());
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
        fs::remove_dir_all(home.root()).unwrap();
    }

    #[test]
    fn an_import_refuses_a_page_of_another_size() {
        // A page is 4 KiB; an entry under pages/ of any other size is
        // refused, so a huge one is never read into memory.
        let home = home("page-size");
        let manifest = finished(ID);
        let page = vec![1u8; PAGE_SIZE + 1];
        let name = format!("{PAGES_DIR}/{}", blake3_hex(&page));
        let bytes = archive(&[
            Entry::File(MANIFEST, &manifest),
            Entry::File(TRACE, b"trace"),
            Entry::File(&name, &page),
        ]);
        assert!(import_bytes(&home, &bytes).is_err());
        fs::remove_dir_all(home.root()).unwrap();
    }

    #[test]
    fn an_import_keeps_a_local_recording_it_differs_from() {
        // A finished copy of the run already here, with its own trace: an
        // export of the same run with another trace is refused and the
        // copy here stays byte for byte; one with the same trace imports.
        let home = home("local");
        let dir = home.runs().join(ID);
        fs::create_dir_all(&dir).unwrap();
        let mut local = manifest(ID, "local", 0);
        local.outcome = serde_json::from_slice::<Manifest>(&finished(ID))
            .unwrap()
            .outcome;
        local.trace_hash = Some(blake3_hex(b"local"));
        let local_bytes = serde_json::to_vec_pretty(&local).unwrap();
        fs::write(dir.join(MANIFEST), &local_bytes).unwrap();
        fs::write(dir.join(TRACE), b"local").unwrap();

        let manifest = finished(ID);
        let other = archive(&[
            Entry::File(MANIFEST, &manifest),
            Entry::File(TRACE, b"other"),
        ]);
        assert!(import_bytes(&home, &other).is_err());
        assert_eq!(fs::read(dir.join(TRACE)).unwrap(), b"local");
        assert_eq!(fs::read(dir.join(MANIFEST)).unwrap(), local_bytes);

        let same = archive(&[
            Entry::File(MANIFEST, &manifest),
            Entry::File(TRACE, b"local"),
        ]);
        import_bytes(&home, &same).unwrap();
        assert_eq!(fs::read(dir.join(TRACE)).unwrap(), b"local");
        fs::remove_dir_all(home.root()).unwrap();
    }

    #[test]
    fn an_import_waits_for_no_execution_of_the_run() {
        // While a process executes the run here, an import of it is
        // refused rather than renaming a trace under the execution.
        let home = home("executing");
        let dir = home.runs().join(ID);
        fs::create_dir_all(&dir).unwrap();
        let _executing = crate::run::lock_executing(&dir).unwrap();
        let manifest = finished(ID);
        let bytes = archive(&[
            Entry::File(MANIFEST, &manifest),
            Entry::File(TRACE, b"trace"),
        ]);
        assert!(import_bytes(&home, &bytes).is_err());
        assert!(!dir.join(TRACE).exists());
        fs::remove_dir_all(home.root()).unwrap();
    }
}
