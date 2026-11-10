//! Runs as single files.
//!
//! A `.rwd` file is a tar archive compressed with zstd. It always holds the
//! run's `manifest.json` and `trace.bin`, which is everything the scrubber
//! needs to show the run, and the desktop app's bookmarks of the run when
//! it has any. A replayable export adds what another machine
//! needs to run it again: the keyframes, the pages they use, the input
//! image, and the guest kernel and initramfs the run booted.
//!
//! ```text
//! manifest.json
//! trace.bin
//! bookmarks.json               when the run has bookmarks
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
use crate::run::{BOOKMARKS, MANIFEST, Manifest, Run, SpecExt, TRACE};
pub use rewind_trace::export::Contents;
use rewind_trace::export::{
    COMPRESSION_LEVEL, IMAGE, INITRD, INPUTS_DIR, KERNEL, MAX_BOOKMARKS, Member, PAGES_DIR,
};

const PAGE_SIZE: usize = 4096;

/// Writes `run` to `out` as a `.rwd` file. The export is written under a
/// temporary name beside `out` and renamed into place once it is whole,
/// so an export that fails leaves a file already at `out` as it was.
pub fn export(home: &Home, run: &Run, contents: Contents, out: &Path) -> Result<()> {
    let create =
        |path: &Path| File::create(path).with_context(|| format!("creating {}", out.display()));

    // A destination that is not a regular file, such as a pipe, has no
    // earlier export to lose and cannot be renamed over, so the export is
    // written straight into it.
    if fs::metadata(out).is_ok_and(|meta| !meta.is_file()) {
        return write_export(home, run, contents, create(out)?);
    }

    // Any other destination is written beside itself and renamed into
    // place. A destination that is a symbolic link stays one: the file
    // the link names is the one replaced.
    let dest = fs::canonicalize(out).unwrap_or_else(|_| out.to_path_buf());
    let tmp = crate::image::temp_beside(&dest);
    let written = create(&tmp)
        .and_then(|file| write_export(home, run, contents, file))
        .and_then(|()| crate::image::place(&tmp, &dest));
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

/// Writes `run` into `file` as a `.rwd` file.
fn write_export(home: &Home, run: &Run, contents: Contents, file: File) -> Result<()> {
    let encoder = zstd::Encoder::new(file, COMPRESSION_LEVEL)?;
    let mut tar = tar::Builder::new(encoder);
    tar.mode(tar::HeaderMode::Deterministic);

    // Keyframes a run shares with another travel in a replayable export as
    // its own, so the manifest names no other run's.
    let mut manifest = run.manifest.clone();
    manifest.shared_keyframes = None;
    append_bytes(&mut tar, MANIFEST, &serde_json::to_vec_pretty(&manifest)?)?;
    tar.append_path_with_name(run.dir.join(TRACE), TRACE)?;

    // The app's bookmarks, within the size an import takes.
    let bookmarks = run.dir.join(BOOKMARKS);
    if let Ok(meta) = fs::metadata(&bookmarks) {
        if meta.len() > MAX_BOOKMARKS {
            bail!(
                "{} is over {MAX_BOOKMARKS} bytes, more than an import takes",
                bookmarks.display()
            );
        }
        tar.append_path_with_name(&bookmarks, BOOKMARKS)?;
    }

    if contents == Contents::Replayable {
        let spec = &run.manifest.spec;
        tar.append_path_with_name(&spec.kernel, KERNEL)
            .with_context(|| format!("adding the kernel {}", spec.kernel.display()))?;
        tar.append_path_with_name(&spec.initrd, INITRD)
            .with_context(|| format!("adding the initrd {}", spec.initrd.display()))?;
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
        // Whether the run took them as it executed goes with them.
        if keyframes::taken(&run.dir) {
            append_bytes(
                &mut tar,
                &format!("{}/{}", keyframes::DIR, keyframes::TAKEN),
                &[],
            )?;
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
/// point at them. A finished copy of the run already in the home keeps
/// its manifest and its inputs, and takes only the keyframes and
/// bookmarks it lacks.
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

/// What an import's staging directory in the home's root is named, before
/// the id of the importing process.
const STAGING_PREFIX: &str = "importing-";

/// The id of the process whose import staged into `dir`, when `dir` is
/// named as a staging directory is. None for any other name.
pub(crate) fn staging_writer(dir: &Path) -> Option<u32> {
    let name = dir.file_name()?.to_str()?;
    name.strip_prefix(STAGING_PREFIX)?.parse().ok()
}

/// Reads an export from `reader` into the home; `source` names it in
/// errors. An import that is killed leaves its staging directory, which
/// `rewind gc` removes.
fn import_from(home: &Home, reader: impl Read, source: &str) -> Result<Run> {
    let staging = home
        .root()
        .join(format!("{STAGING_PREFIX}{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;
    let result = unpack_and_place(home, reader, source, &staging);
    let _ = fs::remove_dir_all(&staging);
    result
}

fn unpack_and_place(home: &Home, reader: impl Read, source: &str, staging: &Path) -> Result<Run> {
    let decoder = zstd::Decoder::new(reader)?;
    let mut store: Option<Store> = None;

    // Pages go straight into the store; everything else is staged. The
    // walk refuses a path that is not plain names and any member that is
    // not a file or a directory.
    rewind_trace::export::walk(decoder, |path, member, size, reader| {
        // Bookmarks are notes, never larger than MAX_BOOKMARKS.
        if member == Member::Bookmarks && size > MAX_BOOKMARKS {
            bail!("{source} holds bookmarks over {MAX_BOOKMARKS} bytes");
        }

        // A page is PAGE_SIZE bytes, so nothing longer is read into memory.
        if member == Member::Page {
            if size != PAGE_SIZE as u64 {
                bail!(
                    "page {} in {source} is not {PAGE_SIZE} bytes",
                    path.display()
                );
            }
            let mut page = vec![0u8; PAGE_SIZE];
            reader.read_exact(&mut page)?;
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
            return Ok(());
        }
        let target = staging.join(path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = File::create(&target)?;
        std::io::copy(reader, &mut out)?;
        Ok(())
    })
    .with_context(|| format!("reading {source}"))?;
    if let Some(store) = &store {
        store.sync()?;
    }

    // The run the export names, and the hash of the trace it actually
    // carries. Every run a manifest names, its own, its parent and the
    // one it shares keyframes with, is a run id, which stays inside the
    // runs directory, or the manifest does not parse.
    let manifest_path = staging.join(MANIFEST);
    let mut manifest =
        Manifest::parse(&fs::read(&manifest_path).context("the export has no manifest.json")?)
            .with_context(|| format!("reading the manifest {source} carries"))?;
    let staged_trace = staging.join(TRACE);
    if !staged_trace.exists() {
        bail!("the export has no trace.bin");
    }
    let trace_hash = crate::image::hash_file(&staged_trace)?;

    // The inputs a replayable export carries must be the ones that make
    // the run its manifest names, before anything is placed.
    if staging.join(INPUTS_DIR).exists() {
        verify_inputs(&manifest, staging, source)?;
    }

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

    // A finished copy of the run already here, with the same trace, keeps
    // its manifest, which names its own inputs and the run it shares
    // keyframes with, and keeps those inputs. The export gives the copy
    // only what the copy lacks: keyframes, bookmarks, and inputs when the
    // copy's own are not on this machine, as after a view export's
    // import. The manifest is then written again with the new inputs'
    // paths and nothing else changed.
    if let Some(mut local) = recorded {
        take_keyframes(staging, &dir)?;
        take_bookmarks(staging, &dir)?;
        if staging.join(INPUTS_DIR).exists() && !has_inputs(&local.manifest) {
            place_inputs(home, staging, &mut local.manifest)?;
            write_manifest(home, &dir, &local.manifest)?;
        }
        return Run::open(&dir);
    }

    // A run that is new here, or that never finished here: its inputs,
    // its trace, its keyframes and its bookmarks.
    if staging.join(INPUTS_DIR).exists() {
        place_inputs(home, staging, &mut manifest)?;
    }
    fs::rename(&staged_trace, dir.join(TRACE))?;
    take_keyframes(staging, &dir)?;
    take_bookmarks(staging, &dir)?;

    // The manifest goes last, whole or not at all.
    manifest.trace_hash = Some(trace_hash);
    write_manifest(home, &dir, &manifest)?;
    Run::open(&dir)
}

/// Writes `manifest` into the run directory `dir` of `home`, whole or not
/// at all, and brings the home's index of runs up to date with it.
fn write_manifest(home: &Home, dir: &Path, manifest: &Manifest) -> Result<()> {
    crate::image::write_atomic(&dir.join(MANIFEST), &serde_json::to_vec_pretty(manifest)?)?;
    crate::run_index::note(home, dir)
}

/// Whether the inputs `manifest` names are on this machine: a kernel and
/// an initramfs that make the manifest's run id, and the image when the
/// run booted one.
fn has_inputs(manifest: &Manifest) -> bool {
    let spec = &manifest.spec;
    let image_here = spec.image.as_ref().is_none_or(|image| image.is_file());
    spec.kernel.is_file() && spec.initrd.is_file() && image_here && spec.id() == manifest.id
}

/// Moves the kernel, initramfs and image staged in `staging` into the
/// home's inputs, where replays will look for them, and makes `manifest`
/// name them there.
fn place_inputs(home: &Home, staging: &Path, manifest: &mut Manifest) -> Result<()> {
    let inputs = home.inputs().join(&manifest.id);
    fs::create_dir_all(&inputs)?;
    let place = |name: &str| -> Result<Option<PathBuf>> {
        let from = staging.join(name);
        if !from.exists() {
            return Ok(None);
        }
        let to = inputs.join(Path::new(name).file_name().expect("a file's path"));
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
    Ok(())
}

/// Checks that the kernel, initramfs and image staged in `staging` are
/// the inputs of the run `manifest` names: with them the manifest's spec
/// hashes to the manifest's run id, and the image has the hash the spec
/// names. An export whose inputs were changed after it was written, or
/// that lacks one, is refused, so no replay here boots other inputs under
/// the run's id. `source` names the export in errors.
fn verify_inputs(manifest: &Manifest, staging: &Path, source: &str) -> Result<()> {
    let id = &manifest.id;

    // A run's id hashes its kernel and initramfs by their contents, and
    // every other field of its spec as the manifest has it.
    let mut spec = manifest.spec.clone();
    spec.kernel = staging.join(KERNEL);
    spec.initrd = staging.join(INITRD);
    for input in [&spec.kernel, &spec.initrd] {
        if !input.is_file() {
            bail!("{source} carries inputs for run {id} without its kernel or its initrd");
        }
    }
    if spec.id() != *id {
        bail!(
            "the kernel and initrd {source} carries do not make run {id}, which its \
             manifest names; the export was changed after it was written"
        );
    }

    // The image counts by the hash the spec names.
    let image = staging.join(IMAGE);
    match (&manifest.spec.image_hash, image.is_file()) {
        (None, false) => Ok(()),
        (Some(_), false) => bail!("{source} carries inputs for run {id} without its image"),
        (None, true) => bail!("{source} carries an image, and run {id} booted none"),
        (Some(hash), true) => {
            if crate::image::hash_file(&image)? != *hash {
                bail!("the image {source} carries is not the one run {id} booted");
            }
            Ok(())
        }
    }
}

/// Moves the keyframes staged in `staging` into the run directory `dir`,
/// in place of any there, unless the run has keyframes of its own that
/// read back: other runs here may read those, and the export's would only
/// be the same states again.
fn take_keyframes(staging: &Path, dir: &Path) -> Result<()> {
    let from = staging.join(keyframes::DIR);
    if !from.exists() || keyframes::own_state(dir) == keyframes::Own::Readable {
        return Ok(());
    }
    let to = dir.join(keyframes::DIR);
    let _ = fs::remove_dir_all(&to);
    fs::rename(from, to)?;
    Ok(())
}

/// Moves the bookmarks staged in `staging` into the run directory `dir`,
/// unless the run has bookmarks already, which stay.
fn take_bookmarks(staging: &Path, dir: &Path) -> Result<()> {
    let staged = staging.join(BOOKMARKS);
    let local = dir.join(BOOKMARKS);
    if staged.is_file() && !local.exists() {
        fs::rename(&staged, &local)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    // Imports of exports built in memory into a home in a temporary
    // directory: what an import refuses, and what it keeps of a copy of
    // the run already there. No VM runs.
    use super::*;
    use crate::run::tests::manifest;
    use crate::run::{BOOKMARKS, RunOutcome};

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
            stop: rewind_trace::stop::Stop::PoweredOff,
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
    fn a_run_exported_and_imported_elsewhere_arrives_whole() {
        // A finished run with bookmarks, its inputs on disk, and a keyframe
        // at step 5 naming one page in the store, taken as it executed. Its
        // replayable export imported into another home brings the same
        // manifest, trace and bookmarks, the keyframe, still taken as the
        // run executed, the page, and the inputs, which the manifest then
        // names in the new home. Its view export brings the first three
        // alone.
        let (from, to, view) = (home("from"), home("to"), home("view"));
        let inputs = from.root().join("given");
        fs::create_dir_all(&inputs).unwrap();
        fs::write(inputs.join("kernel"), b"kernel").unwrap();
        fs::write(inputs.join("initrd"), b"initrd").unwrap();

        let mut m: Manifest = serde_json::from_slice(&finished(ID)).unwrap();
        m.spec.kernel = inputs.join("kernel");
        m.spec.initrd = inputs.join("initrd");
        m.id = m.spec.id();
        m.trace_hash = Some(blake3_hex(b"trace"));
        let dir = from.runs().join(&m.id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(MANIFEST), serde_json::to_vec_pretty(&m).unwrap()).unwrap();
        fs::write(dir.join(TRACE), b"trace").unwrap();
        fs::write(dir.join(BOOKMARKS), br#"[{"step":3,"note":"here"}]"#).unwrap();

        let page = vec![7u8; PAGE_SIZE];
        let hash = {
            let mut store = Store::open(&from.store()).unwrap();
            let hash = store.put(&page).unwrap();
            store.sync().unwrap();
            hash
        };
        let mut kf = rewind_vmm::snapshot::Keyframe::default();
        kf.step = 5;
        kf.pages = vec![(0, hash)];
        keyframes::save(&dir, &kf).unwrap();
        keyframes::mark_taken(&dir).unwrap();
        let run = Run::open(&dir).unwrap();

        // The replayable export, imported.
        let file = from.root().join("run.rwd");
        export(&from, &run, Contents::Replayable, &file).unwrap();
        let got = import(&to, &file).unwrap();
        assert_eq!(got.manifest.id, m.id);
        assert_eq!(got.manifest.outcome, m.outcome);
        assert_eq!(fs::read(got.dir.join(TRACE)).unwrap(), b"trace");
        assert_eq!(
            fs::read(got.dir.join(BOOKMARKS)).unwrap(),
            fs::read(dir.join(BOOKMARKS)).unwrap()
        );
        assert_eq!(got.keyframes().unwrap().steps(), vec![5]);
        assert!(keyframes::taken(&got.dir));
        let mut read = vec![0u8; PAGE_SIZE];
        Store::open(&to.store())
            .unwrap()
            .get(&hash, &mut read)
            .unwrap();
        assert_eq!(read, page);
        assert!(got.manifest.spec.kernel.starts_with(to.inputs()));
        assert_eq!(fs::read(&got.manifest.spec.kernel).unwrap(), b"kernel");
        assert_eq!(fs::read(&got.manifest.spec.initrd).unwrap(), b"initrd");

        // The view export, imported: the run to read, nothing to replay.
        let file = from.root().join("view.rwd");
        export(&from, &run, Contents::View, &file).unwrap();
        let got = import(&view, &file).unwrap();
        assert_eq!(fs::read(got.dir.join(TRACE)).unwrap(), b"trace");
        assert!(got.dir.join(BOOKMARKS).is_file());
        assert!(got.keyframes().unwrap().steps().is_empty());
        assert_eq!(got.manifest.spec.kernel, m.spec.kernel);
        for home in [from, to, view] {
            fs::remove_dir_all(home.root()).unwrap();
        }
    }

    #[test]
    fn a_failed_export_leaves_the_file_it_was_writing_over() {
        // A run whose kernel is gone cannot be exported to replay. The
        // export fails, the file already at the destination keeps its
        // bytes, and no temporary file is left beside it. The same run
        // exported to view then replaces the file, also at a destination
        // named without a directory.
        let home = home("export-fails");
        let dir = home.runs().join(ID);
        fs::create_dir_all(&dir).unwrap();
        let mut m: Manifest = serde_json::from_slice(&finished(ID)).unwrap();
        m.spec.kernel = home.root().join("no-such-kernel");
        fs::write(dir.join(MANIFEST), serde_json::to_vec_pretty(&m).unwrap()).unwrap();
        fs::write(dir.join(TRACE), b"trace").unwrap();
        let run = Run::open(&dir).unwrap();

        let out_dir = home.root().join("exports");
        fs::create_dir_all(&out_dir).unwrap();
        let out = out_dir.join("run.rwd");
        fs::write(&out, b"an earlier export").unwrap();
        assert!(export(&home, &run, Contents::Replayable, &out).is_err());
        assert_eq!(fs::read(&out).unwrap(), b"an earlier export");
        assert_eq!(fs::read_dir(&out_dir).unwrap().count(), 1);

        export(&home, &run, Contents::View, &out).unwrap();
        assert_ne!(fs::read(&out).unwrap(), b"an earlier export");
        assert_eq!(fs::read_dir(&out_dir).unwrap().count(), 1);

        let bare = PathBuf::from(format!("rewind-export-test-{}.rwd", std::process::id()));
        export(&home, &run, Contents::View, &bare).unwrap();
        assert_eq!(fs::read(&bare).unwrap(), fs::read(&out).unwrap());
        fs::remove_file(&bare).unwrap();
        fs::remove_dir_all(home.root()).unwrap();
    }

    /// A finished run in a home, as an import of a replayable export
    /// leaves one: its kernel and initramfs under the home's inputs, its
    /// id the one those inputs make, and a trace of the bytes `trace`.
    fn replayable_run(home: &Home, name: &str) -> Run {
        let mut m: Manifest = serde_json::from_slice(&finished(ID)).unwrap();
        m.name = name.into();
        let given = home.root().join("given");
        fs::create_dir_all(&given).unwrap();
        fs::write(given.join("kernel"), b"kernel").unwrap();
        fs::write(given.join("initrd"), b"initrd").unwrap();
        m.spec.kernel = given.join("kernel");
        m.spec.initrd = given.join("initrd");
        m.id = m.spec.id();
        m.trace_hash = Some(blake3_hex(b"trace"));
        let dir = home.runs().join(&m.id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(MANIFEST), serde_json::to_vec_pretty(&m).unwrap()).unwrap();
        fs::write(dir.join(TRACE), b"trace").unwrap();
        Run::open(&dir).unwrap()
    }

    #[test]
    fn an_import_keeps_the_manifest_and_inputs_of_a_run_already_here() {
        // The run is recorded here, under the name "local", with its
        // inputs. Importing a view export of the same run that another
        // machine named "theirs" leaves the manifest here byte for byte,
        // so the run still names its own inputs. Importing a replayable
        // export of it leaves the manifest and the inputs too, and brings
        // the keyframe and the bookmarks the run here lacks.
        let (from, here) = (home("already-from"), home("already-here"));
        let theirs = replayable_run(&from, "theirs");
        let mut kf = rewind_vmm::snapshot::Keyframe::default();
        kf.step = 5;
        keyframes::save(&theirs.dir, &kf).unwrap();
        fs::write(theirs.dir.join(BOOKMARKS), b"theirs").unwrap();
        let view = from.root().join("view.rwd");
        let replayable = from.root().join("replayable.rwd");
        export(&from, &theirs, Contents::View, &view).unwrap();
        export(&from, &theirs, Contents::Replayable, &replayable).unwrap();

        let local = replayable_run(&here, "local");
        assert_eq!(local.manifest.id, theirs.manifest.id);
        let manifest_here = fs::read(local.dir.join(MANIFEST)).unwrap();

        let unchanged = |run: &Run| fs::read(run.dir.join(MANIFEST)).unwrap() == manifest_here;
        let got = import(&here, &view).unwrap();
        assert!(unchanged(&got), "the view export changed the manifest");
        assert_eq!(got.manifest.spec.kernel, local.manifest.spec.kernel);
        assert!(got.keyframes().unwrap().steps().is_empty());

        let got = import(&here, &replayable).unwrap();
        assert!(
            unchanged(&got),
            "the replayable export changed the manifest"
        );
        assert!(!here.inputs().join(&local.manifest.id).exists());
        assert_eq!(got.keyframes().unwrap().steps(), vec![5]);
        assert_eq!(fs::read(got.dir.join(BOOKMARKS)).unwrap(), b"theirs");
        for home in [from, here] {
            fs::remove_dir_all(home.root()).unwrap();
        }
    }

    #[test]
    fn an_import_gives_a_copy_without_inputs_the_export_s() {
        // The run came here first as a view export, renamed "mine" since,
        // and its manifest names a kernel and an initramfs that are not
        // on this machine. A replayable export of the same run then
        // brings its inputs: the manifest here names them under this
        // home's inputs, and keeps the name the run has here.
        let (from, here) = (home("inputs-from"), home("inputs-here"));
        let theirs = replayable_run(&from, "theirs");
        let view = from.root().join("view.rwd");
        let replayable = from.root().join("replayable.rwd");
        export(&from, &theirs, Contents::View, &view).unwrap();
        export(&from, &theirs, Contents::Replayable, &replayable).unwrap();
        fs::remove_dir_all(from.root().join("given")).unwrap();

        let viewed = import(&here, &view).unwrap();
        assert!(!viewed.manifest.spec.kernel.exists());
        let mut renamed = viewed.manifest.clone();
        renamed.name = "mine".into();
        let renamed = serde_json::to_vec_pretty(&renamed).unwrap();
        fs::write(viewed.dir.join(MANIFEST), renamed).unwrap();

        let got = import(&here, &replayable).unwrap();
        assert_eq!(got.manifest.name, "mine");
        assert!(got.manifest.spec.kernel.starts_with(here.inputs()));
        assert_eq!(fs::read(&got.manifest.spec.kernel).unwrap(), b"kernel");
        assert_eq!(fs::read(&got.manifest.spec.initrd).unwrap(), b"initrd");
        for home in [from, here] {
            fs::remove_dir_all(home.root()).unwrap();
        }
    }

    #[test]
    fn an_import_refuses_inputs_that_do_not_make_its_run() {
        // A replayable export repacked with another kernel, initramfs or
        // image, its manifest and trace as they were. Its inputs no
        // longer make the run its manifest names, so the import is
        // refused, into a home without the run, where nothing is placed,
        // and into a home with the run, whose inputs stay.
        let (empty, here) = (home("identity-empty"), home("identity-here"));
        let local = replayable_run(&here, "local");
        let manifest = fs::read(local.dir.join(MANIFEST)).unwrap();
        let altered = [
            vec![
                ("inputs/kernel", "not a kernel"),
                ("inputs/initrd", "initrd"),
            ],
            vec![("inputs/kernel", "kernel"), ("inputs/initrd", "other")],
            vec![("inputs/kernel", "kernel")],
            vec![
                ("inputs/kernel", "kernel"),
                ("inputs/initrd", "initrd"),
                ("inputs/image.erofs", "an image the run never booted"),
            ],
        ];
        for inputs in altered {
            let mut entries = vec![
                Entry::File(MANIFEST, &manifest),
                Entry::File(TRACE, b"trace"),
            ];
            entries.extend(
                inputs
                    .iter()
                    .map(|(name, bytes)| Entry::File(name, bytes.as_bytes())),
            );
            let bytes = archive(&entries);

            let refused = import_bytes(&empty, &bytes).unwrap_err();
            assert!(
                format!("{refused:#}").contains(local.manifest.id.as_str()),
                "{refused:#}"
            );
            assert_eq!(fs::read_dir(empty.runs()).unwrap().count(), 0);
            assert!(!empty.inputs().exists());

            assert!(import_bytes(&here, &bytes).is_err());
            assert_eq!(fs::read(&local.manifest.spec.kernel).unwrap(), b"kernel");
            assert!(fs::read(local.dir.join(MANIFEST)).unwrap() == manifest);
        }
        for home in [empty, here] {
            fs::remove_dir_all(home.root()).unwrap();
        }
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
        // A manifest naming a run that is not sixteen lowercase hex digits,
        // such as one that climbs out of the runs directory, as its own id,
        // its parent or its keyframes' source, is refused before anything
        // is placed, and the home is left with no run and nothing beside
        // its own directories.
        let home = home("bad-id");
        let naming = |field: &str, value: serde_json::Value| {
            let mut manifest: serde_json::Value = serde_json::from_slice(&finished(ID)).unwrap();
            manifest[field] = value;
            serde_json::to_vec(&manifest).unwrap()
        };
        let mut manifests = Vec::new();
        for id in ["../escape", "0123", "0123456789ABCDEF", "0123456789abcdeg"] {
            manifests.push(naming("id", id.into()));
        }

        // A parent or a keyframe source outside the runs directory is
        // refused the same way.
        manifests.push(naming(
            "parent",
            serde_json::json!({ "run": "../escape", "step": 1 }),
        ));
        manifests.push(naming(
            "shared_keyframes",
            serde_json::json!({ "run": "../escape", "through": 1 }),
        ));
        for manifest in manifests {
            let bytes = archive(&[
                Entry::File(MANIFEST, &manifest),
                Entry::File(TRACE, b"trace"),
            ]);
            assert!(
                import_bytes(&home, &bytes).is_err(),
                "{}",
                String::from_utf8_lossy(&manifest)
            );
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
    fn an_export_carries_the_runs_bookmarks() {
        // A run with a bookmarks file, exported for viewing and imported
        // into another home: the file arrives byte for byte. The engine
        // never reads it, so any bytes do.
        let from = home("bookmarks-from");
        let to = home("bookmarks-to");
        let dir = from.runs().join(ID);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(MANIFEST), finished(ID)).unwrap();
        fs::write(dir.join(TRACE), b"trace").unwrap();
        let bookmarks = br#"[{"step":5,"note":"here"}]"#;
        fs::write(dir.join(BOOKMARKS), bookmarks).unwrap();
        let out = from.root().join("run.rwd");

        export(&from, &Run::open(&dir).unwrap(), Contents::View, &out).unwrap();
        let run = import(&to, &out).unwrap();
        assert_eq!(fs::read(run.dir.join(BOOKMARKS)).unwrap(), bookmarks);
        fs::remove_dir_all(from.root()).unwrap();
        fs::remove_dir_all(to.root()).unwrap();
    }

    #[test]
    fn an_import_refuses_bookmarks_past_their_limit() {
        // Bookmarks are a person's notes; a file over MAX_BOOKMARKS bytes
        // is refused before it is unpacked, and no run is placed.
        let home = home("bookmarks-limit");
        let manifest = finished(ID);
        let huge = vec![b' '; MAX_BOOKMARKS as usize + 1];
        let bytes = archive(&[
            Entry::File(MANIFEST, &manifest),
            Entry::File(TRACE, b"trace"),
            Entry::File(BOOKMARKS, &huge),
        ]);
        assert!(import_bytes(&home, &bytes).is_err());
        assert_eq!(fs::read_dir(home.runs()).unwrap().count(), 0);
        fs::remove_dir_all(home.root()).unwrap();
    }

    #[test]
    fn an_import_keeps_the_bookmarks_of_a_copy_already_here() {
        // The run is here with bookmarks of its own: an export of the same
        // run with other bookmarks imports, and the ones here stay. A copy
        // here without bookmarks takes the export's.
        let home = home("bookmarks-local");
        let dir = home.runs().join(ID);
        fs::create_dir_all(&dir).unwrap();
        let mut local = serde_json::from_slice::<Manifest>(&finished(ID)).unwrap();
        local.trace_hash = Some(blake3_hex(b"trace"));
        fs::write(dir.join(MANIFEST), serde_json::to_vec(&local).unwrap()).unwrap();
        fs::write(dir.join(TRACE), b"trace").unwrap();
        fs::write(dir.join(BOOKMARKS), b"local").unwrap();

        let manifest = finished(ID);
        let bytes = archive(&[
            Entry::File(MANIFEST, &manifest),
            Entry::File(TRACE, b"trace"),
            Entry::File(BOOKMARKS, b"theirs"),
        ]);
        import_bytes(&home, &bytes).unwrap();
        assert_eq!(fs::read(dir.join(BOOKMARKS)).unwrap(), b"local");

        fs::remove_file(dir.join(BOOKMARKS)).unwrap();
        import_bytes(&home, &bytes).unwrap();
        assert_eq!(fs::read(dir.join(BOOKMARKS)).unwrap(), b"theirs");
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
