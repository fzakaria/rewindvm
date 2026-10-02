//! The memory map of the process that was running at a step, read by an
//! inspection (`crate::inspect::running`), turned into what gdb needs to
//! show that process's code by name: each program and library it had
//! mapped, with the offset it was loaded at.
//!
//! A build runs programs from the store paths it was given, and those are
//! on this machine too, at the same paths, so gdb reads their symbols here
//! and fetches their DWARF by build ID. The inspection sends the files
//! only the VM has, such as a program the build itself compiled, and they
//! are written to a directory for gdb.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use rewind_init::{SECTION_MAPS, SECTION_PID, sections};

/// Where store paths start in a path as the VM's root sees it. A job runs
/// in a root of its own, so its files show up under that root's path.
const NIX_STORE: &str = "/nix/store/";

/// What the kernel appends to the path of a file deleted since it was
/// mapped.
const DELETED: &str = " (deleted)";

/// Pages are 4 KiB; a mapping starts on a page boundary.
const PAGE_MASK: u64 = !0xfff;

/// The process that was running and what it had mapped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Running {
    pub pid: u32,
    pub mappings: Vec<Mapping>,
    /// The mapped files only the VM has, by their paths there, with their
    /// bytes.
    pub sent: Vec<(String, Vec<u8>)>,
}

/// One line of /proc/<pid>/maps that maps a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mapping {
    pub start: u64,
    /// Where in the file the mapping starts.
    pub offset: u64,
    pub path: String,
}

/// A file for gdb's `add-symbol-file`, with the offset its addresses are
/// moved by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolFile {
    pub path: PathBuf,
    pub offset: u64,
}

impl Running {
    /// The answer of a `running` inspection, in sections: the pid, the
    /// map, then the files only the VM has. Lines of the map that map no
    /// file, such as the heap, the stack and the vDSO, are left out.
    pub fn parse(answer: &[u8]) -> Option<Running> {
        let mut parts = sections(answer)?.into_iter();
        let (name, pid) = parts.next()?;
        if name != SECTION_PID {
            return None;
        }
        let pid = std::str::from_utf8(pid).ok()?.parse().ok()?;
        let (name, maps) = parts.next()?;
        if name != SECTION_MAPS {
            return None;
        }
        let mappings = String::from_utf8_lossy(maps)
            .lines()
            .filter_map(parse_line)
            .collect();
        let sent = parts.map(|(path, bytes)| (path, bytes.to_vec())).collect();
        Some(Running {
            pid,
            mappings,
            sent,
        })
    }

    /// Each file the process had mapped, by its path in the VM, with the
    /// address its start is mapped at, in the order they first appear in
    /// the map.
    pub fn bases(&self) -> Vec<(&str, u64)> {
        let mut files: Vec<(&str, u64)> = Vec::new();
        for m in &self.mappings {
            if m.offset != 0 {
                continue;
            }
            match files.iter_mut().find(|(p, _)| *p == m.path) {
                Some((_, base)) => *base = (*base).min(m.start),
                None => files.push((&m.path, m.start)),
            }
        }
        files
    }

    /// Where a file the VM mapped at `path` is on this machine: under
    /// `dir` when the VM sent it, else at its store path when this machine
    /// has that.
    fn local(&self, path: &str, dir: &Path) -> Option<PathBuf> {
        if self.sent.iter().any(|(p, _)| p == path) {
            return Some(dir.join(path.trim_start_matches('/')));
        }
        let at = path.find(NIX_STORE)?;
        Some(PathBuf::from(&path[at..])).filter(|p| p.exists())
    }

    /// Writes the files the VM sent under `dir`, at their paths in the
    /// VM, and returns every file gdb can read with its load offset: what
    /// gdb needs to name the process's code.
    pub fn symbol_files(&self, dir: &Path) -> std::io::Result<Vec<SymbolFile>> {
        for (path, bytes) in &self.sent {
            let to = dir.join(path.trim_start_matches('/'));
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&to, bytes)?;
        }

        Ok(self
            .bases()
            .into_iter()
            .filter_map(|(path, base)| {
                let path = self.local(path, dir)?;
                let mut file = File::open(&path).ok()?;
                let offset = load_offset(&mut file, base)?;
                Some(SymbolFile { path, offset })
            })
            .collect())
    }

    /// The programs and libraries gdb has no copy of: store paths that
    /// were not sent and are not on this machine. A file outside the store
    /// that the VM did not send is not a program, such as a database
    /// mapped into memory, so it is not missed.
    pub fn missing(&self, dir: &Path) -> Vec<&str> {
        self.bases()
            .into_iter()
            .map(|(path, _)| path)
            .filter(|path| path.contains(NIX_STORE))
            .filter(|path| self.local(path, dir).is_none_or(|p| !p.exists()))
            .collect()
    }
}

