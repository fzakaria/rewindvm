//! Bookmarks: steps of a run marked with a note, kept in the run's
//! directory as `bookmarks.json` so they stay with the run, and carried in
//! its `.rwd` exports, which the engine copies the file into and out of.

use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// The file a run's bookmarks are kept in, in its directory, and the
/// largest one read, which the engine refuses on import.
pub use rewind_trace::export::MAX_BOOKMARKS as MAX_BYTES;
pub use rewind_trace::manifest::BOOKMARKS as BOOKMARKS_FILE;

/// The version of the format `bookmarks.json` is in.
pub const BOOKMARKS_VERSION: u32 = 1;

/// One marked step.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bookmark {
    pub step: u64,
    pub note: String,
}

/// What `bookmarks.json` holds: the version of its format, then the marks.
#[derive(Serialize, Deserialize)]
struct File {
    version: u32,
    marks: Vec<Bookmark>,
}

/// A file's version alone, read before the rest of it.
#[derive(Deserialize)]
struct Version {
    version: u32,
}

/// A run's bookmarks, one per step, in step order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bookmarks {
    marks: Vec<Bookmark>,
    /// Why the run's bookmarks file was not read, when one is there that
    /// is not bookmarks of this format. Saving leaves that file as it is.
    unread: Option<String>,
}

impl Bookmarks {
    /// The bookmarks kept in `dir`; none when it has no file, or one this
    /// app does not read, which saving then leaves alone.
    pub fn load(dir: &Path) -> Bookmarks {
        let path = dir.join(BOOKMARKS_FILE);
        let Ok(meta) = fs::metadata(&path) else {
            return Bookmarks::default();
        };
        let unread = |why: String| Bookmarks {
            unread: Some(why),
            ..Bookmarks::default()
        };
        if !meta.is_file() || meta.len() > MAX_BYTES {
            return unread(format!("is not a file of at most {MAX_BYTES} bytes"));
        }
        let Ok(bytes) = fs::read(&path) else {
            return unread("does not read".into());
        };
        match serde_json::from_slice::<Version>(&bytes) {
            Ok(Version { version }) if version != BOOKMARKS_VERSION => {
                return unread(format!(
                    "is in format {version}, and this app reads format {BOOKMARKS_VERSION}"
                ));
            }
            Ok(_) => {}
            Err(_) => return unread("names no format this app reads".into()),
        }
        let Ok(file) = serde_json::from_slice::<File>(&bytes) else {
            return unread("is not bookmarks".into());
        };
        let mut bookmarks = Bookmarks::default();
        for mark in file.marks {
            bookmarks.set(mark.step, mark.note);
        }
        bookmarks
    }

    /// Writes the bookmarks to `dir`, whole or not at all: to a temporary
    /// file first, renamed over the old one. No bookmarks left removes the
    /// file. A file there that `load` could not read is left as it is, and
    /// saving says why.
    pub fn save(&self, dir: &Path) -> io::Result<()> {
        if let Some(why) = &self.unread {
            return Err(io::Error::other(format!(
                "{BOOKMARKS_FILE} {why}, so it is left as it is"
            )));
        }
        let path = dir.join(BOOKMARKS_FILE);
        if self.marks.is_empty() {
            return match fs::remove_file(&path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
        }
        let tmp = dir.join(format!(".{BOOKMARKS_FILE}.{}", std::process::id()));
        let file = File {
            version: BOOKMARKS_VERSION,
            marks: self.marks.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&file).map_err(io::Error::other)?;
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

    #[test]
    fn a_bookmarks_file_names_its_format_and_is_kept_when_unread() {
        // Bookmarks save under the version of their format and read back.
        // A file of another version, from before bookmarks named theirs,
        // or not bookmarks at all reads as none and is neither saved over
        // nor removed, so notes this app cannot read wait for one that can.
        let dir = temp_dir("format");
        let mut b = Bookmarks::default();
        b.set(5, "five".into());
        b.save(&dir).unwrap();
        let saved: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.join(BOOKMARKS_FILE)).unwrap()).unwrap();
        assert_eq!(saved["version"], BOOKMARKS_VERSION);
        assert_eq!(Bookmarks::load(&dir), b);

        let other = format!(r#"{{"version": {}, "marks": []}}"#, BOOKMARKS_VERSION + 1);
        let before = r#"[{"step": 5, "note": "before"}]"#.to_string();
        for unread in [other, before, "not json".to_string()] {
            fs::write(dir.join(BOOKMARKS_FILE), &unread).unwrap();
            let mut loaded = Bookmarks::load(&dir);
            assert!(loaded.is_empty());
            assert!(loaded.save(&dir).is_err());
            loaded.set(9, "new".into());
            let err = loaded.save(&dir).unwrap_err().to_string();
            assert!(err.contains(BOOKMARKS_FILE), "{err}");
            assert_eq!(
                fs::read_to_string(dir.join(BOOKMARKS_FILE)).unwrap(),
                unread
            );
        }
        let err = {
            fs::write(
                dir.join(BOOKMARKS_FILE),
                format!(r#"{{"version": {}, "marks": []}}"#, BOOKMARKS_VERSION + 1),
            )
            .unwrap();
            Bookmarks::load(&dir).save(&dir).unwrap_err().to_string()
        };
        assert!(
            err.contains(&format!("format {}", BOOKMARKS_VERSION + 1)),
            "{err}"
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}
