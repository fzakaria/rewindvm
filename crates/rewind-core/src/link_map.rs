//! The shared libraries a process had loaded, from the dynamic loader's
//! own list in the process's memory: the SVR4 link maps gdb and debuggers
//! read, which glibc and musl both keep. The main program's dynamic
//! section holds a DT_DEBUG entry the loader points at its struct r_debug,
//! whose r_map starts a list of struct link_map, one per loaded object,
//! the main program first.
//!
//! gdb told these, as gdbserver tells it, loads each library as a shared
//! library rather than a file of symbols, and knows which link map each
//! one is: what it needs to find a library's thread-local variables.

use std::path::Path;

use anyhow::{Result, bail};

/// Reads the process's memory at an address into a buffer.
pub type Read<'a> = &'a dyn Fn(u64, &mut [u8]) -> Result<()>;

/// A dynamic section's entries: a tag and a value, each 8 bytes, ending
/// at the DT_NULL tag. DT_DEBUG's value is the struct r_debug.
const DYN_LEN: u64 = 16;
const DT_NULL: u64 = 0;
const DT_DEBUG: u64 = 21;

/// The most entries a dynamic section is read for before it is taken to
/// be something else.
const MAX_DYN: u64 = 1024;

/// In struct r_debug: the first link map.
const R_MAP: u64 = 8;

/// In struct link_map, the part the SVR4 ABI fixes: the object's load
/// offset, its path, its dynamic section and the next link map.
const L_ADDR: u64 = 0;
const L_NAME: u64 = 8;
const L_LD: u64 = 16;
const L_NEXT: u64 = 24;

/// The most link maps a list is read for before it is taken to loop.
const MAX_ENTRIES: usize = 4096;

/// The longest path a link map's name is read for.
const MAX_NAME: usize = 4096;

/// How many bytes of a name one read takes, short of a page's end.
const NAME_CHUNK: u64 = 64;
const PAGE_SIZE: u64 = 4096;

/// One loaded object: its link map's address, its load offset, its
/// dynamic section and its path as the loader opened it, empty for the
/// main program.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkMap {
    pub lm: u64,
    pub l_addr: u64,
    pub l_ld: u64,
    pub name: String,
}

/// The struct r_debug a dynamic section at one of `dynamics` points at,
/// the first that does, or None when none does: a static program, or a
/// process whose loader has not started yet. A dynamic section that does
/// not read is passed over.
pub fn find_r_debug(read: Read, dynamics: &[u64]) -> Option<u64> {
    dynamics
        .iter()
        .find_map(|&dynamic| dt_debug(read, dynamic).ok().flatten())
}

/// DT_DEBUG's value in the dynamic section at `dynamic`, unless it is 0
/// or the section has none.
fn dt_debug(read: Read, dynamic: u64) -> Result<Option<u64>> {
    for i in 0..MAX_DYN {
        let entry = dynamic + i * DYN_LEN;
        let tag = word(read, entry)?;
        if tag == DT_NULL {
            return Ok(None);
        }
        if tag != DT_DEBUG {
            continue;
        }
        let value = word(read, entry + 8)?;
        return Ok((value != 0).then_some(value));
    }
    Ok(None)
}

/// The link maps the loader's list at `r_debug` holds, in its order.
pub fn entries(read: Read, r_debug: u64) -> Result<Vec<LinkMap>> {
    let mut maps = Vec::new();
    let mut lm = word(read, r_debug + R_MAP)?;
    while lm != 0 {
        if maps.len() == MAX_ENTRIES {
            bail!("the loader's list at {r_debug:#x} runs past {MAX_ENTRIES} link maps");
        }
        maps.push(LinkMap {
            lm,
            l_addr: word(read, lm + L_ADDR)?,
            l_ld: word(read, lm + L_LD)?,
            name: string(read, word(read, lm + L_NAME)?)?,
        });
        lm = word(read, lm + L_NEXT)?;
    }
    Ok(maps)
}

