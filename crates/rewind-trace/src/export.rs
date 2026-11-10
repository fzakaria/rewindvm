//! A `.rwd` export: a zstd-compressed tar of a run's directory. Every
//! export carries the manifest and trace, the run's summary in plain text,
//! and the bookmarks the desktop app made; a replayable one also carries the keyframes, the pages they
//! name and the VM's inputs. `rewind export` writes them, and `rewind
//! import` and the app read them through [`walk`].

use std::fmt;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use crate::manifest::{BOOKMARKS, KEYFRAMES_DIR, MANIFEST, TRACE};
use crate::summary::SUMMARY;

/// The file extension of an export.
pub const EXTENSION: &str = "rwd";

/// The zstd level an export is written at: its default, since an export is
/// written once and read rarely.
pub const COMPRESSION_LEVEL: i32 = 3;

/// The pages the keyframes name, each under its hash in hex.
pub const PAGES_DIR: &str = "pages";

/// The VM's inputs: its kernel, its initramfs and its input image.
pub const INPUTS_DIR: &str = "inputs";
pub const KERNEL: &str = "inputs/kernel";
pub const INITRD: &str = "inputs/initrd";
pub const IMAGE: &str = "inputs/image.erofs";

/// The directories only a replayable export has.
pub const REPLAY_DIRS: [&str; 3] = [KEYFRAMES_DIR, PAGES_DIR, INPUTS_DIR];

/// The most bytes of bookmarks an export may carry: notes a person typed,
/// far below this, so a larger file is not a run's.
pub const MAX_BOOKMARKS: u64 = 1 << 20;

/// The most bytes of summary a reader takes from an export: a summary is
/// a few dozen lines of at most a few kilobytes each, far below this.
pub const MAX_SUMMARY: u64 = 1 << 20;

/// What an export carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Contents {
    /// The manifest and trace: enough to view and scrub the run.
    View,
    /// Also keyframes, pages and inputs: enough to replay the run on
    /// another machine with a compatible CPU.
    Replayable,
}

/// What a member of an export is, by its path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Member {
    Manifest,
    Trace,
    /// The run in plain text, as `rewind summary` prints it.
    Summary,
    /// The desktop app's bookmarks.
    Bookmarks,
    /// A keyframe, or the mark that the run took keyframes as it executed.
    Keyframe,
    /// A page's contents, under its hash.
    Page,
    /// The kernel, the initramfs or the image.
    Input,
    /// A member no export writes.
    Other,
}

impl Member {
    /// What the member at `path` inside an export is.
    pub fn of(path: &Path) -> Member {
        if path == Path::new(MANIFEST) {
            return Member::Manifest;
        }
        if path == Path::new(TRACE) {
            return Member::Trace;
        }
        if path == Path::new(SUMMARY) {
            return Member::Summary;
        }
        if path == Path::new(BOOKMARKS) {
            return Member::Bookmarks;
        }
        if path.starts_with(KEYFRAMES_DIR) {
            return Member::Keyframe;
        }
        if path.starts_with(PAGES_DIR) {
            return Member::Page;
        }
        if path.starts_with(INPUTS_DIR) {
            return Member::Input;
        }
        Member::Other
    }

    /// Whether only a replayable export carries the member.
    pub fn replay_only(self) -> bool {
        matches!(self, Member::Keyframe | Member::Page | Member::Input)
    }
}

/// A block of a tar archive. An archive ends in two blocks of zeros.
pub const BLOCK: usize = 512;

/// Why an export does not read.
#[derive(Debug, thiserror::Error)]
pub enum UnpackError<E: fmt::Debug + fmt::Display> {
    #[error("not a tar archive: {0}")]
    NotTar(io::Error),
    #[error("the archive holds an unsafe path {}", .0.display())]
    UnsafePath(PathBuf),
    #[error("the archive holds {} as a {kind}, which no export does", .path.display())]
    NotAFile { path: PathBuf, kind: String },
    /// The bytes end before the archive's closing blocks of zeros, as a
    /// download that stopped part way ends.
    #[error("the archive is cut short: {0}")]
    CutShort(io::Error),
    #[error("the archive does not end as a tar archive does")]
    OddEnd,
    #[error("reading the archive: {0}")]
    Io(#[from] io::Error),
    /// What the reader of a member refused.
    #[error("{0}")]
    Refused(E),
}

/// What a taker of members tells [`walk`] after each one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    /// Hand over the next member.
    Continue,
    /// End the walk here: the taker has what it wants. Nothing after the
    /// member is read, so bytes that end after it are not cut short.
    Stop,
}

