//! Removing what no run uses: cached images no run's manifest names,
//! pages in the page store no keyframe names, and the source file caches
//! of runs that are gone. `rewind remove` and `rewind prune` delete runs
//! and leave images and pages behind, since another run may still use
//! them.
//!
//! Collecting is safe against every other rewind process. It holds the
//! home alone (see [`Home::alone`]), which a process packing an image,
//! executing a run or mounting extras in a shell holds in use, so no image
//! is removed between being packed and the run that boots it writing its
//! manifest. It refuses while a run is executing, for a process that
//! holds no such lock. And it holds the page store alone, which every
//! process that reads or writes a page holds open, so no page is removed
//! that a keyframe being written names.
//!
//! An extras image for `rewind shell --with` is named by no run, and goes
//! like any other unused image: the shell that mounts it holds the home in
//! use for as long as it is open, and the next shell with the same
//! packages packs it again.

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rewind_store::{Collected, Collector, PageSet, ZERO_PAGE};
use rewind_vmm::snapshot::Keyframe;
use serde::Deserialize;

use crate::home::Home;
pub use crate::prune::Act;

/// The extension of an image file in the cache.
const IMAGE_EXTENSION: &str = "erofs";

/// An image no run uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    pub path: PathBuf,
    pub bytes: u64,
}

/// What a collection removed, or would.
#[derive(Clone, Debug, Default)]
pub struct Garbage {
    pub images: Vec<Image>,
    pub pages: Collected,
    /// Keyframe files that do not read back, such as ones an older build
    /// of rewind wrote. No build can restore them, so the pages they name
    /// are not kept for them.
    pub unreadable_keyframes: usize,
    /// The source file caches of runs that are no longer in the home.
    pub source_caches: Vec<PathBuf>,
}

impl Garbage {
    /// The bytes the images and pages take.
    pub fn bytes(&self) -> u64 {
        self.image_bytes() + self.pages.bytes
    }

    pub fn image_bytes(&self) -> u64 {
        self.images.iter().map(|i| i.bytes).sum()
    }
}

/// Why collecting removed nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Another rewind process holds the home in use or alone.
    InUse,
    /// A process is executing these runs.
    Executing(Vec<String>),
    /// Another process has the page store open.
    StoreOpen,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::InUse => write!(
                f,
                "another rewind process is packing an image, executing a run, or has a \
                 shell open; collect once it has finished"
            ),
            Refusal::Executing(runs) => write!(
                f,
                "run {} is executing; collect once it has finished",
                runs.join(", ")
            ),
            Refusal::StoreOpen => write!(
                f,
                "another process has the page store open, seeking in a run or the like; \
                 collect once it has finished"
            ),
        }
    }
}

impl std::error::Error for Refusal {}

/// Removes the images and pages no run in the home uses, or with
/// `Act::DryRun` finds them and removes nothing. Refused, removing
/// nothing, while another process may be about to use either; see the
/// module's description.
pub fn collect(home: &Home, act: Act) -> Result<Garbage> {
    let Some(_alone) = home.alone()? else {
        return Err(Refusal::InUse.into());
    };

    // A run executing now may not have named its image or written all its
    // keyframes yet.
    let executing: Vec<String> = run_dirs(home)?
        .into_iter()
        .filter(|dir| crate::run::executing(dir))
        .map(|dir| {
            dir.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    if !executing.is_empty() {
        return Err(Refusal::Executing(executing).into());
    }
    let Some(collector) = Collector::lock(&home.store())? else {
        return Err(Refusal::StoreOpen.into());
    };

    // Every check has passed: find the garbage, then remove the pages,
    // then the images and the source caches.
    let images = unused_images(home)?;
    let source_caches = orphan_source_caches(home)?;
    let (live, unreadable_keyframes) = live_pages(home)?;
    let store_act = match act {
        Act::DryRun => rewind_store::Act::DryRun,
        Act::Remove => rewind_store::Act::Remove,
    };
    let pages = collector
        .collect(&live, store_act)
        .context("collecting the page store")?;
    if act == Act::Remove {
        for image in &images {
            fs::remove_file(&image.path)
                .with_context(|| format!("removing {}", image.path.display()))?;
        }
        for cache in &source_caches {
            fs::remove_dir_all(cache).with_context(|| format!("removing {}", cache.display()))?;
        }
    }
    Ok(Garbage {
        images,
        pages,
        unreadable_keyframes,
        source_caches,
    })
}

/// The source file caches whose run is not in the home, removed or
/// imported elsewhere before `rewind remove` took its cache with it.
fn orphan_source_caches(home: &Home) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(home.source_cache()) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).context("reading the source file caches"),
    };
    let mut orphans = Vec::new();
    for entry in entries {
        let entry = entry?;
        if home.runs().join(entry.file_name()).is_dir() {
            continue;
        }
        orphans.push(entry.path());
    }
    orphans.sort();
    Ok(orphans)
}

