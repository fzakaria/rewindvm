//! What becomes of a file tree read out of a run at a step.
//!
//! The guest's init sends a tree as entries, each file's bytes after the
//! file's entry (see [`rewind_init::TreeEntry`]), and
//! [`crate::inspect::tree`] hands them on as they arrive. Here are the
//! two places a tree goes: a tar archive and a directory on this
//! machine's disk. An archive holds what the VM held and nothing of
//! this machine: the names, the contents, and for a tar the permissions,
//! owners and times the VM's filesystem had. So the same run at the same
//! step is the same archive on any machine.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use rewind_init::{ListedKind, TreeEntry, TreeSink};

/// What separates the names in an entry's path.
const SEPARATOR: u8 = b'/';

/// The name a tar gives a tree whose path has no name of its own, as `/`
/// has none.
pub const UNNAMED_ROOT: &str = ".";

/// The size tar pads each file's contents to a multiple of.
const TAR_BLOCK: u64 = 512;

/// An entry a tree holds that an archive cannot: a device, a socket or a
/// pipe.
fn unarchivable(entry: &TreeEntry) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "{} is a device, a socket or a pipe, which has no contents to keep",
            String::from_utf8_lossy(&entry.path)
        ),
    )
}

/// A tree written as a tar archive: GNU format, each entry with the
/// permissions, the numeric owner and group and the modification time the
/// VM's filesystem held, under the tree's own name, as `tar -c -C parent
/// name` lays an archive out. A device, a socket or a pipe is left out,
/// as tar leaves a socket out, and counted.
pub struct Tar<W: Write> {
    archive: tar::Builder<Abandonable<W>>,
    /// The name the tree's root gets in the archive.
    name: Vec<u8>,
    /// How many zero bytes follow the contents of the file being written.
    padding: u64,
    left_out: Option<LeftOut>,
}

/// The entries a tar left out: how many, and the path of the first below
/// the tree's root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeftOut {
    pub count: usize,
    pub first: Vec<u8>,
}

/// A tar's output, which takes no more bytes once the tar is abandoned.
/// The tar crate ends an archive on drop. Only a successful transfer may
/// write the end blocks. Readers can accept missing end blocks, so callers
/// must also check the producer's exit status.
struct Abandonable<W> {
    out: W,
    abandoned: bool,
}

impl<W: Write> Write for Abandonable<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.abandoned {
            return Ok(bytes.len());
        }
        self.out.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

impl<W: Write> Drop for Tar<W> {
    /// Abandons the output before the tar crate's archive is dropped. An
    /// archive that [`Tar::finish`] ended has nothing left to write.
    fn drop(&mut self) {
        self.archive.get_mut().abandoned = true;
    }
}

impl<W: Write> std::fmt::Debug for Tar<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tar")
            .field("name", &self.name)
            .field("left_out", &self.left_out)
            .finish()
    }
}

impl<W: Write> Tar<W> {
    /// An archive on `out` of a tree named `name`.
    pub fn new(out: W, name: &[u8]) -> Tar<W> {
        let out = Abandonable {
            out,
            abandoned: false,
        };
        Tar {
            archive: tar::Builder::new(out),
            name: name.to_vec(),
            padding: 0,
            left_out: None,
        }
    }

    /// Ends the archive and returns the entries left out of it, when any
    /// were.
    pub fn finish(mut self) -> io::Result<Option<LeftOut>> {
        self.archive.finish()?;
        self.archive.get_mut().flush()?;
        Ok(self.left_out.take())
    }

    /// An entry's path in the archive: the tree's name, then the entry's
    /// path below the tree's root.
    fn path_of(&self, entry: &TreeEntry) -> PathBuf {
        let mut path = self.name.clone();
        if !entry.path.is_empty() {
            path.push(SEPARATOR);
            path.extend_from_slice(&entry.path);
        }

        // A directory's name ends with a slash, as tar writes one.
        if entry.kind == ListedKind::Directory {
            path.push(SEPARATOR);
        }
        PathBuf::from(OsStr::from_bytes(&path))
    }
}

