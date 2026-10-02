// Google Analytics 4 for rewindvm.dev, and the site's own events: which
// header links people follow, how far down the home page they get, which
// Buy button they press, which install command they copy, and the sale
// itself when Stripe sends the buyer back to the thank-you page. Every page
// loads gtag.js from Google next to this file. The desktop app and the
// engine send nothing anywhere; this is the website only.
(() => {
  "use strict";

  // The GA4 property the site reports to.
  const MEASUREMENT_ID = "G-L0515BFLG0";

  // Visitors in these regions (the EEA, the UK and Switzerland) get no
  // analytics cookies, since the site has no consent banner. Google
  // still receives a cookieless ping per page and estimates the rest.
  const NO_COOKIE_REGIONS = [
    "AT",
    "BE",
    "BG",
    "CH",
    "CY",
    "CZ",
    "DE",
    "DK",
    "EE",
    "ES",
    "FI",
    "FR",
    "GB",
    "GR",
    "HR",
    "HU",
    "IE",
    "IS",
    "IT",
    "LI",
    "LT",
    "LU",
    "LV",
    "MT",
    "NL",
    "NO",
    "PL",
    "PT",
    "RO",
    "SE",
    "SI",
    "SK",
  ];

  // Consent Mode states.
  const GRANTED = "granted";
  const DENIED = "denied";

  // Event names. begin_checkout is GA4's own name for starting a
  // purchase, so it lands in the monetization reports.
  const EVENT_NAV = "nav_click";
  const EVENT_SECTION = "view_section";
  const EVENT_CHECKOUT = "begin_checkout";
  const EVENT_COPY_INSTALL = "copy_install";
  const EVENT_PURCHASE = "purchase";

  // The license prices, in dollars, by data-edition on the Buy buttons.
  const CURRENCY = "USD";
  const PRICES = { personal: 49, commercial: 99 };

  // Home page sections whose first sight counts as an event.
  const WATCHED_SECTIONS = ["pricing", "download"];

  // How much of a section must be on screen to count as seen.
  const SEEN_SHARE = 0.25;

  // Stripe's Payment Links send the buyer to the thank-you page with the
  // checkout session's id and the edition bought in the query string.
  const THANKS_PATH = /\/thanks(\.html)?$/;
  const SESSION_PARAM = "session_id";
  const EDITION_PARAM = "edition";

  // The Buy links carry the visitor's GA client id to Stripe in this
  // parameter, so a sale can later be tied to the visit that made it.
  // Stripe takes letters, digits, dashes and underscores only, and GA's
  // client id has a dot, so anything else becomes a dash.
  const REFERENCE_PARAM = "client_reference_id";
  const NOT_REFERENCE_CHARS = /[^A-Za-z0-9_-]/g;
  const REFERENCE_SEPARATOR = "-";

  // Runs RUN once the page's elements exist; this file loads in <head>.
  function whenReady(run) {
    if (document.readyState === "loading") {
      document.addEventListener("DOMContentLoaded", run);
      return;
    }
    run();
  }

  // GA4's ecommerce fields for one license of EDITION.
  function license(edition) {
    return {
      currency: CURRENCY,
      value: PRICES[edition],
      items: [
        {
          item_id: edition,
          item_name: `Rewind VM ${edition}`,
          price: PRICES[edition],
        },
      ],
    };
  }

  // gtag.js reads its commands from this queue.
  window.dataLayer = window.dataLayer || [];
  function gtag() {
    window.dataLayer.push(arguments);
  }

  // Consent first, before any other command: never ad cookies, and no
  // analytics cookies in the regions above.
  gtag("consent", "default", {
    ad_storage: DENIED,
    ad_user_data: DENIED,
    ad_personalization: DENIED,
    analytics_storage: GRANTED,
  });
  gtag("consent", "default", {
    analytics_storage: DENIED,
    region: NO_COOKIE_REGIONS,
  });

  // Page views, with Google's advertising features off.
  gtag("js", new Date());
  gtag("config", MEASUREMENT_ID, {
    allow_google_signals: false,
    allow_ad_personalization_signals: false,
  });

  // Header links, on a wide screen and in the phone menu.
  document.addEventListener("click", (event) => {
    const link = event.target.closest(
      ".nav-links a, .nav-menu-links a, .page-links a",
    );
    if (!link) {
      return;
    }

    gtag("event", EVENT_NAV, { link_text: link.textContent.trim() });
  });

  // Buy buttons, with the edition and its price.
  document.addEventListener("click", (event) => {
    const button = event.target.closest("a[data-edition]");
    if (!button) {
      return;
    }

    gtag("event", EVENT_CHECKOUT, license(button.dataset.edition));
  });

  // The Buy links, once gtag.js knows the visitor's client id: pass it to
  // Stripe as the checkout's client reference.
  gtag("get", MEASUREMENT_ID, "client_id", (clientId) => {
    if (!clientId) {
      return;
    }

    const reference = String(clientId).replace(
      NOT_REFERENCE_CHARS,
      REFERENCE_SEPARATOR,
    );
    whenReady(() => {
      for (const link of document.querySelectorAll("a[data-edition]")) {
        const url = new URL(link.href);
        url.searchParams.set(REFERENCE_PARAM, reference);
        link.href = url.toString();
      }
    });
  });

  // The sale, on the thank-you page. The checkout session id is the
  // transaction id, so GA4 counts a reload of the page only once.
  const query = new URLSearchParams(window.location.search);
  const session = query.get(SESSION_PARAM);
  const bought = query.get(EDITION_PARAM);
  if (
    THANKS_PATH.test(window.location.pathname) &&
    session &&
    Object.hasOwn(PRICES, bought)
  ) {
    gtag("event", EVENT_PURCHASE, {
      transaction_id: session,
      ...license(bought),
    });
  }

  // Install commands: a copy from any block marked data-install counts,
  // whether by keyboard, menu or a selection.
  document.addEventListener("copy", () => {
    const selection = document.getSelection();
    if (!selection || selection.rangeCount === 0) {
      return;
    }

    const node = selection.getRangeAt(0).commonAncestorContainer;
    const element =
      node.nodeType === Node.ELEMENT_NODE ? node : node.parentElement;
    const block = element && element.closest("[data-install]");
    if (!block) {
      return;
    }

    gtag("event", EVENT_COPY_INSTALL, { method: block.dataset.install });
  });

  // Sections on the home page, once each per visit, however the reader
  // got there: a header link, a Download button or scrolling.
  if (!("IntersectionObserver" in window)) {
    return;
  }
  const observer = new IntersectionObserver(
    (entries) => {
      for (const entry of entries) {
        if (!entry.isIntersecting) {
          continue;
        }

        gtag("event", EVENT_SECTION, { section: entry.target.id });
        observer.unobserve(entry.target);
      }
    },
    { threshold: SEEN_SHARE },
  );
  whenReady(() => {
    for (const id of WATCHED_SECTIONS) {
      const section = document.getElementById(id);
      if (!section) {
        continue;
      }

      observer.observe(section);
    }
  });
})();
