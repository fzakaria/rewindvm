//! The names of the source files gdb's debuginfod client fetched.
//!
//! The client, elfutils' libdebuginfod, keeps a source file it fetched at
//! `<cache>/<build ID>/source-<escaped>`, the cache being under the home
//! directory of whoever ran gdb. `<escaped>` is the name gdb asked for the
//! file by, the name the program's DWARF gives it, as elfutils'
//! `path_escape` writes it: the name's djb2 hash in 8 hex digits, a dash,
//! and the name's last 117 bytes at most, each byte other than a letter, a
//! digit, `.`, `-` or `_` written as `#`.
//!
//! The escaping does not undo on its own: a `#` stands for a `/`, a
//! literal `#` or any other byte, and a long name loses its start. A name
//! is taken back only when it escapes to the file's name exactly, so the
//! hash vouches for it; else the DWARF's own name for the file stands in.

use std::path::Path;

/// What the client's cache names a source file with, before the escaped
/// name.
const SOURCE_PREFIX: &str = "source-";

/// The hex digits of the hash, and the dash after them.
const HASH_DIGITS: usize = 8;
const HASH_SEPARATOR: char = '-';

/// The most bytes of a name the escaped name keeps: elfutils writes at
/// most NAME_MAX / 2 - 1 characters, the hash and its dash among them.
const KEPT_BYTES: usize = 255 / 2 - 1 - (HASH_DIGITS + 1);

/// What the escaping writes for each byte it does not keep.
const ESCAPED: char = '#';

/// The separator of a path's components.
const SEPARATOR: char = '/';

/// The name the program's DWARF gives the source file the debuginfod
/// client fetched to `path`, or None when `path` is no source file in a
/// client's cache. `dwarf` is the name gdb's symbol table gives the file,
/// which may be relative to the program's compilation directory; when no
/// name escapes to the file's name exactly, `dwarf` is the answer, or
/// without it the escaped name with each `#` read as a `/`.
pub(crate) fn source_name(path: &Path, dwarf: Option<&str>) -> Option<String> {
    // A file named for its source, in a directory named for a build ID.
    let file = path.file_name()?.to_str()?;
    let build_id = path.parent()?.file_name()?.to_str()?;
    let is_build_id = !build_id.is_empty() && build_id.bytes().all(|b| b.is_ascii_hexdigit());
    if !is_build_id {
        return None;
    }
    let hashed = file.strip_prefix(SOURCE_PREFIX)?;
    let (hash, tail) = hashed.split_at_checked(HASH_DIGITS)?;
    let tail = tail.strip_prefix(HASH_SEPARATOR)?;
    if !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }

    // The names the file may stand for: the DWARF's, when whole; the
    // escaped name with each `#` read as a `/`; and the DWARF's relative
    // name after the directories the escaped name gives before it, which
    // gdb joined it to.
    let read_back = tail.replace(ESCAPED, &SEPARATOR.to_string());
    let absolute = dwarf.filter(|name| name.starts_with(SEPARATOR));
    let joined = dwarf
        .filter(|name| !name.starts_with(SEPARATOR))
        .and_then(|name| {
            let start = tail.strip_suffix(&escape_bytes(name.as_bytes()))?;
            Some(format!(
                "{}{name}",
                start.replace(ESCAPED, &SEPARATOR.to_string())
            ))
        });
    let candidates = [
        absolute.map(str::to_string),
        Some(read_back.clone()),
        joined,
    ];
    if let Some(name) = candidates
        .into_iter()
        .flatten()
        .find(|name| escape(name) == hashed)
    {
        return Some(name);
    }

    // None escapes to the file's name: the DWARF's name says no more than
    // it knows.
    Some(dwarf.map_or(read_back, str::to_string))
}

/// `name` as elfutils' `path_escape` writes it.
fn escape(name: &str) -> String {
    let bytes = name.as_bytes();
    let kept = &bytes[bytes.len() - bytes.len().min(KEPT_BYTES)..];
    format!(
        "{:0width$x}{HASH_SEPARATOR}{}",
        djb2(bytes),
        escape_bytes(kept),
        width = HASH_DIGITS
    )
}

/// `bytes` with each byte other than a letter, a digit, `.`, `-` or `_`
/// written as `#`.
fn escape_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| match b {
            b'.' | b'-' | b'_' => b as char,
            _ if b.is_ascii_alphanumeric() => b as char,
            _ => ESCAPED,
        })
        .collect()
}

