//! Samples of the file formats rewind writes and later builds must read:
//! a run's manifest, its trace and its keyframes, and the page store. Each
//! sample under tests/formats/ is named for the version of its format, and
//! its BLAKE3 hash is pinned below.
//!
//! The sample of each format's current version must read and write back
//! byte for byte, so changing what a format holds without changing its
//! version fails here. A sample never changes once its version is
//! released: a new format is a new version with a sample of its own, and
//! the old samples stay, for the code that reads old versions to be tested
//! against. `REWIND_WRITE_SAMPLES=1` writes the samples of the current
//! versions that the tests make up (the keyframe and the store); the
//! manifest and the trace are copied from a run recorded with the build.

use std::fs;
use std::path::{Path, PathBuf};

use rewind_core::keyframes::{self, KEYFRAME_VERSION};
use rewind_store::{STORE_VERSION, Store, hex};
use rewind_trace::manifest::{MANIFEST_VERSION, Manifest};
use rewind_trace::{TRACE_VERSION, TraceWriter};
use rewind_vmm::snapshot::Keyframe;

/// Every sample file, by its path under tests/formats/, and the BLAKE3
/// hash of its contents.
const SAMPLES: &[(&str, &str)] = &[
    (
        "keyframe-1.kf",
        "13b493d68a226b3ded464eed2be9d71163bf0839f00846b7537bb208cc5ba870",
    ),
    (
        "manifest-2.json",
        "1760004634e87ae88359afd7e683f4b59f31070f3a4e2bdad7cafbf076eb29d2",
    ),
    (
        "store-1/format",
        "50cc1102b1c612e6962547aacdcef9a400d4416ef8dd9388e885991853c400c9",
    ),
    (
        "store-1/index",
        "835336a3f0bea20251acec562fc21bdbd769fefa9694b30d7c3cdd706d4c772a",
    ),
    (
        "store-1/packs/00000000.pack",
        "c00a0ce8a2fbfe0187afdf64f2ae2b53ce2c897f53519903865ffa0d53870515",
    ),
    (
        "trace-1.bin",
        "5c4753ae7fe3ee9005df25ae8a09a9fd731c92c99c0383dd413f903ba7638017",
    ),
];

/// The environment variable that has the tests write the samples they
/// make up.
const WRITE_SAMPLES: &str = "REWIND_WRITE_SAMPLES";

/// The pages the store sample holds, each a page of one byte repeated.
const STORE_FILLS: [u8; 2] = [0x11, 0x22];
const PAGE: usize = 4096;

fn samples() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/formats")
}

fn manifest_sample() -> PathBuf {
    samples().join(format!("manifest-{MANIFEST_VERSION}.json"))
}

fn trace_sample() -> PathBuf {
    samples().join(format!("trace-{TRACE_VERSION}.bin"))
}

fn keyframe_sample() -> PathBuf {
    samples().join(format!("keyframe-{KEYFRAME_VERSION}.kf"))
}

fn store_sample() -> PathBuf {
    samples().join(format!("store-{STORE_VERSION}"))
}

/// The files of the store sample that are its format: the format file,
/// the index and the pack. Opening a store adds locks, which are not.
const STORE_FILES: [&str; 3] = ["format", "index", "packs/00000000.pack"];

/// The keyframe the keyframe sample holds: every field its default but the
/// step, the parent and a few pages, so the sample stays small.
fn sample_keyframe() -> Keyframe {
    let mut kf = Keyframe::default();
    kf.step = 4096;
    kf.parent = Some(2048);
    kf.pages = vec![(0, [1; 32]), (7, [2; 32])];
    kf
}

fn page(fill: u8) -> Vec<u8> {
    vec![fill; PAGE]
}

/// The made-up samples, written when asked for, once however many tests
/// ask.
fn write_samples_if_asked() {
    static WRITTEN: std::sync::Once = std::sync::Once::new();
    if std::env::var_os(WRITE_SAMPLES).is_some() {
        WRITTEN.call_once(write_samples);
    }
}