/// The images in the cache that no run's manifest names, by path. Reads
/// only manifests, so it is cheap enough to say after each removal what
/// collecting would free; it takes no lock, so the answer is as of now.
pub fn unused_images(home: &Home) -> Result<Vec<Image>> {
    // Runs name images by path. Two paths are the same image when they are
    // the same file, which also matches a home reached through a symlink.
    let mut named: HashSet<PathBuf> = HashSet::new();
    for dir in run_dirs(home)? {
        if let Some(image) = named_image(&dir)? {
            named.insert(image);
        }
    }
    let used: HashSet<(u64, u64)> = named
        .iter()
        .filter_map(|path| fs::metadata(path).ok())
        .map(|meta| (meta.dev(), meta.ino()))
        .collect();

    let mut unused = Vec::new();
    for path in cached_images(home)? {
        let meta = fs::metadata(&path).with_context(|| format!("reading {}", path.display()))?;
        if used.contains(&(meta.dev(), meta.ino())) {
            continue;
        }
        unused.push(Image {
            path,
            bytes: meta.len(),
        });
    }
    unused.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(unused)
}

/// The directories in the home's runs directory.
fn run_dirs(home: &Home) -> Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    let entries = match fs::read_dir(home.runs()) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(dirs),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            dirs.push(entry.path());
        }
    }
    Ok(dirs)
}

/// The one field of a manifest that names the run's image.
#[derive(Deserialize)]
struct Uses {
    spec: UsesSpec,
}

#[derive(Deserialize)]
struct UsesSpec {
    image: Option<PathBuf>,
}

/// The image the run in `dir` boots, if it has one. A directory with no
/// manifest names none. A manifest that does not say is an error, since
/// collecting could otherwise remove the image the run needs.
fn named_image(dir: &Path) -> Result<Option<PathBuf>> {
    let path = dir.join(crate::run::MANIFEST);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let uses: Uses = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "{} does not say which image the run uses; remove the run or the file",
            path.display()
        )
    })?;
    Ok(uses.spec.image)
}

/// Every image file in the cache: at its top, where the first way of
/// building images kept them, and in the directory of each way since.
fn cached_images(home: &Home) -> Result<Vec<PathBuf>> {
    let mut images = Vec::new();
    let mut dirs = vec![home.image_cache()];
    for entry in fs::read_dir(home.image_cache())? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            dirs.push(entry.path());
        }
    }
    for dir in dirs {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let is_image = path.extension().is_some_and(|e| e == IMAGE_EXTENSION);
            if is_image && entry.file_type()?.is_file() {
                images.push(path);
            }
        }
    }
    Ok(images)
}

/// Every page a keyframe in the home names, and how many keyframe files
/// do not read back. A fork reads keyframes from the directories of the
/// runs it shares them with, so every run directory's keyframes count,
/// read one file at a time.
fn live_pages(home: &Home) -> Result<(PageSet, usize)> {
    let mut live = PageSet::default();
    let mut unreadable = 0;
    for dir in run_dirs(home)? {
        let keyframes = dir.join(crate::keyframes::DIR);
        let entries = match fs::read_dir(&keyframes) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(e).with_context(|| format!("reading {}", keyframes.display()));
            }
        };
        for entry in entries {
            let path = entry?.path();
            let is_keyframe = path
                .extension()
                .is_some_and(|e| e == crate::keyframes::EXTENSION);
            if !is_keyframe {
                continue;
            }
            let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            let Ok(kf) = bincode::deserialize::<Keyframe>(&bytes) else {
                unreadable += 1;
                continue;
            };
            live.extend(
                kf.pages
                    .into_iter()
                    .map(|(_, hash)| hash)
                    .filter(|hash| *hash != ZERO_PAGE),
            );
        }
    }
    Ok((live, unreadable))
}

#[cfg(test)]
mod tests {
    // collect() over a temporary home holding hand-made runs: manifests
    // that name only their image, keyframes that hold only pages, images
    // in both layouts of the images directory, and pages in a real page
    // store. Checks what goes, what stays, what a dry run leaves, and
    // when collecting refuses and removes nothing.
    use super::*;
    use rewind_store::{Hash, Store};
    use rewind_vmm::snapshot::Keyframe;

    const PAGE: usize = 4096;

