//! The browser viewer's page around the window: the runs it hands the
//! scrubber, the links it could not fetch, and the address bar kept on the
//! exports on screen. `crate::web` is the page's side.

use std::path::PathBuf;

use futures::StreamExt;
use gpui::Context;

use crate::address::{Address, is_overtaken};
use crate::ui::scrubber::{NoticeTone, Scrubber};
use crate::web::Delivery;

/// What the app says of a link the browser could not fetch, and of the
/// links the Open link dialog takes.
pub const CORS_NOTE: &str = "The link must be served with CORS headers.";

/// What the app says of a file it opens in the browser.
pub const NOT_UPLOADED: &str = "It is read in your browser and is not uploaded.";

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
    pub(super) fn listen_to_page(&mut self, cx: &mut Context<Self>) {
        let Some(mut inbox) = crate::web::take_inbox() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            while let Some(delivery) = inbox.next().await {
                let taken = this.update(cx, |this, cx| this.take_delivery(delivery, cx));
                if taken.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn take_delivery(&mut self, delivery: Delivery, cx: &mut Context<Self>) {
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
                let step = address.step;
                self.address_bar.address = Address {
                    step: None,
                    ..address
                };
                self.show(*session, step, cx);
            }
            Delivery::Unfetched { url, answer, .. } => {
                self.loading = None;
                let why = match answer {
                    Some(status) => format!("{url} answered {status}."),
                    None => format!("The browser could not fetch {url}. {CORS_NOTE}"),
                };
                self.notify_user(NoticeTone::Error, "Could not open the link", why, cx);
            }
            Delivery::Unread { name, why, .. } => {
                self.loading = None;
                self.notify_user(NoticeTone::Error, format!("Could not open {name}"), why, cx);
            }
        }
        cx.notify();
    }

    /// Opens an example served next to the viewer.
    pub(super) fn open_example(&mut self, example: &Example, cx: &mut Context<Self>) {
        let address = Address {
            url: Some(example.url.to_string()),
            compare: example.compare.map(str::to_string),
            step: None,
        };
        crate::web::fetch(&address);
        cx.notify();
    }

    /// Opens the export at `link`, which the page fetches.
    pub(super) fn open_link(&mut self, link: String, cx: &mut Context<Self>) {
        let address = Address {
            url: Some(link),
            compare: None,
            step: None,
        };
        crate::web::fetch(&address);
        cx.notify();
    }

    /// Keeps the address bar on the exports on screen and the playhead.
    /// Before a run is on screen the address the page was opened with
    /// stays, so a link that did not open can be tried again.
    pub(super) fn sync_address(&mut self) {
        if self.session.is_none() {
            return;
        }
        self.address_bar.show(self.step);
    }
}
