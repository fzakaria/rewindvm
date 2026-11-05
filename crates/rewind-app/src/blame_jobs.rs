//! The window's memory blame searches: which run, which wait, and the
//! answers kept.
//!
//! Each search runs the engine, which restores a VM of the run's memory,
//! so few run at once: at most [`MAX_RUNNING`], and of those at most
//! [`MAX_AHEAD_RUNNING`] made ahead of a click. A click's search goes
//! before every search made ahead, and one made ahead for a frame no
//! longer shown stops. A search is kept under a key of its run, process,
//! step, bytes and the class they print as, and asking again for a key
//! kept or running reuses it.
//!
//! A search stopped and asked for again under the same key is a new
//! request: the old one's late progress and answer carry its own number
//! and are dropped. Dropping the jobs, as closing the window or quitting
//! does, stops every search still running.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use crate::blame::{Ask, Blame, Blamed, Searching, Target, ValueClass};
use crate::clock::Instant;
use crate::engine::{Cancel, EngineError};
use crate::request::{Request, Requests};

/// The most searches that run at once.
pub const MAX_RUNNING: usize = 2;

/// The most searches made ahead of a click that run at once, and that
/// wait.
pub const MAX_AHEAD_RUNNING: usize = 1;
pub const MAX_AHEAD_WAITING: usize = 8;

/// The most finished searches kept; past it the oldest go.
pub const MAX_KEPT: usize = 64;

/// What keys a search: the run, the process whose memory it is, the
/// step, the value's address, size and class, and what is asked.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BlameKey {
    pub run: PathBuf,
    pub pid: u32,
    pub step: u64,
    pub address: u64,
    pub size: u64,
    pub class: ValueClass,
    pub ask: Ask,
}

impl BlameKey {
    /// The key of a search of `run` for `target`, asking `ask`.
    pub fn of(run: &Path, target: &Target, ask: Ask) -> BlameKey {
        BlameKey {
            run: run.to_path_buf(),
            pid: target.pid,
            step: target.step,
            address: target.address,
            size: target.size,
            class: target.class,
            ask,
        }
    }
}

/// Why a search was made: a click on its value, or ahead of one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    Clicked,
    Ahead,
}

/// A search, its answer once it comes, and what stops it.
pub struct BlameEntry {
    pub blame: Blame,
    why: Why,
    request: Request,
    /// When it began to run; None while it waits its turn.
    started: Option<Instant>,
    /// The time it took, once it is done.
    took_ms: Option<u64>,
    cancel: Option<Cancel>,
}

impl BlameEntry {
    /// How long the search has run so far, or took; 0 while it waits.
    pub fn elapsed_ms(&self) -> u64 {
        match (self.took_ms, self.started) {
            (Some(took), _) => took,
            (None, Some(started)) => started.elapsed().as_millis() as u64,
            (None, None) => 0,
        }
    }

    /// Whether the search waits for its turn to run.
    pub fn waiting(&self) -> bool {
        self.started.is_none() && self.blame.answer.is_none()
    }

    fn running(&self) -> bool {
        self.started.is_some() && self.blame.answer.is_none()
    }

    fn failed(&self) -> bool {
        matches!(self.blame.answer, Some(Err(_)))
    }
}

/// A search to run now: what the engine is asked, the request its lines
/// carry, and what stops it.
pub struct Start {
    pub key: BlameKey,
    pub request: Request,
    pub target: Target,
    pub ask: Ask,
    pub cancel: Cancel,
}

/// The window's searches.
#[derive(Default)]
pub struct Jobs {
    entries: HashMap<BlameKey, BlameEntry>,
    /// The searches waiting to run, in the order they run: clicks first.
    waiting: VecDeque<BlameKey>,
    requests: Requests,
}

impl Jobs {
    /// The search under `key`, made or waiting.
    pub fn get(&self, key: &BlameKey) -> Option<&BlameEntry> {
        self.entries.get(key)
    }

