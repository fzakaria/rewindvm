//! The browser viewer's page around the window: the runs it hands the
//! scrubber, the links it could not fetch, and the address bar kept on the
//! exports on screen. `crate::web` is the page's side.

use std::path::PathBuf;

use futures::StreamExt;
use gpui::{Context, Window};

use crate::address::{Address, is_overtaken};
use crate::proc_card::pick_thread;
use crate::ui::RightColumn;
use crate::ui::desktop::desktop_command;
use crate::ui::link::unfetched_words;
use crate::ui::scrubber::{NoticeTone, Scrubber};
use crate::ui::tabs::RightTab;
use crate::web::{Delivery, absolute_url};

/// What the app says of a file it opens in the browser.
pub const NOT_UPLOADED: &str = "It is read in your browser and is not uploaded.";

/// What the Copy link button's note says.
pub const COPY_LINK_NOTE: &str = "Copies a link to this recording at the playhead, with the tab, the picked thread and the search as they are on screen. A recording opened from your disk has no link.";

/// What the notice says once the link is on the clipboard.
const LINK_COPIED: &str = "Link copied";

/// The example recordings served next to the viewer, which open without
/// CORS and without installing anything.
pub struct Example {
    pub title: &'static str,
    pub url: &'static str,
    pub compare: Option<&'static str>,
}

pub const EXAMPLES: [Example; 4] = [
    Example {
        title: "Nix's gc-closure test dies of SIGPIPE",
        url: "examples/nix-gc-closure.rwd",
        compare: None,
    },
    Example {
        title: "A thread pool test crashes under one schedule, compared with one that passes",
        url: "examples/mylib-fail.rwd",
        compare: Some("examples/mylib-pass.rwd"),
    },
    Example {
        title: "Five dining philosophers deadlock",
        url: "examples/philosophers.rwd",
        compare: None,
    },
    Example {
        title: "Two bank tellers lose a deposit",
        url: "examples/bank.rwd",
        compare: None,
    },
];