/// A line of /proc/<pid>/maps: `start-end perms offset dev inode path`,
/// with the path padded out to a column. None for a line with no file.
fn parse_line(line: &str) -> Option<Mapping> {
    let mut rest = line;
    let mut fields = [""; 5];
    for field in &mut fields {
        rest = rest.trim_start();
        let end = rest.find(' ')?;
        *field = &rest[..end];
        rest = &rest[end..];
    }
    let path = rest.trim_start();
    let path = path.strip_suffix(DELETED).unwrap_or(path);
    if !path.starts_with('/') {
        return None;
    }

    let start = u64::from_str_radix(fields[0].split('-').next()?, 16).ok()?;
    let offset = u64::from_str_radix(fields[2], 16).ok()?;
    Some(Mapping {
        start,
        offset,
        path: path.to_string(),
    })
}

/// ELF's magic, the class byte for 64 bits, and the program header types
/// of a loadable segment and a note segment.
const ELF_MAGIC: &[u8; 4] = b"\x7fELF";
const ELF_CLASS_64: u8 = 2;
const PT_LOAD: u32 = 1;
const PT_NOTE: u32 = 4;

/// A note's owner and type when it is a build ID, and the alignment of
/// its name and description.
const NOTE_GNU: &[u8] = b"GNU\0";
const NT_GNU_BUILD_ID: u32 = 3;
const NOTE_ALIGN: usize = 4;
const NOTE_HEADER_LEN: usize = 12;

/// Offsets into a 64-bit ELF header and program header.
const E_PHOFF: usize = 32;
const E_PHENTSIZE: usize = 54;
const E_PHNUM: usize = 56;
const ELF_HEADER_LEN: usize = 64;
const P_OFFSET: usize = 8;
const P_VADDR: usize = 16;
const P_FILESZ: usize = 32;
const PHDR_LEN: usize = 56;

/// How far an ELF file's addresses moved when it was loaded with its
/// start at `base`: the first loadable segment says which address the
/// file's start was linked at. 0 for a program that is not position
/// independent. None when the file is not a 64-bit ELF file.
pub fn load_offset(file: &mut (impl Read + Seek), base: u64) -> Option<u64> {
    let mut header = [0u8; ELF_HEADER_LEN];
    file.read_exact(&mut header).ok()?;
    if &header[..4] != ELF_MAGIC || header[4] != ELF_CLASS_64 {
        return None;
    }
    let u16_at = |b: &[u8], at: usize| u16::from_le_bytes([b[at], b[at + 1]]) as usize;
    let u64_at = |b: &[u8], at: usize| u64::from_le_bytes(b[at..at + 8].try_into().unwrap());
    let phoff = u64_at(&header, E_PHOFF);
    let phentsize = u16_at(&header, E_PHENTSIZE);
    let phnum = u16_at(&header, E_PHNUM);
    if phentsize < PHDR_LEN {
        return None;
    }

    // Program headers list loadable segments in address order, so the
    // first is the one the file's start belongs to.
    let mut phdrs = vec![0u8; phentsize * phnum];
    file.seek(SeekFrom::Start(phoff)).ok()?;
    file.read_exact(&mut phdrs).ok()?;
    let first = phdrs
        .chunks_exact(phentsize)
        .find(|p| u32::from_le_bytes(p[..4].try_into().unwrap()) == PT_LOAD)?;
    let linked = (u64_at(first, P_VADDR) - u64_at(first, P_OFFSET)) & PAGE_MASK;
    Some(base.wrapping_sub(linked))
}