impl<W: Write> TreeSink for Tar<W> {
    fn entry(&mut self, entry: &TreeEntry) -> io::Result<()> {
        let kind = match entry.kind {
            ListedKind::File => tar::EntryType::Regular,
            ListedKind::Directory => tar::EntryType::Directory,
            ListedKind::Symlink => tar::EntryType::Symlink,
            ListedKind::Other => {
                let left_out = self.left_out.get_or_insert_with(|| LeftOut {
                    count: 0,
                    first: entry.path.clone(),
                });
                left_out.count += 1;
                return Ok(());
            }
        };

        // What the VM's filesystem held, and nothing else: no user or
        // group names, which only this machine could give.
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(kind);
        header.set_mode(entry.mode);
        header.set_uid(u64::from(entry.uid));
        header.set_gid(u64::from(entry.gid));
        header.set_mtime(u64::try_from(entry.mtime).unwrap_or(0));
        header.set_size(entry.size);

        // The header alone: a file's contents arrive after, in parts, and
        // are written as they do.
        let path = self.path_of(entry);
        if entry.kind == ListedKind::Symlink {
            let target = Path::new(OsStr::from_bytes(&entry.target));
            return self.archive.append_link(&mut header, path, target);
        }
        self.padding = (TAR_BLOCK - entry.size % TAR_BLOCK) % TAR_BLOCK;
        self.archive.append_data(&mut header, path, io::empty())
    }

    fn contents(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.archive.get_mut().write_all(bytes)
    }

    fn end_of_file(&mut self) -> io::Result<()> {
        let zeros = [0u8; TAR_BLOCK as usize];
        self.archive
            .get_mut()
            .write_all(&zeros[..self.padding as usize])
    }
}

/// A tree written onto this machine's disk, its root at a path nothing
/// has yet: each file with its contents and permissions, each link with
/// its target. Owners and times are this machine's, as for any file
/// written here. A device, a socket or a pipe is refused.
#[derive(Debug)]
pub struct Unpack {
    root: PathBuf,
    /// The file being written, and the permissions it gets once whole.
    file: Option<(File, u32)>,
    /// The directories made, with the permissions each gets once the
    /// tree is whole: a directory the VM kept read-only still has to take
    /// its entries here.
    directories: Vec<(PathBuf, u32)>,
    /// The directories made, for the parent check each entry takes.
    made: HashSet<PathBuf>,
}

impl Unpack {
    /// A tree whose root lands at `root`.
    pub fn new(root: PathBuf) -> Unpack {
        Unpack {
            root,
            file: None,
            directories: Vec::new(),
            made: HashSet::new(),
        }
    }

    /// Gives every directory its permissions, the innermost first.
    pub fn finish(self) -> io::Result<()> {
        for (path, mode) in self.directories.iter().rev() {
            fs::set_permissions(path, fs::Permissions::from_mode(*mode))?;
        }
        Ok(())
    }

    /// Where an entry lands. An error for a path that would land outside
    /// the root, which init never sends, and for one whose parent is not
    /// a directory this tree made: a symlink sent first would take a file
    /// "under" it anywhere on this machine the link points, as a guest the
    /// build took over could send one.
    fn path_of(&self, entry: &TreeEntry) -> io::Result<PathBuf> {
        if entry.path.is_empty() {
            return Ok(self.root.clone());
        }
        let outside = entry
            .path
            .split(|b| *b == SEPARATOR)
            .any(|name| name.is_empty() || name == b"." || name == b"..");
        if outside {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} is no path below the tree's root",
                    String::from_utf8_lossy(&entry.path)
                ),
            ));
        }

        // The parent must be a directory of this tree, the root included:
        // every path from the root down is then one of directories made
        // here, and no symlink is on the way.
        let path = self.root.join(OsStr::from_bytes(&entry.path));
        let parent = path.parent().unwrap_or(&self.root);
        if !self.made.contains(parent) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} is not under a directory of the tree",
                    String::from_utf8_lossy(&entry.path)
                ),
            ));
        }
        Ok(path)
    }
}

impl TreeSink for Unpack {
    fn entry(&mut self, entry: &TreeEntry) -> io::Result<()> {
        let path = self.path_of(entry)?;
        match entry.kind {
            ListedKind::Directory => {
                fs::create_dir(&path)?;
                self.made.insert(path.clone());
                self.directories.push((path, entry.mode));
            }
            ListedKind::Symlink => {
                std::os::unix::fs::symlink(OsStr::from_bytes(&entry.target), &path)?;
            }
            ListedKind::File => {
                self.file = Some((File::create_new(&path)?, entry.mode));
            }
            ListedKind::Other => return Err(unarchivable(entry)),
        }
        Ok(())
    }

    fn contents(&mut self, bytes: &[u8]) -> io::Result<()> {
        match &mut self.file {
            Some((file, _)) => file.write_all(bytes),
            None => Ok(()),
        }
    }

    fn end_of_file(&mut self) -> io::Result<()> {
        let Some((file, mode)) = self.file.take() else {
            return Ok(());
        };
        file.set_permissions(fs::Permissions::from_mode(mode))
    }
}

