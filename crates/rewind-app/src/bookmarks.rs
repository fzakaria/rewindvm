//! Bookmarks: steps of a run marked with a note, kept in the run's
//! directory as `bookmarks.json` so they stay with the run, and carried in
//! its `.rwd` exports, which the engine copies the file into and out of.

use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// The file a run's bookmarks are kept in, in its directory, as the engine
/// names it (`rewind_core::run::BOOKMARKS`).
pub const BOOKMARKS_FILE: &str = "bookmarks.json";

/// The largest bookmarks file read; the engine refuses a larger one on
/// import.
pub const MAX_BYTES: u64 = 1 << 20;

/// One marked step.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bookmark {
    pub step: u64,
    pub note: String,
}

/// A run's bookmarks, one per step, in step order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bookmarks {
    marks: Vec<Bookmark>,
}

impl Bookmarks {
    /// The bookmarks kept in `dir`; none when it has no file, or one too
    /// large or not readable as bookmarks.
    pub fn load(dir: &Path) -> Bookmarks {
        let path = dir.join(BOOKMARKS_FILE);
        let small = fs::metadata(&path).is_ok_and(|m| m.is_file() && m.len() <= MAX_BYTES);
        if !small {
            return Bookmarks::default();
        }
        let marks: Vec<Bookmark> = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let mut bookmarks = Bookmarks::default();
        for mark in marks {
            bookmarks.set(mark.step, mark.note);
        }
        bookmarks
    }

    /// Writes the bookmarks to `dir`, whole or not at all: to a temporary
    /// file first, renamed over the old one. No bookmarks left removes the
    /// file.
    pub fn save(&self, dir: &Path) -> io::Result<()> {
        let path = dir.join(BOOKMARKS_FILE);
        if self.marks.is_empty() {
            return match fs::remove_file(&path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
        }
        let tmp = dir.join(format!(".{BOOKMARKS_FILE}.{}", std::process::id()));
        let bytes = serde_json::to_vec_pretty(&self.marks).map_err(io::Error::other)?;
        fs::write(&tmp, bytes)?;
        fs::rename(&tmp, &path)
    }

    /// Marks `step` with `note`, replacing the note of a bookmark already
    /// there.
    pub fn set(&mut self, step: u64, note: String) {
        match self.marks.binary_search_by_key(&step, |m| m.step) {
            Ok(i) => self.marks[i].note = note,
            Err(i) => self.marks.insert(i, Bookmark { step, note }),
        }
    }

    /// Removes the bookmark at `step`, if there is one.
    pub fn remove(&mut self, step: u64) {
        self.marks.retain(|m| m.step != step);
    }

    /// The bookmark at `step`, if there is one.
    pub fn at(&self, step: u64) -> Option<&Bookmark> {
        self.marks.iter().find(|m| m.step == step)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Bookmark> {
        self.marks.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.marks.is_empty()
    }

    pub fn len(&self) -> usize {
        self.marks.len()
    }
}

#[cfg(test)]
mod tests {
    // Bookmarks set, removed, saved to a temporary directory and read
    // back.
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rewind-app-bookmarks-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn bookmarks_stay_in_step_order_one_per_step() {
        // Steps set out of order come back in order; setting a step again
        // replaces its note; removing takes the step out.
        let mut b = Bookmarks::default();
        b.set(900, "failure".into());
        b.set(10, "start".into());
        b.set(400, "workers".into());
        b.set(400, "workers part".into());
        let steps: Vec<u64> = b.iter().map(|m| m.step).collect();
        assert_eq!(steps, vec![10, 400, 900]);
        assert_eq!(b.at(400).unwrap().note, "workers part");
        b.remove(10);
        assert_eq!(b.len(), 2);
        assert_eq!(b.at(10), None);
    }

    #[test]
    fn bookmarks_are_kept_in_the_run_s_directory() {
        // Saved bookmarks read back the same; with none left the file
        // goes; a file that is not bookmarks reads as none.
        let dir = temp_dir("save");
        let mut b = Bookmarks::default();
        b.set(5, "five".into());
        b.set(70, String::new());
        b.save(&dir).unwrap();
        assert_eq!(Bookmarks::load(&dir), b);

        b.remove(5);
        b.remove(70);
        b.save(&dir).unwrap();
        assert!(!dir.join(BOOKMARKS_FILE).exists());
        b.save(&dir).unwrap();

        fs::write(dir.join(BOOKMARKS_FILE), "not json").unwrap();
        assert!(Bookmarks::load(&dir).is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }
}
