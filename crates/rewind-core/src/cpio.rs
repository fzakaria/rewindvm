//! A newc cpio writer, for the archive of per-run files the monitor
//! appends to the initramfs.
//!
//! The kernel unpacks every archive concatenated into the initramfs in
//! order, so one written here lands on top of the base archive from
//! nix/guest.nix.

const MAGIC: &str = "070701";
const TRAILER: &str = "TRAILER!!!";

/// The kernel requires each archive in an initramfs to start on a four
/// byte boundary; padding the end to 512 keeps the next one aligned too.
const ARCHIVE_ALIGN: usize = 512;

const MODE_DIR: u32 = 0o040000;
const MODE_FILE: u32 = 0o100000;

/// An archive being built. Every entry is owned by root with a fixed
/// timestamp, so the same files always make the same bytes.
#[derive(Default)]
pub struct Archive {
    out: Vec<u8>,
    ino: u32,
}

impl Archive {
    pub fn new() -> Archive {
        Archive::default()
    }

    pub fn dir(&mut self, path: &str, perm: u32) {
        self.entry(path, MODE_DIR | perm, &[]);
    }

    pub fn file(&mut self, path: &str, perm: u32, data: &[u8]) {
        self.entry(path, MODE_FILE | perm, data);
    }

    pub fn finish(mut self) -> Vec<u8> {
        self.entry(TRAILER, 0, &[]);
        let padded = self.out.len().div_ceil(ARCHIVE_ALIGN) * ARCHIVE_ALIGN;
        self.out.resize(padded, 0);
        self.out
    }

    fn entry(&mut self, path: &str, mode: u32, data: &[u8]) {
        const MTIME: u32 = 1;
        self.ino += 1;
        let name = path.trim_start_matches('/');
        let nlink = if mode & MODE_DIR != 0 { 2 } else { 1 };
        let header = format!(
            "{MAGIC}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}",
            self.ino,
            mode,
            0, // uid
            0, // gid
            nlink,
            MTIME,
            data.len(),
            0, // devmajor
            0, // devminor
            0, // rdevmajor
            0, // rdevminor
            name.len() + 1,
            0, // check
        );
        self.out.extend_from_slice(header.as_bytes());
        self.out.extend_from_slice(name.as_bytes());
        self.out.push(0);
        self.pad4();
        self.out.extend_from_slice(data);
        self.pad4();
    }

    fn pad4(&mut self) {
        let padded = self.out.len().div_ceil(4) * 4;
        self.out.resize(padded, 0);
    }
}

#[cfg(test)]
mod tests {
    // The writer against cpio(1): an archive written here must list back
    // the entries it was given, and two identical archives must be equal.
    use super::*;

    fn sample() -> Vec<u8> {
        let mut a = Archive::new();
        a.dir("/rewind", 0o755);
        a.file("/rewind/job.json", 0o644, b"{}");
        a.finish()
    }

    #[test]
    fn is_reproducible_and_aligned() {
        let a = sample();
        assert_eq!(a, sample());
        assert_eq!(a.len() % ARCHIVE_ALIGN, 0);
        assert!(a.starts_with(MAGIC.as_bytes()));
    }

    #[test]
    fn cpio_lists_the_entries() {
        let dir = std::env::temp_dir().join(format!("rewind-cpio-{}", std::process::id()));
        std::fs::write(&dir, sample()).unwrap();
        let out = std::process::Command::new("cpio")
            .args(["-t", "--quiet", "-F"])
            .arg(&dir)
            .output();
        std::fs::remove_file(&dir).unwrap();
        let Ok(out) = out else {
            // cpio is on PATH in the dev shell and the Nix check; skip
            // quietly anywhere else.
            return;
        };
        let listing = String::from_utf8(out.stdout).unwrap();
        assert_eq!(listing, "rewind\nrewind/job.json\n");
    }
}