fn write_samples() {
    fs::create_dir_all(samples()).unwrap();
    fs::write(
        keyframe_sample(),
        keyframes::encode(&sample_keyframe()).unwrap(),
    )
    .unwrap();
    let store = store_sample();
    let _ = fs::remove_dir_all(&store);
    let mut opened = Store::open(&store).unwrap();
    for fill in STORE_FILLS {
        opened.put(&page(fill)).unwrap();
    }
    drop(opened);
    for entry in fs::read_dir(&store).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_str().unwrap();
        if path.is_file() && !STORE_FILES.contains(&name) {
            fs::remove_file(&path).unwrap();
        }
    }
}

/// Each sample is as it was checked in: its hash is the one pinned, and
/// every file under tests/formats/ has a pinned hash. A sample that
/// changed is a format changed without a new version.
#[test]
fn samples_are_as_they_were_checked_in() {
    write_samples_if_asked();
    let mut found = Vec::new();
    let mut dirs = vec![samples()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
                continue;
            }
            let name = path.strip_prefix(samples()).unwrap();
            found.push(name.to_str().unwrap().to_string());
        }
    }
    found.sort();
    let mut pinned: Vec<String> = SAMPLES.iter().map(|(name, _)| name.to_string()).collect();
    pinned.sort();

    let mut wrong = Vec::new();
    for name in &found {
        let got = hex(&Store::hash(&fs::read(samples().join(name)).unwrap()));
        match SAMPLES.iter().find(|(n, _)| n == name) {
            Some((_, want)) if *want == got => {}
            Some(_) => wrong.push(format!("{name} changed: now {got}")),
            None => wrong.push(format!("{name} is not pinned: (\"{name}\", \"{got}\"),")),
        }
    }
    for name in pinned.iter().filter(|n| !found.contains(n)) {
        wrong.push(format!("{name} is pinned but gone"));
    }
    assert!(
        wrong.is_empty(),
        "a released sample never changes; for a new format, bump its version and add a \
         sample of the new one:\n{}",
        wrong.join("\n")
    );
}

/// The manifest sample reads, and writes back as the same bytes.
#[test]
fn the_manifest_sample_reads_and_writes_back() {
    let bytes = fs::read(manifest_sample()).expect("a manifest sample of the current version");
    let manifest = Manifest::parse(&bytes).unwrap();
    assert_eq!(serde_json::to_vec_pretty(&manifest).unwrap(), bytes);
}

/// The trace sample's records read, and write back as the same bytes.
#[test]
fn the_trace_sample_reads_and_writes_back() {
    let path = trace_sample();
    let bytes = fs::read(&path).expect("a trace sample of the current version");
    let records = rewind_trace::records(&path).unwrap();
    assert!(!records.is_empty());
    rewind_trace::Trace::decode(&records).unwrap();
    let mut writer = TraceWriter::new(Vec::new());
    for (step, record) in &records {
        writer.record(*step, record).unwrap();
    }
    assert_eq!(writer.finish().unwrap(), bytes);
}

/// The keyframe sample reads as the keyframe it was made from, and writes
/// back as the same bytes.
#[test]
fn the_keyframe_sample_reads_and_writes_back() {
    write_samples_if_asked();
    let bytes = fs::read(keyframe_sample()).expect("a keyframe sample of the current version");
    let kf = keyframes::decode(&bytes).unwrap();
    let want = sample_keyframe();
    assert_eq!(
        (kf.step, kf.parent, &kf.pages),
        (want.step, want.parent, &want.pages)
    );
    assert_eq!(keyframes::encode(&kf).unwrap(), bytes);
}

/// A copy of the store sample opens, and gives back every page it holds.
#[test]
fn the_store_sample_gives_back_its_pages() {
    write_samples_if_asked();
    let copy = std::env::temp_dir().join(format!("rewind-store-sample-{}", std::process::id()));
    let _ = fs::remove_dir_all(&copy);
    for file in STORE_FILES {
        let to = copy.join(file);
        fs::create_dir_all(to.parent().unwrap()).unwrap();
        fs::copy(store_sample().join(file), &to).expect("a store sample of the current version");
    }
    let store = Store::open(&copy).unwrap();
    let mut out = vec![0u8; PAGE];
    for fill in STORE_FILLS {
        store.get(&Store::hash(&page(fill)), &mut out).unwrap();
        assert_eq!(out, page(fill));
    }
    fs::remove_dir_all(&copy).unwrap();
}