    /// Asks for a search of `target` under `key`, for `why`. A search
    /// kept under the key, unless it failed, is reused, and a click on
    /// one made ahead makes it the click's. A search made ahead past the
    /// ones waiting is not made. [`Jobs::start`] says what to run.
    pub fn ask(&mut self, key: BlameKey, target: Target, why: Why) {
        if let Some(entry) = self.entries.get_mut(&key)
            && !entry.failed()
        {
            if why == Why::Clicked && entry.why == Why::Ahead {
                entry.why = Why::Clicked;
                if entry.waiting() {
                    self.waiting.retain(|k| *k != key);
                    self.wait(key);
                }
            }
            return;
        }
        let ahead_waiting = self
            .waiting
            .iter()
            .filter(|k| self.entries[*k].why == Why::Ahead)
            .count();
        if why == Why::Ahead && ahead_waiting >= MAX_AHEAD_WAITING {
            return;
        }
        let entry = BlameEntry {
            blame: Blame::new(target, key.ask),
            why,
            request: self.requests.issue(),
            started: None,
            took_ms: None,
            cancel: None,
        };
        self.entries.insert(key.clone(), entry);
        self.wait(key);
    }

    /// Puts the search under `key` in the line to run: after the clicks
    /// waiting for a click, last for one made ahead.
    fn wait(&mut self, key: BlameKey) {
        let at = match self.entries[&key].why {
            Why::Clicked => self
                .waiting
                .iter()
                .position(|k| self.entries[k].why == Why::Ahead)
                .unwrap_or(self.waiting.len()),
            Why::Ahead => self.waiting.len(),
        };
        self.waiting.insert(at, key);
    }

    /// The waiting searches that may run now, marked as running.
    pub fn start(&mut self) -> Vec<Start> {
        let mut out = Vec::new();
        loop {
            let running: Vec<Why> = self
                .entries
                .values()
                .filter(|e| e.running())
                .map(|e| e.why)
                .collect();
            if running.len() >= MAX_RUNNING {
                break;
            }
            let ahead = running.iter().filter(|w| **w == Why::Ahead).count();
            let Some(at) = self
                .waiting
                .iter()
                .position(|k| self.entries[k].why == Why::Clicked || ahead < MAX_AHEAD_RUNNING)
            else {
                break;
            };
            let key = self.waiting.remove(at).expect("a position in the line");
            let entry = self
                .entries
                .get_mut(&key)
                .expect("a waiting search is kept");
            let cancel = Cancel::default();
            entry.started = Some(Instant::now());
            entry.cancel = Some(cancel.clone());
            out.push(Start {
                key,
                request: entry.request,
                target: entry.blame.target.clone(),
                ask: entry.blame.ask,
                cancel,
            });
        }
        out
    }

    /// The engine's latest word on the search under `key`, if `request`
    /// is still that search's. False when it is not.
    pub fn progress(&mut self, key: &BlameKey, request: Request, progress: Searching) -> bool {
        let Some(entry) = self.current(key, request) else {
            return false;
        };
        entry.blame.progress = Some(progress);
        true
    }

    /// The answer to the search under `key`, if `request` is still that
    /// search's; the oldest searches past [`MAX_KEPT`] go then, but for
    /// `shown`. False when it is not.
    pub fn finish(
        &mut self,
        key: &BlameKey,
        request: Request,
        answer: Result<Blamed, EngineError>,
        shown: Option<&BlameKey>,
    ) -> bool {
        let Some(entry) = self.current(key, request) else {
            return false;
        };
        entry.took_ms = Some(entry.elapsed_ms());
        entry.blame.answer = Some(answer);
        entry.cancel = None;
        self.forget_oldest(shown);
        true
    }

    /// The search under `key`, while `request` is its.
    fn current(&mut self, key: &BlameKey, request: Request) -> Option<&mut BlameEntry> {
        self.entries.get_mut(key).filter(|e| e.request == request)
    }

    /// Stops the search under `key`, running or waiting, and forgets it.
    pub fn stop(&mut self, key: &BlameKey) {
        if let Some(entry) = self.entries.remove(key)
            && let Some(cancel) = entry.cancel
        {
            cancel.cancel();
        }
        self.waiting.retain(|k| k != key);
    }