#[cfg(test)]
mod tests {
    // Send a sample tree through both sinks. Read the tar back with the
    // tar crate and check the unpacked files on disk.
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;

    use rewind_init::TreeReader;

    use super::*;

    /// The VM's clock when the sample tree's entries were last written,
    /// and their owner and group there: a Nix build's.
    const MTIME: i64 = 1_790_985_600;
    const UID: u32 = 1000;
    const GID: u32 = 100;

    /// A name longer than a tar header's 100 bytes holds.
    fn long_name() -> String {
        "n".repeat(150)
    }

    /// An entry of the sample tree.
    fn entry(path: &str, kind: ListedKind, mode: u32, size: usize, target: &str) -> TreeEntry {
        TreeEntry {
            path: path.as_bytes().to_vec(),
            kind,
            mode,
            uid: UID,
            gid: GID,
            mtime: MTIME,
            size: size as u64,
            target: target.as_bytes().to_vec(),
        }
    }

    /// A tree as init sends one: a directory holding an executable, a
    /// link to it, a file only its owner reads, a file with a long name,
    /// an empty file, a read-only directory with a file, and an empty
    /// directory. Each entry comes with its file's contents.
    fn sample() -> Vec<(TreeEntry, Vec<u8>)> {
        let tool = b"#!/bin/sh\necho hi\n".to_vec();
        let secret = vec![0u8, 255, 10, 13, 0];
        let page = b"a page".to_vec();
        let long = format!("share/{}", long_name());
        vec![
            (entry("", ListedKind::Directory, 0o755, 0, ""), vec![]),
            (entry("bin", ListedKind::Directory, 0o755, 0, ""), vec![]),
            (
                entry("bin/alias", ListedKind::Symlink, 0o777, 0, "tool"),
                vec![],
            ),
            (
                entry("bin/tool", ListedKind::File, 0o755, tool.len(), ""),
                tool,
            ),
            (entry("empty", ListedKind::Directory, 0o700, 0, ""), vec![]),
            (entry("locked", ListedKind::Directory, 0o555, 0, ""), vec![]),
            (
                entry("locked/page", ListedKind::File, 0o444, page.len(), ""),
                page,
            ),
            (
                entry("secret", ListedKind::File, 0o600, secret.len(), ""),
                secret,
            ),
            (entry("share", ListedKind::Directory, 0o755, 0, ""), vec![]),
            (entry(&long, ListedKind::File, 0o644, 0, ""), vec![]),
        ]
    }

    /// Sends `tree` to `sink` the way it arrives from a VM: as bytes, in
    /// parts that cut entries and contents anywhere.
    fn send(tree: &[(TreeEntry, Vec<u8>)], sink: &mut impl TreeSink) -> io::Result<()> {
        /// A part size that divides neither a header nor the contents.
        const PART: usize = 11;

        let mut bytes = Vec::new();
        for (entry, contents) in tree {
            bytes.extend(entry.header());
            bytes.extend(contents);
        }
        let mut reader = TreeReader::default();
        for part in bytes.chunks(PART) {
            reader.feed(part, sink)?;
        }
        reader.finish()
    }