impl Scrubber {
    /// Takes what the page delivers for as long as the window is open.
    pub(super) fn listen_to_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut inbox) = crate::web::take_inbox() else {
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            while let Some(delivery) = inbox.next().await {
                let taken = this.update_in(cx, |this, window, cx| {
                    this.take_delivery(delivery, window, cx)
                });
                if taken.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn take_delivery(&mut self, delivery: Delivery, window: &mut Window, cx: &mut Context<Self>) {
        // What a fetch or file open older than the newest brings is
        // dropped: the newer one's run is on screen, or on its way.
        let request = delivery.request();
        if is_overtaken(self.page_newest, request) {
            return;
        }
        self.page_newest = Some(request);
        match delivery {
            Delivery::Fetching { url, .. } => {
                self.loading = Some(PathBuf::from(url));
            }
            Delivery::Open {
                session, address, ..
            } => {
                self.loading = None;
                self.address_bar.opened(&address);
                self.show(*session, address.step, cx);
                self.show_view(&address, window, cx);
            }
            Delivery::Unfetched { url, answer, .. } => {
                self.loading = None;
                let why = unfetched_words(&url, answer.as_deref());
                self.notify_user(NoticeTone::Error, "Could not open the link", why, cx);
            }
            Delivery::Unread { name, why, .. } => {
                self.loading = None;
                self.notify_user(NoticeTone::Error, format!("Could not open {name}"), why, cx);
            }
        }
        cx.notify();
    }

    /// Opens the rest of the view `address` asks for, once its run is on
    /// screen at its step: the thread picked in the process tree, the
    /// tab, and the search box with its text. The thread goes first, as
    /// picking one shows "At this step". A tab the run cannot show, as
    /// the Compare tab of a run compared with nothing, stays closed.
    fn show_view(&mut self, address: &Address, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        let compared = session.other.is_some();
        let picked = address
            .thread
            .and_then(|tid| pick_thread(&session.run.timeline, tid, self.step));
        if picked.is_some() {
            self.pick_row(picked, cx);
        }
        match address.tab.map(RightTab::from_link) {
            Some(RightTab::Threads) => self.open_lanes(cx),
            Some(RightTab::Compare) if compared => self.select_tab(RightTab::Compare, cx),
            Some(RightTab::Bookmarks) => self.select_tab(RightTab::Bookmarks, cx),
            _ => {}
        }
        if let Some(text) = &address.search {
            self.open_search(window, cx);
            if let Some(search) = &self.shown.search {
                search
                    .input
                    .update(cx, |input, cx| input.insert(text, window, cx));
            }
        }
    }

    /// Opens an example served next to the viewer.
    pub(super) fn open_example(&mut self, example: &Example, cx: &mut Context<Self>) {
        let address = Address {
            url: Some(example.url.to_string()),
            compare: example.compare.map(str::to_string),
            ..Address::default()
        };
        crate::web::fetch(&address);
        cx.notify();
    }

    /// Opens the export at `link`, which the page fetches.
    pub(super) fn open_link(&mut self, link: String, cx: &mut Context<Self>) {
        let address = Address {
            url: Some(link),
            ..Address::default()
        };
        crate::web::fetch(&address);
        cx.notify();
    }

    /// The address of what is on screen: the exports, the playhead, the
    /// tab, the thread picked in the process tree and the text in the
    /// search box, when there is any. None before a run is on screen.
    fn address_here(&self, cx: &Context<Self>) -> Option<Address> {
        self.session.as_ref()?;
        let search = self
            .shown
            .search
            .as_ref()
            .map(|search| search.input.read(cx).text().to_string())
            .filter(|text| !text.is_empty());
        Some(Address {
            step: Some(self.step),
            tab: self.right_tab.linked(),
            thread: self.shown.picked_row.map(|pick| pick.tid),
            search,
            ..self.address_bar.address.clone()
        })
    }

    /// Keeps the address bar on what is on screen. Before a run is on
    /// screen the address the page was opened with stays, so a link that
    /// did not open can be tried again.
    pub(super) fn sync_address(&mut self, cx: &Context<Self>) {
        let Some(address) = self.address_here(cx) else {
            return;
        };
        self.address_bar.show(address);
    }

    /// The link to what is on screen, for another browser: the page's
    /// address with the query of [`Self::address_here`]. None before a
    /// run is on screen, and for a run opened from a file, which no link
    /// reaches.
    pub(super) fn link_here(&self, cx: &Context<Self>) -> Option<String> {
        let query = self.address_here(cx)?.query();
        if query.is_empty() {
            return None;
        }
        Some(absolute_url(&query))
    }

    /// Puts the link to what is on screen on the clipboard, and says so.
    pub(super) fn copy_link(&mut self, cx: &mut Context<Self>) {
        let Some(link) = self.link_here(cx) else {
            return;
        };
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(link.clone()));
        self.notify_user(NoticeTone::Info, LINK_COPIED, link, cx);
    }

    /// The command that opens the run on screen in the desktop app at
    /// the playhead, with the right column on `right`: the run and the
    /// one it is compared with by their links, made absolute for a shell
    /// on another machine, or by their names for runs opened from files.
    /// None before a run is on screen.
    pub(super) fn desktop_command_here(&self, right: RightColumn) -> Option<String> {
        let session = self.session.as_ref()?;
        let address = &self.address_bar.address;
        let run = match &address.url {
            Some(url) => absolute_url(url),
            None => session.run.path.display().to_string(),
        };
        let compare = match (&address.compare, &session.other) {
            (Some(url), _) => Some(absolute_url(url)),
            (None, Some(other)) => Some(other.path.display().to_string()),
            (None, None) => None,
        };
        Some(desktop_command(&run, compare.as_deref(), self.step, right))
    }
}