/// Walks the tar archive of an export, `tar` being the archive already
/// decompressed, and hands each regular file to `take` with its path, what
/// it is and its size; `take` says whether to go on. A member whose path
/// is not plain names, or that is neither a regular file nor a directory,
/// refuses the export: a link could point anywhere, and an entry under it
/// would be written there. An archive walked to its end must end in its
/// two blocks of zeros, or it is refused as cut short. Returns what the
/// export carries: replayable when any member only a replayable export
/// has is in it.
pub fn walk<R: Read, E: fmt::Debug + fmt::Display>(
    tar: R,
    mut take: impl FnMut(&Path, Member, u64, &mut dyn Read) -> Result<Flow, E>,
) -> Result<Contents, UnpackError<E>> {
    let mut archive = tar::Archive::new(tar);
    let mut contents = Contents::View;
    let mut entries = archive.entries().map_err(UnpackError::NotTar)?;
    let flow = loop {
        let Some(entry) = entries.next() else {
            break Flow::Continue;
        };
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        if path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(UnpackError::UnsafePath(path));
        }
        let kind = entry.header().entry_type();
        if !matches!(kind, tar::EntryType::Regular | tar::EntryType::Directory) {
            return Err(UnpackError::NotAFile {
                path,
                kind: format!("{kind:?}"),
            });
        }
        let member = Member::of(&path);
        if member.replay_only() {
            contents = Contents::Replayable;
        }

        // A directory only holds files, which come as entries of their own.
        if kind == tar::EntryType::Directory {
            continue;
        }
        let size = entry.size();
        let flow = take(&path, member, size, &mut entry).map_err(UnpackError::Refused)?;
        if flow == Flow::Stop {
            break Flow::Stop;
        }
    };
    if flow == Flow::Stop {
        return Ok(contents);
    }

    // The tar reader ends the walk at the first block of zeros, and the
    // same way where the bytes end on a member's edge; the second block
    // of zeros tells an archive's end from bytes that end too soon.
    let mut block = [0u8; BLOCK];
    archive
        .into_inner()
        .read_exact(&mut block)
        .map_err(UnpackError::CutShort)?;
    if block.iter().any(|byte| *byte != 0) {
        return Err(UnpackError::OddEnd);
    }
    Ok(contents)
}

#[cfg(test)]
mod tests {
    // Walks over archives built in memory the way the engine builds them:
    // which members come out, what they are, and what refuses the archive.
    use super::*;