/// The 8-byte little-endian word at `at`.
fn word(read: Read, at: u64) -> Result<u64> {
    let mut bytes = [0u8; 8];
    read(at, &mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

/// The NUL-terminated string at `at`, read a chunk at a time without
/// crossing into a page past the string's end, which may not be mapped.
/// A null pointer is the empty string.
fn string(read: Read, at: u64) -> Result<String> {
    if at == 0 {
        return Ok(String::new());
    }
    let mut bytes = Vec::new();
    while bytes.len() < MAX_NAME {
        let from = at + bytes.len() as u64;
        let to_page_end = PAGE_SIZE - from % PAGE_SIZE;
        let mut chunk = vec![0u8; NAME_CHUNK.min(to_page_end) as usize];
        read(from, &mut chunk)?;
        if let Some(end) = chunk.iter().position(|b| *b == 0) {
            bytes.extend_from_slice(&chunk[..end]);
            return Ok(String::from_utf8_lossy(&bytes).into_owned());
        }
        bytes.extend_from_slice(&chunk);
    }
    bail!("a link map's name at {at:#x} runs past {MAX_NAME} bytes")
}

/// gdb's SVR4 library list: the main program's link map at `main`, and
/// each library's link map with the file gdb reads for it.
pub fn svr4_xml(main: u64, libraries: &[(&LinkMap, &Path)]) -> String {
    let mut xml = format!("<library-list-svr4 version=\"1.0\" main-lm=\"{main:#x}\">");
    for (map, path) in libraries {
        xml.push_str(&format!(
            "<library name=\"{}\" lm=\"{:#x}\" l_addr=\"{:#x}\" l_ld=\"{:#x}\" lmid=\"0x0\"/>",
            escape(&path.to_string_lossy()),
            map.lm,
            map.l_addr,
            map.l_ld
        ));
    }
    xml.push_str("</library-list-svr4>");
    xml
}

/// `text` as an XML attribute's value.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    // The list read out of made-up memory: a dynamic section whose
    // DT_DEBUG points at an r_debug, and link maps chained from it.
    use super::*;
    use std::cell::RefCell;
    use std::path::PathBuf;

    /// Memory of a few written regions, mapped a page at a time as real
    /// memory is: a byte no region holds reads 0 on a page some region
    /// touches, and fails on any other.
    #[derive(Default)]
    struct Fake(RefCell<Vec<(u64, Vec<u8>)>>);

    impl Fake {
        fn put(&self, at: u64, bytes: &[u8]) {
            self.0.borrow_mut().push((at, bytes.to_vec()));
        }

        fn put_u64(&self, at: u64, value: u64) {
            self.put(at, &value.to_le_bytes());
        }

        fn read(&self, at: u64, buf: &mut [u8]) -> Result<()> {
            for (i, byte) in buf.iter_mut().enumerate() {
                let address = at + i as u64;
                let region = self.0.borrow().iter().rev().find_map(|(start, bytes)| {
                    let offset = address.checked_sub(*start)? as usize;
                    bytes.get(offset).copied()
                });
                let page = address / PAGE_SIZE;
                let mapped = self.0.borrow().iter().any(|(start, bytes)| {
                    let end = start + bytes.len() as u64;
                    (start / PAGE_SIZE..=end.saturating_sub(1) / PAGE_SIZE).contains(&page)
                });
                match (region, mapped) {
                    (Some(b), _) => *byte = b,
                    (None, true) => *byte = 0,
                    (None, false) => bail!("no memory at {address:#x}"),
                }
            }
            Ok(())
        }
    }

    /// Where the made-up r_debug and link maps are.
    const DYNAMIC: u64 = 0x5000_3d68;
    const R_DEBUG: u64 = 0x7f00_0000_0100;
    const MAPS: [u64; 3] = [0x7f00_0000_1000, 0x7f00_0000_2000, 0x7f00_0000_3000];

    /// A process with a dynamic section of three entries, DT_DEBUG the
    /// second, and a main program, libc at 0x7f10_0000_0000 and the
    /// loader in its list.
    fn process() -> Fake {
        let fake = Fake::default();
        fake.put_u64(DYNAMIC, 1);
        fake.put_u64(DYNAMIC + 8, 0x10);
        fake.put_u64(DYNAMIC + DYN_LEN, DT_DEBUG);
        fake.put_u64(DYNAMIC + DYN_LEN + 8, R_DEBUG);
        fake.put_u64(DYNAMIC + 2 * DYN_LEN, DT_NULL);
        fake.put_u64(DYNAMIC + 2 * DYN_LEN + 8, 0);
        fake.put_u64(R_DEBUG + R_MAP, MAPS[0]);

        let names = ["", "/nix/store/x-glibc/lib/libc.so.6", "/lib/ld.so"];
        let offsets = [0x5000_0000, 0x7f10_0000_0000, 0x7f20_0000_0000];
        for (i, lm) in MAPS.iter().enumerate() {
            let name_at = lm + 0x100;
            fake.put_u64(lm + L_ADDR, offsets[i]);
            fake.put_u64(lm + L_NAME, name_at);
            fake.put_u64(lm + L_LD, offsets[i] + 0x3d68);
            fake.put_u64(lm + L_NEXT, MAPS.get(i + 1).copied().unwrap_or(0));
            let mut name = names[i].as_bytes().to_vec();
            name.push(0);
            fake.put(name_at, &name);
        }
        fake
    }

    /// DT_DEBUG names the r_debug; a dynamic section that does not read,
    /// listed first, is passed over, and one without DT_DEBUG finds none.
    #[test]
    fn the_r_debug_is_found_through_dt_debug() {
        let fake = process();
        let read = |at: u64, buf: &mut [u8]| fake.read(at, buf);
        assert_eq!(find_r_debug(&read, &[0x1234, DYNAMIC]), Some(R_DEBUG));

        let bare = Fake::default();
        bare.put_u64(DYNAMIC, DT_NULL);
        bare.put_u64(DYNAMIC + 8, 0);
        let read = |at: u64, buf: &mut [u8]| bare.read(at, buf);
        assert_eq!(find_r_debug(&read, &[DYNAMIC]), None);
    }

    /// The list is every link map in order, with its offset, dynamic
    /// section and name, the main program's empty.
    #[test]
    fn the_list_is_read_in_order() {
        let fake = process();
        let read = |at: u64, buf: &mut [u8]| fake.read(at, buf);
        let maps = entries(&read, R_DEBUG).unwrap();
        let names: Vec<&str> = maps.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(
            names,
            ["", "/nix/store/x-glibc/lib/libc.so.6", "/lib/ld.so"]
        );
        assert_eq!(maps[1].lm, MAPS[1]);
        assert_eq!(maps[1].l_addr, 0x7f10_0000_0000);
        assert_eq!(maps[1].l_ld, 0x7f10_0000_3d68);
    }

    /// A list that loops back on itself is refused rather than read
    /// forever.
    #[test]
    fn a_looping_list_is_refused() {
        let fake = process();
        fake.put_u64(MAPS[2] + L_NEXT, MAPS[0]);
        let read = |at: u64, buf: &mut [u8]| fake.read(at, buf);
        assert!(entries(&read, R_DEBUG).is_err());
    }

    /// The XML names the main program's link map and each library's, with
    /// the path gdb reads it from, escaped.
    #[test]
    fn the_xml_names_each_library() {
        let libc = LinkMap {
            lm: 0x1000,
            l_addr: 0x7f00,
            l_ld: 0x7f80,
            name: "/lib/libc.so.6".into(),
        };
        let here = PathBuf::from("/tmp/a&b/libc.so.6");
        let xml = svr4_xml(0x500, &[(&libc, &here)]);
        assert_eq!(
            xml,
            "<library-list-svr4 version=\"1.0\" main-lm=\"0x500\">\
             <library name=\"/tmp/a&amp;b/libc.so.6\" lm=\"0x1000\" l_addr=\"0x7f00\" \
             l_ld=\"0x7f80\" lmid=\"0x0\"/>\
             </library-list-svr4>"
        );
    }
}