    /// Searches made ahead are wanted now for `wanted` alone: the others
    /// still running or waiting stop, but for `shown`.
    pub fn ahead_only(&mut self, wanted: &[BlameKey], shown: Option<&BlameKey>) {
        let obsolete: Vec<BlameKey> = self
            .entries
            .iter()
            .filter(|(key, e)| {
                e.why == Why::Ahead
                    && e.blame.answer.is_none()
                    && !wanted.contains(key)
                    && Some(*key) != shown
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in obsolete {
            self.stop(&key);
        }
    }

    /// Forgets the oldest finished searches past [`MAX_KEPT`], never
    /// `shown`.
    fn forget_oldest(&mut self, shown: Option<&BlameKey>) {
        let mut finished: Vec<(Request, BlameKey)> = self
            .entries
            .iter()
            .filter(|(key, e)| e.blame.answer.is_some() && Some(*key) != shown)
            .map(|(key, e)| (e.request, key.clone()))
            .collect();
        if finished.len() <= MAX_KEPT {
            return;
        }
        finished.sort_by_key(|(request, _)| *request);
        let past = finished.len() - MAX_KEPT;
        for (_, key) in finished.into_iter().take(past) {
            self.entries.remove(&key);
        }
    }

    /// Stops every search running or waiting.
    pub fn stop_all(&mut self) {
        let keys: Vec<BlameKey> = self
            .entries
            .iter()
            .filter(|(_, e)| e.blame.answer.is_none())
            .map(|(key, _)| key.clone())
            .collect();
        for key in keys {
            self.stop(&key);
        }
    }
}

impl Drop for Jobs {
    fn drop(&mut self) {
        self.stop_all();
    }
}

#[cfg(test)]
mod tests {
    // The window's searches with no engine and no UI: targets made up,
    // searches started, answered and stopped by hand in the order a test
    // names, as the engine's thread and the UI would.
    use super::*;

    fn target(address: u64, pid: u32) -> Target {
        Target {
            text: format!("v{address:x}"),
            address,
            size: 8,
            class: ValueClass::Unsigned,
            pid,
            tid: pid,
            step: 100,
        }
    }

    fn key(target: &Target) -> BlameKey {
        BlameKey::of(Path::new("/runs/a"), target, Ask::Writes(1))
    }

    fn answer() -> Result<Blamed, EngineError> {
        Err(EngineError::Cancelled)
    }

    fn progress() -> Searching {
        serde_json::from_value(serde_json::json!({
            "phase": "searching", "before": 100, "window": [90, 100], "reached": 95,
            "steps": 5, "windows": 0, "writes": 0, "reads": 0, "elapsed_ms": 10
        }))
        .unwrap()
    }

    /// The same address and size at the same step in two processes, a
    /// parent and the child it forked, are two searches; asking again in
    /// one process reuses its search.
    #[test]
    fn each_process_has_its_own_search() {
        let (parent, child) = (target(0x4000, 10), target(0x4000, 11));
        assert_ne!(key(&parent), key(&child));

        let mut jobs = Jobs::default();
        jobs.ask(key(&parent), parent.clone(), Why::Clicked);
        jobs.ask(key(&child), child.clone(), Why::Clicked);
        assert_eq!(jobs.start().len(), 2);
        jobs.ask(key(&parent), parent, Why::Clicked);
        assert!(jobs.start().is_empty());
        assert_eq!(jobs.entries.len(), 2);
    }

    /// A search stopped and asked for again under the same key is a new
    /// request: the stopped one's progress and answer, coming late, leave
    /// the new one running and stoppable.
    #[test]
    fn a_stopped_search_cannot_write_into_its_replacement() {
        let value = target(0x4000, 10);
        let mut jobs = Jobs::default();
        jobs.ask(key(&value), value.clone(), Why::Clicked);
        let first = jobs.start().pop().unwrap();
        jobs.stop(&first.key);
        assert!(first.cancel.is_cancelled());

        jobs.ask(key(&value), value.clone(), Why::Clicked);
        let second = jobs.start().pop().unwrap();
        assert!(!jobs.progress(&first.key, first.request, progress()));
        assert!(!jobs.finish(&first.key, first.request, answer(), None));
        let entry = jobs.get(&key(&value)).unwrap();
        assert!(entry.blame.progress.is_none() && entry.blame.answer.is_none());

        jobs.stop(&second.key);
        assert!(second.cancel.is_cancelled());
    }

    /// Ten values made ahead run one at a time, with eight waiting and
    /// the rest not made; a click goes before every one waiting and runs
    /// beside the one made ahead, and a third search waits for a turn.
    #[test]
    fn searches_ahead_run_one_at_a_time_behind_clicks() {
        // As the app does, each ask is followed by starting what may run.
        let mut jobs = Jobs::default();
        let ahead: Vec<Target> = (0..10).map(|i| target(0x1000 * (i + 1), 10)).collect();
        let mut started = Vec::new();
        for t in &ahead {
            jobs.ask(key(t), t.clone(), Why::Ahead);
            started.extend(jobs.start());
        }
        assert_eq!(started.len(), MAX_AHEAD_RUNNING);
        assert_eq!(jobs.waiting.len(), MAX_AHEAD_WAITING);
        assert_eq!(jobs.entries.len(), MAX_AHEAD_RUNNING + MAX_AHEAD_WAITING);

        let clicked = target(0xbeef0, 10);
        jobs.ask(key(&clicked), clicked.clone(), Why::Clicked);
        let next = jobs.start();
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].key, key(&clicked));
        assert!(jobs.start().is_empty());

        // The click's answer frees a place, which the next made ahead
        // takes only once the one made ahead is done.
        jobs.finish(&next[0].key, next[0].request, answer(), None);
        assert!(jobs.start().is_empty());
        jobs.finish(&started[0].key, started[0].request, answer(), None);
        assert_eq!(jobs.start()[0].key, key(&ahead[1]));
    }