/// An ELF file's GNU build ID, in hex, from its note segments: the name
/// gdb and debuginfod find its separate DWARF by. None when the file is
/// not a 64-bit ELF file or has no build ID.
pub fn build_id(file: &mut (impl Read + Seek)) -> Option<String> {
    let mut header = [0u8; ELF_HEADER_LEN];
    file.read_exact(&mut header).ok()?;
    if &header[..4] != ELF_MAGIC || header[4] != ELF_CLASS_64 {
        return None;
    }
    let u16_at = |b: &[u8], at: usize| u16::from_le_bytes([b[at], b[at + 1]]) as usize;
    let u32_at = |b: &[u8], at: usize| u32::from_le_bytes(b[at..at + 4].try_into().unwrap());
    let u64_at = |b: &[u8], at: usize| u64::from_le_bytes(b[at..at + 8].try_into().unwrap());
    let phentsize = u16_at(&header, E_PHENTSIZE);
    let phnum = u16_at(&header, E_PHNUM);
    if phentsize < PHDR_LEN {
        return None;
    }
    let mut phdrs = vec![0u8; phentsize * phnum];
    file.seek(SeekFrom::Start(u64_at(&header, E_PHOFF))).ok()?;
    file.read_exact(&mut phdrs).ok()?;

    // Each note segment holds notes one after another: sizes and a type,
    // then the name and description, each padded to four bytes.
    let align = |n: usize| n.div_ceil(NOTE_ALIGN) * NOTE_ALIGN;
    for phdr in phdrs.chunks_exact(phentsize) {
        if u32_at(phdr, 0) != PT_NOTE {
            continue;
        }
        let mut notes = vec![0u8; usize::try_from(u64_at(phdr, P_FILESZ)).ok()?];
        file.seek(SeekFrom::Start(u64_at(phdr, P_OFFSET))).ok()?;
        file.read_exact(&mut notes).ok()?;

        let mut at = 0;
        while at + NOTE_HEADER_LEN <= notes.len() {
            let namesz = u32_at(&notes, at) as usize;
            let descsz = u32_at(&notes, at + 4) as usize;
            let kind = u32_at(&notes, at + 8);
            let name = at + NOTE_HEADER_LEN;
            let desc = name + align(namesz);
            if desc + descsz > notes.len() {
                break;
            }
            if kind == NT_GNU_BUILD_ID && &notes[name..name + namesz] == NOTE_GNU {
                let id = &notes[desc..desc + descsz];
                return Some(id.iter().map(|b| format!("{b:02x}")).collect());
            }
            at = desc + align(descsz);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    // Memory maps as the VM reports them, read into store files and load
    // offsets, with the ELF headers made up in memory.
    use super::*;
    use std::io::Cursor;

    const MAPS: &str = "\
5578a000-5578b000 r--p 00000000 00:1a 123      /nix/store/aaa-bash-5.3/bin/bash
5578b000-5578f000 r-xp 00001000 00:1a 123      /nix/store/aaa-bash-5.3/bin/bash
55790000-557b1000 rw-p 00000000 00:00 0        [heap]
7f0000000000-7f0000028000 r--p 00000000 00:1a 77 /nix/store/bbb-glibc-2.44/lib/libc.so.6
7f0000028000-7f00001a0000 r-xp 00028000 00:1a 77 /nix/store/bbb-glibc-2.44/lib/libc.so.6
7f0000300000-7f0000301000 r--p 00000000 00:1b 9  /build/source/test-helper (deleted)
7ffd00000000-7ffd00021000 rw-p 00000000 00:00 0  [stack]
7ffd00100000-7ffd00102000 r-xp 00000000 00:00 0  [vdso]
";

    /// An answer with the map above and `sent` files.
    fn answer(sent: &[(&str, &[u8])]) -> Vec<u8> {
        let mut answer = Vec::new();
        let parts = [(SECTION_PID, &b"4242"[..]), (SECTION_MAPS, MAPS.as_bytes())];
        for (name, body) in parts.iter().chain(sent) {
            answer.extend(rewind_init::section_header(name, body.len()).into_bytes());
            answer.extend(*body);
        }
        answer
    }

    /// A 64-bit ELF header and one program header for a loadable segment
    /// at `vaddr`, from the start of the file.
    fn elf(vaddr: u64) -> Vec<u8> {
        let mut b = vec![0u8; ELF_HEADER_LEN + PHDR_LEN];
        b[..4].copy_from_slice(ELF_MAGIC);
        b[4] = ELF_CLASS_64;
        b[E_PHOFF..E_PHOFF + 8].copy_from_slice(&(ELF_HEADER_LEN as u64).to_le_bytes());
        b[E_PHENTSIZE..E_PHENTSIZE + 2].copy_from_slice(&(PHDR_LEN as u16).to_le_bytes());
        b[E_PHNUM..E_PHNUM + 2].copy_from_slice(&1u16.to_le_bytes());
        let p = ELF_HEADER_LEN;
        b[p..p + 4].copy_from_slice(&PT_LOAD.to_le_bytes());
        b[p + P_VADDR..p + P_VADDR + 8].copy_from_slice(&vaddr.to_le_bytes());
        b
    }

    #[test]
    fn the_answer_gives_the_pid_mapped_files_and_what_was_sent() {
        let running = Running::parse(&answer(&[("/build/source/test-helper", b"x")])).unwrap();
        assert_eq!(running.pid, 4242);
        assert_eq!(running.mappings.len(), 5);
        assert_eq!(running.mappings[4].path, "/build/source/test-helper");
        assert_eq!(running.sent.len(), 1);
    }

    #[test]
    fn each_file_is_based_at_its_first_mapping() {
        let running = Running::parse(&answer(&[])).unwrap();
        assert_eq!(
            running.bases(),
            vec![
                ("/nix/store/aaa-bash-5.3/bin/bash", 0x5578a000),
                ("/nix/store/bbb-glibc-2.44/lib/libc.so.6", 0x7f0000000000),
                ("/build/source/test-helper", 0x7f0000300000),
            ]
        );
    }

    #[test]
    fn sent_files_are_written_where_gdb_finds_them() {
        let dir = std::env::temp_dir().join(format!("rewind-maps-test-{}", std::process::id()));
        let running = Running::parse(&answer(&[("/build/source/test-helper", &elf(0))])).unwrap();
        let files = running.symbol_files(&dir).unwrap();
        let helper = dir.join("build/source/test-helper");
        assert!(files.contains(&SymbolFile {
            path: helper.clone(),
            offset: 0x7f0000300000
        }));
        assert!(!running.missing(&dir).contains(&"/build/source/test-helper"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_store_files_this_machine_lacks_are_missed() {
        // The map's store files are made up, so this machine lacks them;
        // the unsent file outside the store is data, not a program.
        let running = Running::parse(&answer(&[])).unwrap();
        let dir = std::env::temp_dir();
        assert_eq!(
            running.missing(&dir),
            vec![
                "/nix/store/aaa-bash-5.3/bin/bash",
                "/nix/store/bbb-glibc-2.44/lib/libc.so.6"
            ]
        );
    }

    #[test]
    fn a_position_independent_file_moves_by_its_base() {
        let mut file = Cursor::new(elf(0));
        assert_eq!(load_offset(&mut file, 0x7f0000000000), Some(0x7f0000000000));
    }

    #[test]
    fn a_program_linked_where_it_runs_does_not_move() {
        let mut file = Cursor::new(elf(0x400000));
        assert_eq!(load_offset(&mut file, 0x400000), Some(0));
    }

    #[test]
    fn a_file_that_is_not_elf_has_no_offset() {
        let mut file = Cursor::new(b"#!/bin/sh\necho hello\n".repeat(8));
        assert_eq!(load_offset(&mut file, 0x400000), None);
    }

    /// A 64-bit ELF file whose one program header is a note segment
    /// holding a GNU build ID note for `id`, as vmlinux has, read back as
    /// hex; and a file whose note is of another kind, which has none.
    #[test]
    fn a_build_id_is_read_from_the_note_segment() {
        let elf_with_note = |note_type: u32| {
            let mut b = elf(0);
            let p = ELF_HEADER_LEN;
            let note = ELF_HEADER_LEN + PHDR_LEN;
            b[p..p + 4].copy_from_slice(&PT_NOTE.to_le_bytes());
            b[p + P_OFFSET..p + P_OFFSET + 8].copy_from_slice(&(note as u64).to_le_bytes());
            let mut body = Vec::new();
            body.extend(4u32.to_le_bytes());
            body.extend(3u32.to_le_bytes());
            body.extend(note_type.to_le_bytes());
            body.extend(b"GNU\0");
            body.extend([0x15, 0xf1, 0x13]);
            body.push(0);
            b[p + P_FILESZ..p + P_FILESZ + 8].copy_from_slice(&(body.len() as u64).to_le_bytes());
            b.extend(body);
            b
        };

        let mut file = Cursor::new(elf_with_note(NT_GNU_BUILD_ID));
        assert_eq!(build_id(&mut file).as_deref(), Some("15f113"));
        let mut other = Cursor::new(elf_with_note(NT_GNU_BUILD_ID + 1));
        assert_eq!(build_id(&mut other), None);
    }

    #[test]
    fn an_answer_without_a_pid_is_no_answer() {
        assert_eq!(Running::parse(b""), None);
        let maps_only = rewind_init::section_header(SECTION_MAPS, 0);
        assert_eq!(Running::parse(maps_only.as_bytes()), None);
    }
}
