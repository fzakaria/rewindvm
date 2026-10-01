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

    tar.append_path_with_name(run.dir.join(MANIFEST), MANIFEST)?;
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

        // Every page any keyframe names, once.
        let store = Store::open(&home.store())?;
        let mut hashes: BTreeSet<Hash> = BTreeSet::new();
        for step in keyframes::steps(&run.dir) {
            let name = format!("{}/{step:016}.kf", keyframes::DIR);
            tar.append_path_with_name(run.dir.join(&name), &name)?;
            for (_, hash) in keyframes::load(&run.dir, step)?.pages {
                if hash != ZERO_PAGE {
                    hashes.insert(hash);
                }
            }
        }
        let mut page = vec![0u8; PAGE_SIZE];
        for hash in hashes {
            store.get(&hash, &mut page)?;
            let mut header = tar::Header::new_gnu();
            header.set_size(PAGE_SIZE as u64);
            header.set_mode(0o644);
            header.set_mtime(0);
            header.set_cksum();
            tar.append_data(
                &mut header,
                format!("{PAGES_DIR}/{}", hex(&hash)),
                &page[..],
            )?;
        }
    }

    tar.into_inner()?.finish()?.sync_all()?;
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
        // Pages go straight into the store; everything else is staged.
        if path.starts_with(PAGES_DIR) {
            let mut page = Vec::with_capacity(PAGE_SIZE);
            entry.read_to_end(&mut page)?;
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

    let manifest_path = staging.join(MANIFEST);
    let mut manifest: Manifest = serde_json::from_slice(
        &fs::read(&manifest_path).context("the export has no manifest.json")?,
    )?;
    let dir = home.runs().join(&manifest.id);

    // Inputs move into the home, where replays will look for them.
    if staging.join(INPUTS_DIR).exists() {
        let inputs = home.root().join("inputs").join(&manifest.id);
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

    fs::create_dir_all(&dir)?;
    fs::rename(staging.join(TRACE), dir.join(TRACE)).context("the export has no trace.bin")?;
    let kf_from = staging.join(keyframes::DIR);
    if kf_from.exists() {
        let kf_to = dir.join(keyframes::DIR);
        let _ = fs::remove_dir_all(&kf_to);
        fs::rename(kf_from, kf_to)?;
    }
    let mut f = File::create(dir.join(MANIFEST))?;
    f.write_all(&serde_json::to_vec_pretty(&manifest)?)?;
    Run::open(&dir)
}
