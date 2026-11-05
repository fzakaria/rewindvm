//! The store's one mapping of a file: the sorted copy of its index, read
//! where it lies (see [`rewind_mapped`]).

#![allow(
    unsafe_code,
    reason = "a file mapping's soundness rests on how the store names files"
)]

use std::fs::File;

use rewind_mapped::Mapped;

/// The first `len` bytes of `file`, a sorted copy the store opened by its
/// name, mapped read-only.
pub(crate) fn sorted(file: &File, len: usize) -> std::io::Result<Mapped> {
    // SAFETY: the store never writes a file that has the sorted copy's
    // name. `write_sorted` writes each copy in full under a name of its
    // own, created for it, and renames it into place; a collector removes
    // the name. A mapping of a copy replaced or removed keeps its bytes.
    // The store's directory is Rewind's own, and a program that cut the
    // file short in place, outside the store, would break this.
    unsafe { Mapped::file(file, len) }
}
