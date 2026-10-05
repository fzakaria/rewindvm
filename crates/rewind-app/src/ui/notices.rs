//! The notices over the scrubber: what each says and offers, in the order
//! they came. Nothing here draws or times a notice out, so the rules of the
//! list are tested without a window.

use gpui::SharedString;

use super::scrubber::{NoticeAction, NoticeTone};

/// One notice on screen.
pub struct Notice {
    pub id: u64,
    pub tone: NoticeTone,
    pub title: SharedString,
    pub body: SharedString,
    pub actions: Vec<NoticeAction>,
}

/// The notices on screen, oldest first.
#[derive(Default)]
pub struct Notices {
    list: Vec<Notice>,
    /// The id the next notice takes, never given twice.
    next: u64,
}

impl Notices {
    /// Puts up a notice and returns its id. The same message again
    /// replaces the one on screen rather than stacking a copy.
    pub fn post(
        &mut self,
        tone: NoticeTone,
        title: SharedString,
        body: SharedString,
        actions: Vec<NoticeAction>,
    ) -> u64 {
        self.list.retain(|n| n.title != title || n.body != body);
        let id = self.next;
        self.next += 1;
        self.list.push(Notice {
            id,
            tone,
            title,
            body,
            actions,
        });
        id
    }

    /// Changes the body of notice `id`. False when it is gone.
    pub fn set_body(&mut self, id: u64, body: SharedString) -> bool {
        let Some(notice) = self.list.iter_mut().find(|n| n.id == id) else {
            return false;
        };
        notice.body = body;
        true
    }

    /// Takes notice `id` away.
    pub fn remove(&mut self, id: u64) {
        self.list.retain(|n| n.id != id);
    }

    /// Takes away every notice titled `title`.
    pub fn remove_titled(&mut self, title: &str) {
        self.list.retain(|n| n.title.as_ref() != title);
    }

    /// Notice `id`, while it is on screen.
    pub fn find(&self, id: u64) -> Option<&Notice> {
        self.list.iter().find(|n| n.id == id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Notice> {
        self.list.iter()
    }
}

#[cfg(test)]
mod tests {
    // The list of notices, as notices are put up, changed and closed.
    use super::*;

    #[test]
    fn the_same_message_replaces_the_one_on_screen() {
        // A notice posted twice is one notice, under the newer id; another
        // message stacks; a closed notice cannot be changed, and ids are
        // never given twice.
        let mut notices = Notices::default();
        let post = |n: &mut Notices, title: &'static str, body: &'static str| {
            n.post(NoticeTone::Info, title.into(), body.into(), Vec::new())
        };
        let first = post(&mut notices, "Forked", "same as its parent");
        let again = post(&mut notices, "Forked", "same as its parent");
        let other = post(&mut notices, "Forked", "first differs at step 9");
        let titles: Vec<u64> = notices.iter().map(|n| n.id).collect();
        assert_eq!(titles, vec![again, other]);
        assert!(notices.find(first).is_none());

        assert!(notices.set_body(other, "first differs at step 10".into()));
        assert_eq!(
            notices.find(other).unwrap().body.as_ref(),
            "first differs at step 10"
        );
        notices.remove(other);
        assert!(!notices.set_body(other, "gone".into()));
        notices.remove_titled("Forked");
        assert_eq!(notices.iter().count(), 0);
        assert_ne!(post(&mut notices, "Forked", "x"), other);
    }
}