    /// A directory of this test's own, empty.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rewind-tree-{name}-{}", std::process::id()));
        let _ = make_removable(&dir);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Lets the owner into every directory under `dir`, so a tree with a
    /// read-only directory can be removed.
    fn make_removable(dir: &Path) -> io::Result<()> {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            if path.symlink_metadata()?.is_dir() {
                make_removable(&path)?;
            }
        }
        Ok(())
    }

    #[test]
    fn a_tar_holds_the_tree_as_the_vm_held_it() {
        // The sample tree as a tar, read back with the tar crate: every
        // entry under the tree's name, in the order sent, with its kind,
        // permissions, owner, group, time, contents and target, a name
        // longer than a header holds included. The same tree makes the
        // same bytes a second time, and nothing was left out.
        let archive = || {
            let mut bytes = Vec::new();
            let mut tar = Tar::new(&mut bytes, b"out");
            send(&sample(), &mut tar).unwrap();
            assert_eq!(tar.finish().unwrap(), None);
            bytes
        };
        let bytes = archive();
        assert_eq!(bytes.len() as u64 % TAR_BLOCK, 0);
        assert_eq!(bytes, archive());

        let mut read = tar::Archive::new(bytes.as_slice());
        let mut seen = Vec::new();
        for (file, (sent, contents)) in read.entries().unwrap().zip(sample()) {
            let mut file = file.unwrap();
            let header = file.header().clone();
            assert_eq!(header.mode().unwrap(), sent.mode);
            assert_eq!(header.uid().unwrap(), u64::from(UID));
            assert_eq!(header.gid().unwrap(), u64::from(GID));
            assert_eq!(header.mtime().unwrap(), MTIME as u64);
            assert_eq!(header.username().unwrap(), Some(""));
            let kind = match sent.kind {
                ListedKind::File => tar::EntryType::Regular,
                ListedKind::Directory => tar::EntryType::Directory,
                _ => tar::EntryType::Symlink,
            };
            assert_eq!(header.entry_type(), kind);
            let target = file.link_name().unwrap().map(|t| t.into_owned());
            let wanted = (sent.kind == ListedKind::Symlink).then(|| PathBuf::from("tool"));
            assert_eq!(target, wanted);
            let mut read = Vec::new();
            file.read_to_end(&mut read).unwrap();
            assert_eq!(read, contents);
            seen.push(file.path().unwrap().display().to_string());
        }
        let long = format!("out/share/{}", long_name());
        assert_eq!(
            seen,
            vec![
                "out/",
                "out/bin/",
                "out/bin/alias",
                "out/bin/tool",
                "out/empty/",
                "out/locked/",
                "out/locked/page",
                "out/secret",
                "out/share/",
                long.as_str(),
            ]
        );
    }

    #[test]
    fn a_tar_abandoned_part_way_does_not_end() {
        // A tree the VM could not send whole leaves a tar that was never
        // ended. The bytes written hold the entries that arrived and no
        // end mark. A tar of a path nothing had is empty. Neither property
        // replaces checking the producer's exit status.
        let whole = {
            let mut bytes = Vec::new();
            let mut tar = Tar::new(&mut bytes, b"out");
            send(&sample(), &mut tar).unwrap();
            tar.finish().unwrap();
            bytes
        };
        let mut abandoned = Vec::new();
        let mut tar = Tar::new(&mut abandoned, b"out");
        send(&sample(), &mut tar).unwrap();
        drop(tar);
        assert_eq!(abandoned.len() as u64, whole.len() as u64 - 2 * TAR_BLOCK);
        assert!(whole.starts_with(&abandoned));

        let mut nothing = Vec::new();
        drop(Tar::new(&mut nothing, b"out"));
        assert!(nothing.is_empty());
    }

    #[test]
    fn a_tar_leaves_out_what_has_no_contents() {
        // A socket between two files: the tar holds the two files, and
        // says one entry was left out and which. A file on its own, as
        // `rewind cat --archive` of a file sends, is an archive of that
        // one name.
        let tree = vec![
            (entry("", ListedKind::Directory, 0o755, 0, ""), vec![]),
            (entry("a", ListedKind::File, 0o644, 1, ""), b"a".to_vec()),
            (entry("sock", ListedKind::Other, 0o755, 0, ""), vec![]),
            (entry("z", ListedKind::File, 0o644, 1, ""), b"z".to_vec()),
        ];
        let mut bytes = Vec::new();
        let mut tar = Tar::new(&mut bytes, UNNAMED_ROOT.as_bytes());
        send(&tree, &mut tar).unwrap();
        let left_out = LeftOut {
            count: 1,
            first: b"sock".to_vec(),
        };
        assert_eq!(tar.finish().unwrap(), Some(left_out));
        let names = |bytes: &[u8]| -> Vec<String> {
            tar::Archive::new(bytes)
                .entries()
                .unwrap()
                .map(|e| e.unwrap().path().unwrap().display().to_string())
                .collect()
        };
        // The tar crate writes a path below `.` without the `./`.
        assert_eq!(names(&bytes), vec!["./", "a", "z"]);

        let file = vec![(entry("", ListedKind::File, 0o644, 2, ""), b"hi".to_vec())];
        let mut bytes = Vec::new();
        let mut tar = Tar::new(&mut bytes, b"notes.txt");
        send(&file, &mut tar).unwrap();
        tar.finish().unwrap();
        assert_eq!(names(&bytes), vec!["notes.txt"]);
    }

    #[test]
    fn an_unpacked_tree_keeps_contents_and_permissions() {
        // The sample tree unpacked onto disk has each file's contents and
        // permissions, the link, and the directories with theirs, the
        // read-only one included.
        let dir = scratch("unpack");
        let root = dir.join("out");
        let mut unpack = Unpack::new(root.clone());
        send(&sample(), &mut unpack).unwrap();
        unpack.finish().unwrap();

        let mode = |path: &str| fs::symlink_metadata(root.join(path)).unwrap().mode() & 0o7777;
        assert_eq!(
            fs::read(root.join("bin/tool")).unwrap(),
            b"#!/bin/sh\necho hi\n"
        );
        assert_eq!(mode("bin/tool"), 0o755);
        assert_eq!(mode("secret"), 0o600);
        assert_eq!(fs::read(root.join("secret")).unwrap(), [0, 255, 10, 13, 0]);
        assert_eq!(mode("locked"), 0o555);
        assert_eq!(mode("locked/page"), 0o444);
        assert_eq!(mode("empty"), 0o700);
        assert_eq!(fs::read_dir(root.join("empty")).unwrap().count(), 0);
        assert_eq!(
            fs::read_link(root.join("bin/alias")).unwrap(),
            PathBuf::from("tool")
        );
        assert!(root.join("share").join(long_name()).is_file());

        make_removable(&dir).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn what_cannot_be_kept_is_refused() {
        // Unpacking refuses a socket, as Nix refuses one in a store
        // path. Unpacking also refuses a path that would land
        // outside the tree's root, and a root something already has.
        let socket = vec![
            (entry("", ListedKind::Directory, 0o755, 0, ""), vec![]),
            (entry("sock", ListedKind::Other, 0o755, 0, ""), vec![]),
        ];
        let dir = scratch("refuse");
        let mut unpack = Unpack::new(dir.join("socket"));
        assert!(send(&socket, &mut unpack).is_err());

        for path in ["../escaped", "a/../../escaped", "/escaped", "a//b"] {
            let tree = vec![
                (entry("", ListedKind::Directory, 0o755, 0, ""), vec![]),
                (entry(path, ListedKind::File, 0o644, 0, ""), vec![]),
            ];
            let mut unpack = Unpack::new(dir.join("out"));
            assert!(send(&tree, &mut unpack).is_err(), "{path}");
            fs::remove_dir_all(dir.join("out")).unwrap();
        }
        assert!(!dir.join("escaped").exists());

        fs::write(dir.join("taken"), b"mine").unwrap();
        let file = vec![(entry("", ListedKind::File, 0o644, 2, ""), b"hi".to_vec())];
        let mut unpack = Unpack::new(dir.join("taken"));
        assert!(send(&file, &mut unpack).is_err());
        assert_eq!(fs::read(dir.join("taken")).unwrap(), b"mine");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_under_a_symlink_is_refused() {
        // A tree that sends a symlink to a directory outside the root, then
        // a file "under" the link: a VM the build took over could write
        // anywhere on this machine that way. The file is refused, and
        // nothing lands where the link points. The same for a file under a
        // file, and for a file under a link to a directory inside the root.
        let dir = scratch("symlink");
        let elsewhere = dir.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        let escaping = vec![
            (entry("", ListedKind::Directory, 0o755, 0, ""), vec![]),
            (
                entry(
                    "link",
                    ListedKind::Symlink,
                    0o777,
                    0,
                    elsewhere.to_str().unwrap(),
                ),
                vec![],
            ),
            (
                entry("link/file", ListedKind::File, 0o644, 2, ""),
                b"hi".to_vec(),
            ),
        ];
        let mut unpack = Unpack::new(dir.join("out"));
        let err = send(&escaping, &mut unpack).unwrap_err();
        assert!(err.to_string().contains("link/file"), "{err}");
        assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
        fs::remove_dir_all(dir.join("out")).unwrap();

        let under_file = vec![
            (entry("", ListedKind::Directory, 0o755, 0, ""), vec![]),
            (entry("a", ListedKind::File, 0o644, 0, ""), vec![]),
            (entry("a/b", ListedKind::File, 0o644, 0, ""), vec![]),
        ];
        let mut unpack = Unpack::new(dir.join("out"));
        assert!(send(&under_file, &mut unpack).is_err());
        fs::remove_dir_all(dir.join("out")).unwrap();

        let inside = vec![
            (entry("", ListedKind::Directory, 0o755, 0, ""), vec![]),
            (entry("real", ListedKind::Directory, 0o755, 0, ""), vec![]),
            (entry("link", ListedKind::Symlink, 0o777, 0, "real"), vec![]),
            (entry("link/file", ListedKind::File, 0o644, 0, ""), vec![]),
        ];
        let mut unpack = Unpack::new(dir.join("out"));
        assert!(send(&inside, &mut unpack).is_err());
        assert_eq!(fs::read_dir(dir.join("out/real")).unwrap().count(), 0);
        fs::remove_dir_all(&dir).unwrap();
    }
}