    /// A fresh home for one test.
    fn home(name: &str) -> Home {
        let root = std::env::temp_dir().join(format!("rewind-gc-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        Home::at(root).unwrap()
    }

    /// An image file of `len` bytes at `path`.
    fn image(path: &Path, len: usize) -> PathBuf {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![1u8; len]).unwrap();
        path.to_path_buf()
    }

    /// A run directory whose manifest names `image`, with a keyframe at
    /// each step of `keyframes` naming its pages.
    fn run(home: &Home, id: &str, image: Option<&Path>, keyframes: &[(u64, &[Hash])]) -> PathBuf {
        let dir = home.runs().join(id);
        fs::create_dir_all(&dir).unwrap();
        let manifest = serde_json::json!({ "id": id, "spec": { "image": image } });
        fs::write(dir.join(crate::run::MANIFEST), manifest.to_string()).unwrap();
        for (step, pages) in keyframes {
            let mut kf = Keyframe::default();
            kf.step = *step;
            kf.pages = pages.iter().zip(0..).map(|(h, i)| (i, *h)).collect();
            crate::keyframes::save(&dir, &kf).unwrap();
        }
        dir
    }

    /// Stores a page for each fill and returns their hashes.
    fn pages(home: &Home, fills: &[u8]) -> Vec<Hash> {
        let mut store = Store::open(&home.store()).unwrap();
        let hashes = fills
            .iter()
            .map(|f| {
                let mut page = vec![0u8; PAGE];
                page[..64].fill(*f);
                store.put(&page).unwrap()
            })
            .collect();
        store.sync().unwrap();
        hashes
    }

    /// How often, and how far apart, `settled` tries again.
    const SETTLE_TRIES: u32 = 200;
    const SETTLE_WAIT: std::time::Duration = std::time::Duration::from_millis(10);

    /// `collect` once the locks a test let go of are free. Other tests in
    /// this process start programs, and a child holds a copy of every open
    /// file, and so every lock, from its fork until it execs; a lock this
    /// test dropped can stay held that long.
    fn settled(home: &Home, act: Act) -> Garbage {
        for _ in 0..SETTLE_TRIES {
            match collect(home, act) {
                Ok(garbage) => return garbage,
                Err(e) if e.downcast_ref::<Refusal>().is_some() => std::thread::sleep(SETTLE_WAIT),
                Err(e) => panic!("collecting failed: {e:#}"),
            }
        }
        panic!("collecting was refused {SETTLE_TRIES} times");
    }

    /// The refusal `collect` failed with.
    fn refusal(result: Result<Garbage>) -> Refusal {
        let err = result.expect_err("collecting should refuse");
        err.downcast_ref::<Refusal>()
            .unwrap_or_else(|| panic!("not a refusal: {err:#}"))
            .clone()
    }

    #[test]
    fn images_no_run_names_go_from_either_layout() {
        // Two runs name an image each, one in images/2 and one in the
        // older top-level images directory. The unused image beside each,
        // and an extras image no run names, go; a dry run names them and
        // deletes nothing. A leftover that is no image stays.
        let home = home("images");
        let top = home.root().join("images");
        let used_new = image(&home.images().join("store-a.erofs"), 10);
        let used_old = image(&top.join("store-b.erofs"), 20);
        let unused_new = image(&home.images().join("store-c.erofs"), 30);
        let unused_old = image(&top.join("0123abcd.erofs"), 40);
        let extras = image(&home.images().join("extras-d.erofs"), 50);
        let leftover = image(&home.images().join("store-e.tmp-1-0"), 60);
        run(&home, "r1", Some(&used_new), &[]);
        run(&home, "r2", Some(&used_old), &[]);
        run(&home, "r3", None, &[]);

        let planned = settled(&home, Act::DryRun);
        let mut named: Vec<(PathBuf, u64)> = planned
            .images
            .iter()
            .map(|i| (i.path.clone(), i.bytes))
            .collect();
        named.sort();
        let mut expected = vec![
            (unused_new.clone(), 30),
            (unused_old.clone(), 40),
            (extras.clone(), 50),
        ];
        expected.sort();
        assert_eq!(named, expected);
        assert_eq!(planned.bytes(), 120);
        assert!(unused_new.exists() && unused_old.exists() && extras.exists());

        let collected = settled(&home, Act::Remove);
        assert_eq!(collected.images.len(), 3);
        assert!(!unused_new.exists() && !unused_old.exists() && !extras.exists());
        assert!(used_new.exists() && used_old.exists() && leftover.exists());
        fs::remove_dir_all(home.root()).unwrap();
    }

    #[test]
    fn pages_no_keyframe_names_go() {
        // r names pages 0 and 1, and its fork f reads r's keyframes and
        // names page 2 in its own. Pages 3 and 4 are named by no keyframe
        // and go; a keyframe that does not read back keeps nothing and is
        // counted. A dry run counts the same and removes nothing.
        let home = home("pages");
        let hashes = pages(&home, &[1, 2, 3, 4, 5]);
        run(
            &home,
            "r",
            None,
            &[(256, &hashes[..1]), (512, &hashes[1..2])],
        );
        let fork = run(&home, "f", None, &[(1024, &hashes[2..3])]);
        fs::write(
            fork.join(crate::keyframes::DIR).join("0000000000002048.kf"),
            b"old",
        )
        .unwrap();

        let planned = settled(&home, Act::DryRun);
        assert_eq!(planned.pages.pages, 2);
        assert!(planned.pages.bytes > 0);
        assert_eq!(planned.unreadable_keyframes, 1);
        assert!(Store::open(&home.store()).unwrap().contains(&hashes[4]));

        let collected = settled(&home, Act::Remove);
        assert_eq!(collected.pages, planned.pages);
        let store = Store::open(&home.store()).unwrap();
        let mut out = vec![0u8; PAGE];
        for hash in &hashes[..3] {
            store.get(hash, &mut out).unwrap();
        }
        assert!(!store.contains(&hashes[3]));
        assert!(!store.contains(&hashes[4]));
        drop(store);
        fs::remove_dir_all(home.root()).unwrap();
    }

    #[test]
    fn source_caches_of_runs_that_are_gone_go() {
        // Runs r and gone each had source files cached, and gone's
        // directory was since removed: gone's cache goes, r's stays, and a
        // dry run names the one and removes nothing.
        use crate::source_cache::{Entry, SourceCache, Version};
        let home = home("source-caches");
        run(&home, "r", None, &[]);
        let file = Entry::File(b"int x;\n".to_vec());
        for id in ["r", "gone"] {
            SourceCache::of(&home, id)
                .put(Version::Original, "/src/a.c", &file)
                .unwrap();
        }
        let gone = home.source_cache().join("gone");

        let planned = settled(&home, Act::DryRun);
        assert_eq!(planned.source_caches, vec![gone.clone()]);
        assert!(gone.exists());

        let collected = settled(&home, Act::Remove);
        assert_eq!(collected.source_caches, vec![gone.clone()]);
        assert!(!gone.exists());
        let kept = SourceCache::of(&home, "r").get(Version::Original, "/src/a.c");
        assert_eq!(kept, Some(file));
        fs::remove_dir_all(home.root()).unwrap();
    }

    #[test]
    fn an_executing_run_refuses_and_removes_nothing() {
        // While a process executes run u, which may not have named its
        // pages or even its image yet, collecting refuses and leaves the
        // unused image and page. Once the execution ends, interrupted or
        // not, collecting goes ahead.
        let home = home("executing");
        let unused = image(&home.images().join("store-x.erofs"), 10);
        let hashes = pages(&home, &[1]);
        let dir = run(&home, "u", None, &[]);
        let executing = crate::run::lock_executing(&dir).unwrap();

        for act in [Act::DryRun, Act::Remove] {
            assert_eq!(
                refusal(collect(&home, act)),
                Refusal::Executing(vec!["u".into()])
            );
        }
        assert!(unused.exists());
        assert!(Store::open(&home.store()).unwrap().contains(&hashes[0]));

        drop(executing);
        settled(&home, Act::Remove);
        assert!(!unused.exists());
        fs::remove_dir_all(home.root()).unwrap();
    }

    #[test]
    fn a_home_in_use_refuses() {
        // A process packing an image or about to start a run holds the
        // home in use, and collecting refuses until it lets go.
        let home = home("in-use");
        let unused = image(&home.images().join("store-x.erofs"), 10);
        let in_use = home.in_use().unwrap();
        assert_eq!(refusal(collect(&home, Act::Remove)), Refusal::InUse);
        assert!(unused.exists());
        drop(in_use);
        settled(&home, Act::Remove);
        assert!(!unused.exists());
        fs::remove_dir_all(home.root()).unwrap();
    }

    #[test]
    fn an_open_page_store_refuses() {
        // A process seeking in a run has the page store open and may be
        // about to read any page, so collecting refuses, images included.
        let home = home("store-open");
        let unused = image(&home.images().join("store-x.erofs"), 10);
        let store = Store::open(&home.store()).unwrap();
        assert_eq!(refusal(collect(&home, Act::Remove)), Refusal::StoreOpen);
        assert!(unused.exists());
        drop(store);
        fs::remove_dir_all(home.root()).unwrap();
    }
}