    /// A click on a value made ahead and waiting makes its search the
    /// click's, which runs next.
    #[test]
    fn a_click_takes_over_a_search_made_ahead() {
        let mut jobs = Jobs::default();
        let (a, b) = (target(0x1000, 10), target(0x2000, 10));
        jobs.ask(key(&a), a.clone(), Why::Ahead);
        jobs.ask(key(&b), b.clone(), Why::Ahead);
        let first = jobs.start();
        jobs.ask(key(&b), b.clone(), Why::Clicked);
        assert_eq!(jobs.start()[0].key, key(&b));
        assert_eq!(first[0].key, key(&a));
    }

    /// When the frame changes, the searches made ahead for its values
    /// stop, running or waiting, but for one on screen and for clicks.
    #[test]
    fn searches_ahead_for_another_frame_stop() {
        let mut jobs = Jobs::default();
        let (a, b, c) = (target(0x1000, 10), target(0x2000, 10), target(0x3000, 10));
        jobs.ask(key(&a), a.clone(), Why::Ahead);
        jobs.ask(key(&b), b.clone(), Why::Ahead);
        jobs.ask(key(&c), c.clone(), Why::Clicked);
        let started = jobs.start();
        jobs.ahead_only(&[], None);
        let a_started = started.iter().find(|s| s.key == key(&a)).unwrap();
        assert!(a_started.cancel.is_cancelled());
        assert!(jobs.get(&key(&a)).is_none() && jobs.get(&key(&b)).is_none());
        assert!(jobs.get(&key(&c)).is_some());
    }

    /// Past the most kept, the oldest answers go, but never the one on
    /// screen; dropping the jobs stops what still runs.
    #[test]
    fn answers_past_the_most_kept_go_oldest_first() {
        let mut jobs = Jobs::default();
        let first = target(0x10, 10);
        let mut shown = None;
        for i in 0..=MAX_KEPT as u64 + 1 {
            let t = target(0x10 * (i + 1), 10);
            jobs.ask(key(&t), t.clone(), Why::Clicked);
            for s in jobs.start() {
                jobs.finish(&s.key, s.request, answer(), shown.as_ref());
            }
            shown = Some(key(&first));
        }
        assert_eq!(jobs.entries.len(), MAX_KEPT + 1);
        assert!(jobs.get(&key(&first)).is_some());
        assert!(jobs.get(&key(&target(0x20, 10))).is_none());

        let last = target(0xfff0, 10);
        jobs.ask(key(&last), last, Why::Clicked);
        let running = jobs.start().pop().unwrap();
        drop(jobs);
        assert!(running.cancel.is_cancelled());
    }
}
