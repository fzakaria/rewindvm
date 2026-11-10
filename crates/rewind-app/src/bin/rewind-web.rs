//! `rewind-web`: the desktop app's window in a browser, as a read-only
//! viewer of `.rwd` exports, for the site's viewer page. Built for
//! wasm32-unknown-unknown and loaded through wasm-bindgen's glue by
//! site/viewer/index.html, which calls [`start`] with its address and
//! hands the app the exports it fetches and the files it reads. On the
//! desktop the binary only says so.

#![cfg_attr(target_family = "wasm", no_main)]

#[cfg(not(target_family = "wasm"))]
fn main() {
    eprintln!("rewind-web runs in a browser: build it with --target wasm32-unknown-unknown");
}

#[cfg(target_family = "wasm")]
mod web {
    use std::sync::Arc;

    use anyhow::Result;
    use rewind_app::address::Address;
    use rewind_app::archive;
    use rewind_app::engine::CliEngine;
    use rewind_app::run::{InMemoryOrigin, Run, Session};
    use rewind_app::ui::{self, Launch, RightColumn};
    use rewind_app::web::{self as page, Delivery};
    use wasm_bindgen::prelude::*;

    /// Keeps a font the page fetched, for the app to load when it starts.
    /// The page hands every font over before it calls [`start`].
    #[wasm_bindgen]
    pub fn add_font(bytes: Vec<u8>) {
        page::add_font(bytes);
    }

    /// Opens the window on the page's canvas, with the start screen, and
    /// asks the page for the exports `query`, the page's address, names.
    #[wasm_bindgen]
    pub fn start(query: String) {
        console_error_panic_hook::set_once();
        gpui_web::init_logging();

        // The engine is the desktop's command, which the browser cannot
        // start; the app says so of everything that needs it.
        ui::run(Launch {
            session: None,
            step: None,
            right: RightColumn::AtStep,
            engine: Arc::new(CliEngine::from_env()),
        });
        page::fetch(&Address::parse(&query));
    }

    /// What the first bytes of an export hold, as [`export_prefix`] tells
    /// the page: `archive::Prefix`, in a form wasm-bindgen hands to JS.
    #[wasm_bindgen]
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Prefix {
        /// The run opens from the bytes.
        Whole,
        /// More of the export is needed.
        Short,
        /// The bytes are not an export, however many more come.
        Broken,
    }

    impl From<archive::Prefix> for Prefix {
        fn from(prefix: archive::Prefix) -> Prefix {
            match prefix {
                archive::Prefix::Whole => Prefix::Whole,
                archive::Prefix::Short => Prefix::Short,
                archive::Prefix::Broken => Prefix::Broken,
            }
        }
    }

    /// Whether `bytes`, the first bytes of an export the page is fetching
    /// or reading, hold the run: the page stops reading once they do, so
    /// the memory and inputs behind the trace in a replayable export
    /// never come.
    #[wasm_bindgen]
    pub fn export_prefix(bytes: &[u8]) -> Prefix {
        archive::prefix(bytes).into()
    }

    /// Opens the exports the page fetched for an address: `run` from
    /// `url`, compared with `compare` from `compare_url`, at `step`.
    #[wasm_bindgen]
    pub fn open_fetched(
        run: Vec<u8>,
        url: String,
        compare: Option<Vec<u8>>,
        compare_url: Option<String>,
        step: Option<f64>,
    ) {
        let address = Address {
            url: Some(url),
            compare: compare_url,
            step: step.map(|step| step as u64),
        };
        // The fetch the page answers, by the links the fetch was given.
        let request = page::fetched(
            address.url.as_deref().unwrap_or_default(),
            address.compare.as_deref(),
        );
        let opened = session_of(&address, &run, compare.as_deref());
        page::deliver(match opened {
            Ok(session) => Delivery::Open {
                session: Box::new(session),
                address,
                request,
            },
            Err(e) => Delivery::Unread {
                name: name_of(address.url.as_deref().unwrap_or_default()).to_string(),
                why: format!("{e:#}"),
                request,
            },
        });
    }

    /// Says the page starts reading the file `name`, picked or dropped,
    /// and returns the token to pass to [`open_file`] with its bytes. A
    /// page that calls this before it reads the file lets the app tell
    /// which of two reads, or of a read and a fetch, the user asked for
    /// last.
    #[wasm_bindgen]
    pub fn begin_file(name: String) -> u32 {
        page::file_started(&name)
    }

    /// Opens a file the page read, picked or dropped: it has no address to
    /// share. `token` is what [`begin_file`] gave for the read. Without
    /// one, the file counts as asked for when its bytes arrive: newer than
    /// every fetch and read started before.
    #[wasm_bindgen]
    pub fn open_file(name: String, bytes: Vec<u8>, token: Option<u32>) {
        let request = page::file_read(token);
        let opened = Run::open_bytes(&name, &bytes, InMemoryOrigin::Export);
        page::deliver(match opened {
            Ok(run) => Delivery::Open {
                session: Box::new(Session::new(run, None)),
                address: Address::default(),
                request,
            },
            Err(e) => Delivery::Unread {
                name,
                why: format!("{e:#}"),
                request,
            },
        });
    }

    /// Says the page could not read the file `name` it started under
    /// `token`, or could not take an export out of it: `why` is the
    /// page's reason, shown as it is. Without this call the app would
    /// show the read as still going.
    #[wasm_bindgen]
    pub fn unread_file(name: String, why: String, token: Option<u32>) {
        let request = page::file_read(token);
        page::deliver(Delivery::Unread { name, why, request });
    }

    /// Says the page could not fetch `url`: the server's answer, or None
    /// when the browser got none.
    #[wasm_bindgen]
    pub fn unfetched(url: String, answer: Option<String>) {
        let request = page::unfetched(&url);
        page::deliver(Delivery::Unfetched {
            url,
            answer,
            request,
        });
    }

    /// The session of the exports an address names.
    fn session_of(address: &Address, run: &[u8], compare: Option<&[u8]>) -> Result<Session> {
        let url = address.url.as_deref().unwrap_or_default();
        let run = Run::open_bytes(name_of(url), run, InMemoryOrigin::Export)?;
        let other = match (address.compare.as_deref(), compare) {
            (Some(url), Some(bytes)) => Some(Run::open_bytes(
                name_of(url),
                bytes,
                InMemoryOrigin::Export,
            )?),
            _ => None,
        };
        Ok(Session::new(run, other))
    }

    /// The file name a URL ends in, without its query or fragment.
    fn name_of(url: &str) -> &str {
        let path = url.split(['?', '#']).next().unwrap_or(url);
        path.rsplit('/').next().unwrap_or(path)
    }
}