/// The djb2 hash of `bytes`, cut to 32 bits, as elfutils computes it:
/// over C's `char`, which is signed on x86-64.
fn djb2(bytes: &[u8]) -> u32 {
    const START: u64 = 5381;
    const FACTOR: u64 = 33;
    let hash = bytes.iter().fold(START, |hash, &b| {
        hash.wrapping_mul(FACTOR)
            .wrapping_add(i64::from(b as i8) as u64)
    });
    hash as u32
}

#[cfg(test)]
mod tests {
    // Names in a debuginfod client's cache, some copied from a real cache
    // and some escaped by elfutils' rules by hand, read back to the names
    // the DWARF gives the files.
    use super::*;

    /// A client cache directory for a build ID, under a home directory.
    const CACHE: &str = "/home/someone/.cache/debuginfod_client/2486ec7a0b9f";

    fn name(file: &str, dwarf: Option<&str>) -> Option<String> {
        source_name(&Path::new(CACHE).join(file), dwarf)
    }

    #[test]
    fn a_cached_source_reads_back_as_the_name_gdb_asked_by() {
        // Names from a real cache: each `#` a `/`, and a `/./` gdb joined
        // in kept, the hash vouching for each. The DWARF's relative name
        // does not change the answer.
        assert_eq!(
            name(
                "source-c866380d-#build#glibc-2.44#nptl#pthread_join.c",
                None
            )
            .as_deref(),
            Some("/build/glibc-2.44/nptl/pthread_join.c")
        );
        assert_eq!(
            name("source-2ed34182-#build#bash-5.3#jobs.c", Some("jobs.c")).as_deref(),
            Some("/build/bash-5.3/jobs.c")
        );
        assert_eq!(
            name(
                "source-10c58f72-#build#linux-7.2.8#.#arch#x86#include#asm#shared#io.h",
                None
            )
            .as_deref(),
            Some("/build/linux-7.2.8/./arch/x86/include/asm/shared/io.h")
        );
    }

    #[test]
    fn a_name_the_escaping_blurs_takes_the_dwarfs_part() {
        // The `+` of libstdc++ escapes as `#`, so reading each `#` as a
        // `/` gives a name whose hash differs. An absolute DWARF name is
        // the answer; a relative one joined to the directories before it
        // is, when the hash vouches for the whole.
        let plus = "source-17358f2d-#build#gcc-14.3.0#libstdc##-v3#src#c##11#thread.cc";
        let whole = "/build/gcc-14.3.0/libstdc++-v3/src/c++11/thread.cc";
        assert_eq!(name(plus, Some(whole)).as_deref(), Some(whole));
        assert_eq!(
            name(plus, Some("libstdc++-v3/src/c++11/thread.cc")).as_deref(),
            Some(whole)
        );
        assert_eq!(escape(whole), plus.strip_prefix(SOURCE_PREFIX).unwrap());
    }

    #[test]
    fn a_long_name_lost_its_start_and_takes_the_dwarfs_name() {
        // A name past 117 bytes keeps its last 117: the DWARF's absolute
        // name vouches for the whole; its relative name, which the hash
        // cannot vouch for, is the answer as it is.
        let long = format!("/build/x/{}/f.c", "a".repeat(130));
        let file = format!("{SOURCE_PREFIX}{}", escape(&long));
        assert_eq!(file.len(), SOURCE_PREFIX.len() + 126);
        assert!(file.starts_with("source-35da3a02-aaaa"), "{file}");
        assert_eq!(name(&file, Some(&long)), Some(long.clone()));
        assert_eq!(name(&file, Some("f.c")).as_deref(), Some("f.c"));
    }

    #[test]
    fn other_paths_are_no_cached_source() {
        // A source written elsewhere, a debuginfo file in the cache, and a
        // name whose hash is not hex are not the client's sources.
        assert_eq!(source_name(Path::new("/build/src/pool.c"), None), None);
        assert_eq!(name("debuginfo", None), None);
        assert_eq!(name("source-zzzzzzzz-#a.c", None), None);
        assert_eq!(
            source_name(Path::new("/tmp/session/source-c866380d-#a.c"), None),
            None
        );
    }
}
