//! The browser build's link to its page, site/viewer/index.html: what the
//! page hands the app, and what the app asks of the page.
//!
//! The page fetches exports and reads dropped and picked files, since
//! gpui's web platform has no file chooser and drops no files into the
//! app. It hands the bytes to the exports of `rewind-web`, which read them
//! into a session and deliver it here, to the scrubber's inbox. The
//! scrubber asks the page, through `window.rewindPage`, to pick a file, to
//! fetch a link, and to show the address of what is on screen.

use std::cell::RefCell;

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use wasm_bindgen::prelude::*;

use crate::address::{Address, Fetches};
use crate::request::Request;
use crate::run::Session;

/// What the page hands the app. Each delivery carries the number of the
/// fetch or file open it belongs to: the scrubber drops a delivery older
/// than the newest it has seen, so a fetch that answers late does not
/// replace a run opened after the fetch started.
pub enum Delivery {
    /// The page started fetching the export at `url`.
    Fetching { url: String, request: Request },
    /// Runs the page fetched or read from a file, with the address that
    /// names them, which is empty for a file. Boxed: a session is large
    /// beside the other deliveries.
    Open {
        session: Box<Session>,
        address: Address,
        request: Request,
    },
    /// The page could not fetch `url`: the server's answer, such as
    /// "404 Not Found", or None when the browser got none, as it gets
    /// none from a server without CORS headers.
    Unfetched {
        url: String,
        answer: Option<String>,
        request: Request,
    },
    /// Why the bytes of `name` are not an export.
    Unread {
        name: String,
        why: String,
        request: Request,
    },
}

impl Delivery {
    /// The number of the fetch or file open the delivery belongs to.
    pub fn request(&self) -> Request {
        match self {
            Delivery::Fetching { request, .. }
            | Delivery::Open { request, .. }
            | Delivery::Unfetched { request, .. }
            | Delivery::Unread { request, .. } => *request,
        }
    }
}

/// The deliveries the scrubber has not taken yet. The page can deliver
/// before the window opens, so the channel is made before either end is
/// used, and the scrubber takes the receiving end once.
struct Inbox {
    sender: UnboundedSender<Delivery>,
    receiver: Option<UnboundedReceiver<Delivery>>,
}

thread_local! {
    static INBOX: RefCell<Inbox> = {
        let (sender, receiver) = unbounded();
        RefCell::new(Inbox {
            sender,
            receiver: Some(receiver),
        })
    };

    /// The page's fetches that have not answered, and where fetches and
    /// file opens get their numbers.
    static FETCHES: RefCell<Fetches> = RefCell::new(Fetches::default());

    /// The fonts the page fetched, until the app starts and loads them.
    static FONTS: RefCell<Vec<Vec<u8>>> = const { RefCell::new(Vec::new()) };
}

/// Hands `delivery` to the scrubber.
pub fn deliver(delivery: Delivery) {
    INBOX.with(|inbox| {
        let _ = inbox.borrow().sender.unbounded_send(delivery);
    });
}

/// The receiving end of the inbox, for the scrubber; None after the first
/// call.
pub fn take_inbox() -> Option<UnboundedReceiver<Delivery>> {
    INBOX.with(|inbox| inbox.borrow_mut().receiver.take())
}

/// Keeps a font the page fetched, for the app to load when it starts.
pub fn add_font(bytes: Vec<u8>) {
    FONTS.with(|fonts| fonts.borrow_mut().push(bytes));
}

/// The fonts the page fetched, taken once.
pub fn take_fonts() -> Vec<Vec<u8>> {
    FONTS.with(|fonts| std::mem::take(&mut *fonts.borrow_mut()))
}

/// The page's side, as site/viewer/index.html defines it.
#[wasm_bindgen]
extern "C" {
    /// Opens the browser's file chooser for a .rwd file.
    #[wasm_bindgen(js_namespace = rewindPage, js_name = pickFile)]
    fn page_pick_file();

    /// Fetches the export at `url`, and the one at `compare` when given,
    /// and hands them back with the step to show.
    #[wasm_bindgen(js_namespace = rewindPage, js_name = fetchRuns)]
    fn page_fetch_runs(url: &str, compare: Option<String>, step: Option<f64>);

    /// Puts `query` in the address bar, without a new history entry.
    #[wasm_bindgen(js_namespace = rewindPage, js_name = showAddress)]
    fn page_show_address(query: &str);
}

/// Asks the page for a file to open.
pub fn pick_file() {
    page_pick_file();
}

/// The number of the fetch of `url` and `compare` the page now hands the
/// exports of.
pub fn fetched(url: &str, compare: Option<&str>) -> Request {
    FETCHES.with(|fetches| fetches.borrow_mut().fetched(url, compare))
}

/// The number of the fetch that could not fetch `url`.
pub fn unfetched(url: &str) -> Request {
    FETCHES.with(|fetches| fetches.borrow_mut().unfetched(url))
}

/// The number of a file the page read and now hands over.
pub fn file_read() -> Request {
    FETCHES.with(|fetches| fetches.borrow_mut().file())
}

/// Asks the page to fetch the exports `address` names.
pub fn fetch(address: &Address) {
    let Some(url) = &address.url else {
        return;
    };
    let request = FETCHES.with(|fetches| fetches.borrow_mut().start(address));
    deliver(Delivery::Fetching {
        url: url.clone(),
        request,
    });
    page_fetch_runs(
        url,
        address.compare.clone(),
        address.step.map(|step| step as f64),
    );
}

/// The address bar, kept on what is on screen.
#[derive(Default)]
pub struct AddressBar {
    /// The exports on screen, as the address named them.
    pub address: Address,
    /// The query last shown, so an unchanged one is not shown again; None
    /// until a run is on screen, so the first one is always shown, over
    /// whatever address the page was opened with.
    shown: Option<String>,
}

impl AddressBar {
    /// Shows the address of the exports on screen with the playhead at
    /// `step`; the page waits for the playhead to rest before it does.
    pub fn show(&mut self, step: u64) {
        let address = Address {
            step: Some(step),
            ..self.address.clone()
        };
        let query = address.query();
        if self.shown.as_ref() == Some(&query) {
            return;
        }
        page_show_address(&query);
        self.shown = Some(query);
    }
}
