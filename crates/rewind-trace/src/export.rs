//! A `.rwd` export: a zstd-compressed tar of a run's directory. Every
//! export carries the manifest and trace, and the bookmarks the desktop
//! app made; a replayable one also carries the keyframes, the pages they
//! name and the VM's inputs. `rewind export` writes them, and `rewind
//! import` and the app read them.

use crate::manifest::KEYFRAMES_DIR;

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