    /// A tar of regular files, each `(name, bytes)`.
    fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        for (name, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, name, *data).unwrap();
        }
        tar.into_inner().unwrap()
    }

    /// Every regular file of `tar`, as walk hands them out, and what the
    /// archive carries.
    fn walked(tar: &[u8]) -> (Vec<(String, Member, Vec<u8>)>, Contents) {
        let mut seen = Vec::new();
        let contents = walk(tar, |path, member, size, reader| -> Result<Flow, String> {
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes.len() as u64, size);
            seen.push((path.display().to_string(), member, bytes));
            Ok(Flow::Continue)
        })
        .unwrap();
        (seen, contents)
    }

    #[test]
    fn members_come_out_in_order_each_named_for_what_it_is() {
        // A view export's four members, then one of each kind a
        // replayable export adds and one no export writes: each is handed
        // over with its bytes, and the replay members make the export
        // replayable.
        let view = archive(&[
            ("trace.bin", b"trace"),
            ("manifest.json", b"{}"),
            ("summary.txt", b"run"),
            ("bookmarks.json", b"[]"),
        ]);
        let (seen, contents) = walked(&view);
        assert_eq!(contents, Contents::View);
        assert_eq!(
            seen,
            vec![
                ("trace.bin".to_string(), Member::Trace, b"trace".to_vec()),
                (
                    "manifest.json".to_string(),
                    Member::Manifest,
                    b"{}".to_vec()
                ),
                ("summary.txt".to_string(), Member::Summary, b"run".to_vec()),
                (
                    "bookmarks.json".to_string(),
                    Member::Bookmarks,
                    b"[]".to_vec()
                ),
            ]
        );

        let replayable = archive(&[
            ("manifest.json", b"{}"),
            ("trace.bin", b"trace"),
            ("keyframes/0000000000000100.kf", b"kf"),
            ("keyframes/taken", b""),
            ("pages/00ff", &[0u8; 16]),
            ("inputs/kernel", b"kernel"),
            ("notes.txt", b"?"),
        ]);
        let (seen, contents) = walked(&replayable);
        assert_eq!(contents, Contents::Replayable);
        let kinds: Vec<Member> = seen.iter().map(|(_, m, _)| *m).collect();
        assert_eq!(
            kinds,
            vec![
                Member::Manifest,
                Member::Trace,
                Member::Keyframe,
                Member::Keyframe,
                Member::Page,
                Member::Input,
                Member::Other,
            ]
        );
    }

    #[test]
    fn a_directory_entry_is_passed_over() {
        // An archive that lists the keyframes directory itself hands out
        // only the file under it, and the directory still marks the
        // export replayable.
        let mut tar = tar::Builder::new(Vec::new());
        let mut dir = tar::Header::new_gnu();
        dir.set_entry_type(tar::EntryType::Directory);
        dir.set_size(0);
        dir.set_mode(0o755);
        dir.set_cksum();
        tar.append_data(&mut dir, "keyframes", &b""[..]).unwrap();
        let mut file = tar::Header::new_gnu();
        file.set_size(2);
        file.set_mode(0o644);
        file.set_cksum();
        tar.append_data(&mut file, "keyframes/0000000000000100.kf", &b"kf"[..])
            .unwrap();
        let (seen, contents) = walked(&tar.into_inner().unwrap());
        assert_eq!(contents, Contents::Replayable);
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, "keyframes/0000000000000100.kf");
    }

    #[test]
    fn an_archive_cut_short_is_refused() {
        // The tar reader ends the walk at the first block of zeros, and
        // the same way where the bytes end on a member's edge. An archive
        // whole to its last byte walks; the same archive cut at the edge
        // of its last member, or after the first of its two closing
        // blocks of zeros, is refused as cut short, and one whose second
        // closing block is not zeros is refused as not ending as a tar
        // archive does.
        let whole = archive(&[("manifest.json", b"{}"), ("trace.bin", b"trace")]);
        assert_eq!(walked(&whole).1, Contents::View);

        let closing = 2 * BLOCK;
        let at_member_edge = &whole[..whole.len() - closing];
        let after_first_zeros = &whole[..whole.len() - BLOCK];
        for cut in [at_member_edge, after_first_zeros] {
            let err = walk(cut, |_, _, _, _| -> Result<Flow, String> {
                Ok(Flow::Continue)
            })
            .unwrap_err();
            assert!(matches!(err, UnpackError::CutShort(_)), "{err}");
        }

        let mut odd_end = whole.clone();
        let last = odd_end.len() - 1;
        odd_end[last] = 1;
        let err = walk(&odd_end[..], |_, _, _, _| -> Result<Flow, String> {
            Ok(Flow::Continue)
        })
        .unwrap_err();
        assert!(matches!(err, UnpackError::OddEnd), "{err}");
    }

    #[test]
    fn a_taker_that_stops_ends_the_walk_where_it_stops() {
        // A view taker stops at the first member only a replay needs: the
        // members after it are never handed over, and the bytes behind it
        // may be missing, as they are from the start of a download. The
        // export is replayable all the same.
        let replayable = archive(&[
            ("manifest.json", b"{}"),
            ("trace.bin", b"trace"),
            ("inputs/kernel", &[7u8; 4 * BLOCK]),
            ("pages/00ff", &[0u8; 16]),
        ]);
        let kernel_at = 4 * BLOCK;
        let start = &replayable[..kernel_at + BLOCK + BLOCK];
        let mut seen = Vec::new();
        let contents = walk(start, |_, member, _, _| -> Result<Flow, String> {
            seen.push(member);
            if member.replay_only() {
                return Ok(Flow::Stop);
            }
            Ok(Flow::Continue)
        })
        .unwrap();
        assert_eq!(contents, Contents::Replayable);
        assert_eq!(seen, vec![Member::Manifest, Member::Trace, Member::Input]);
    }

    #[test]
    fn a_climbing_path_a_link_and_a_refused_member_stop_the_walk() {
        // A path with .., a symbolic link, and a member the taker refuses
        // each end the walk with an error that says which; bytes that are
        // no tar at all are refused as such.
        let climbing = {
            let mut tar = tar::Builder::new(Vec::new());
            let mut header = tar::Header::new_gnu();
            header.as_gnu_mut().unwrap().name[..7].copy_from_slice(b"../evil");
            header.set_size(1);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append(&header, &b"x"[..]).unwrap();
            tar.into_inner().unwrap()
        };
        let err = walk(&climbing[..], |_, _, _, _| -> Result<Flow, String> {
            Ok(Flow::Continue)
        })
        .unwrap_err();
        assert!(matches!(err, UnpackError::UnsafePath(_)), "{err}");
        assert!(err.to_string().contains("../evil"), "{err}");

        let linked = {
            let mut tar = tar::Builder::new(Vec::new());
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            tar.append_link(&mut header, "trace.bin", "/etc/passwd")
                .unwrap();
            tar.into_inner().unwrap()
        };
        let err = walk(&linked[..], |_, _, _, _| -> Result<Flow, String> {
            Ok(Flow::Continue)
        })
        .unwrap_err();
        assert!(matches!(err, UnpackError::NotAFile { .. }), "{err}");
        assert!(err.to_string().contains("trace.bin"), "{err}");

        let refused = walk(
            &archive(&[("manifest.json", b"{}"), ("trace.bin", b"t")])[..],
            |path, member, _, _| {
                if member == Member::Trace {
                    return Err(format!("{} is too large", path.display()));
                }
                Ok(Flow::Continue)
            },
        )
        .unwrap_err();
        assert_eq!(refused.to_string(), "trace.bin is too large");

        let err = walk(&b"plain text"[..], |_, _, _, _| -> Result<Flow, String> {
            Ok(Flow::Continue)
        })
        .unwrap_err();
        assert!(
            matches!(err, UnpackError::NotTar(_) | UnpackError::Io(_)),
            "{err}"
        );
    }
}
