// The viewer page: the desktop app's window, built for the browser
// (crates/rewind-app/src/bin/rewind-web.rs), on a canvas below the site's
// header. The app has no file chooser, no way to fetch and no address bar
// of its own; this page does those for it, through window.rewindPage,
// which crates/rewind-app/src/web.rs calls.
//
//     viewer/?url=<rwd>&compare=<rwd>&step=<n>
//
// opens an export, compared with another, at a step. Relative links
// resolve against this page, so the examples beside it open as
// ?url=examples/philosophers.rwd.

import { ZipRefusal, isZip, rwdFromZip } from "./zip.js";

// The app's module, loaded only once the frame is wide enough for it, so a
// phone never downloads it.
let web = null;

// Where the app is: not asked for yet, loading, on screen, or stopped by
// an error while it loaded or in its own code, which only a reload undoes.
const Phase = {
  Idle: "idle",
  Starting: "starting",
  Running: "running",
  Stopped: "stopped",
};
let phase = Phase.Idle;

// A file picked or dropped while the app was loading, opened once the app
// is on screen.
let heldFile = null;

// The fonts the app is set in, which the web platform needs as files: it
// starts with none, and cannot read the system's or the stylesheet's.
const FONTS = [
  "fonts/IBMPlexSans-Regular.otf",
  "fonts/IBMPlexSans-Italic.otf",
  "fonts/IBMPlexSans-SemiBold.otf",
  "fonts/IBMPlexSans-Bold.otf",
  "fonts/JetBrainsMono-Regular.ttf",
  "fonts/JetBrainsMono-Italic.ttf",
  "fonts/JetBrainsMono-Bold.ttf",
];

// How long the playhead rests before the address follows it.
const ADDRESS_DELAY_MS = 300;

// The narrowest layout the app has. A frame narrower than this when the
// page opens shows a note instead of the app; one narrowed after the app
// started scrolls sideways.
const APP_MIN_WIDTH = 960;

// What the page says once the app's code has stopped.
const STOPPED_NOTE =
  "The viewer stopped on an error. Reload the page to start it again.";

// The schemes of a page served securely and of a link that is not. A
// browser refuses to fetch the second from the first, as mixed content,
// and tells the page nothing of why.
const SECURE_SCHEME = "https:";
const PLAIN_SCHEME = "http:";

// The hosts a browser fetches over plain http even from a secure page:
// the visitor's own machine.
const LOOPBACK_HOSTS = ["localhost", "[::1]"];
const LOOPBACK_DOMAIN = ".localhost";
const LOOPBACK_IPV4 = /^127(\.\d{1,3}){3}$/;

// What the page tells the app of a link it did not fetch or could not
// read whole. The app shows each after the link and "answered".
const MIXED_ANSWER =
  "nothing, because a browser does not fetch an http link from an https page. Use an https link";
const DROPPED_ANSWER = "then the connection dropped";

const header = document.getElementById("site-header");
const narrowNote = document.getElementById("viewer-narrow");
const app = document.getElementById("viewer-app");
const frame = document.getElementById("viewer-frame");
const status = document.getElementById("viewer-status");
const notice = document.getElementById("viewer-notice");
const pick = document.getElementById("pick");
const veil = document.getElementById("drop-veil");

// The window fills the viewport below the header, however tall it wraps,
// and below the page's notice while one shows.
function fitBelowHeader() {
  let top = header.offsetHeight;
  if (!notice.hidden) {
    const gap = parseFloat(getComputedStyle(notice).marginBottom);
    top += notice.offsetHeight + gap;
  }
  document.documentElement.style.setProperty("--viewer-top", `${top}px`);
}
new ResizeObserver(fitBelowHeader).observe(document.body);
fitBelowHeader();

// The page's word on the app goes away once the app has something to
// show, unless that word is that the app stopped.
function clearStatus() {
  if (phase !== Phase.Stopped) {
    status.hidden = true;
  }
}

// The app appends its canvas, and any message that the browser cannot
// draw, to the body; they go into the frame. On a touch screen narrower
// than the app, a sideways swipe scrolls the frame instead of reaching
// the app.
function adopt(node) {
  if (node instanceof HTMLCanvasElement) {
    frame.appendChild(node);
    clearStatus();
    const narrow = app.clientWidth < APP_MIN_WIDTH;
    if (narrow && matchMedia("(pointer: coarse)").matches) {
      node.style.touchAction = "pan-x";
    }
    return;
  }
  if (
    node instanceof HTMLParagraphElement &&
    node.parentElement === document.body
  ) {
    clearStatus();
    frame.appendChild(node);
  }
}
new MutationObserver((changes) => {
  for (const change of changes) {
    change.addedNodes.forEach(adopt);
  }
}).observe(document.body, { childList: true });

// Says `words` above the window, for what the app has no way to say: why
// the page did not hand it a file. Null takes the notice away.
function say(words) {
  notice.textContent = words ?? "";
  notice.hidden = words === null;
  fitBelowHeader();
}

// The app's code stopped: a panic in it ends every later call too, and
// its canvas stays as it was last drawn. The page says so over the canvas
// and hands the app nothing more.
function showStopped(error) {
  console.error(error);
  phase = Phase.Stopped;
  heldFile = null;
  veil.hidden = true;
  status.textContent = STOPPED_NOTE;
  status.classList.add("stopped");
  status.hidden = false;
}

// Calls into the app. A call that throws is the app's code stopping.
function callApp(call) {
  if (phase !== Phase.Running) {
    return;
  }
  try {
    call();
  } catch (error) {
    showStopped(error);
  }
}

// A panic outside any call the page makes, in a frame or an event the app
// handles, reaches the window as a trap in the app's WebAssembly. Errors
// of other kinds are not the app stopping, and are left alone.
function watchForStop(error) {
  if (error instanceof WebAssembly.RuntimeError) {
    showStopped(error);
  }
}
window.addEventListener("error", (event) => watchForStop(event.error));
window.addEventListener("unhandledrejection", (event) =>
  watchForStop(event.reason),
);

// Whether a browser refuses to fetch `url` from this page as mixed
// content: a plain http link, not to this machine, from a secure page.
function isMixedContent(url) {
  let target;
  try {
    target = new URL(url, location.href);
  } catch {
    return false;
  }
  if (location.protocol !== SECURE_SCHEME || target.protocol !== PLAIN_SCHEME) {
    return false;
  }
  const host = target.hostname;
  const loopback =
    LOOPBACK_HOSTS.includes(host) ||
    host.endsWith(LOOPBACK_DOMAIN) ||
    LOOPBACK_IPV4.test(host);
  return !loopback;
}

// The bytes at a link, fetched whole. A server's error answer comes back
// as its status, and a body that stops short as its status and what
// happened; a fetch the browser refused, as a server without CORS headers
// gets, as null.
async function fetchBytes(url) {
  let response;
  try {
    response = await fetch(new URL(url, location.href));
  } catch {
    return { answer: null };
  }
  const answer = `${response.status} ${response.statusText}`.trim();
  if (!response.ok) {
    return { answer };
  }
  try {
    return { bytes: new Uint8Array(await response.arrayBuffer()) };
  } catch {
    return { answer: `${answer}, ${DROPPED_ANSWER}` };
  }
}

// The export at a link: the link's bytes, or the .rwd file inside them
// when they are a zip. A link the browser would refuse is not tried, and
// a zip that is not opened comes back as the reason, both in the place of
// a server's answer.
async function fetchExport(url) {
  if (isMixedContent(url)) {
    return { answer: MIXED_ANSWER };
  }
  const fetched = await fetchBytes(url);
  if (!fetched.bytes || !isZip(fetched.bytes)) {
    return fetched;
  }
  try {
    const inside = await rwdFromZip(fetched.bytes);
    return { bytes: inside.bytes };
  } catch (error) {
    if (error instanceof ZipRefusal) {
      return { answer: error.reason };
    }
    throw error;
  }
}

// Fetches the exports an address names and hands them to the app, or
// tells it which one did not come.
async function fetchRuns(url, compare, step) {
  say(null);
  const [run, other] = await Promise.all([
    fetchExport(url),
    compare ? fetchExport(compare) : null,
  ]);
  if (!run.bytes) {
    callApp(() => web.unfetched(url, run.answer ?? undefined));
    return;
  }
  if (other && !other.bytes) {
    callApp(() => web.unfetched(compare, other.answer ?? undefined));
    return;
  }
  callApp(() =>
    web.open_fetched(
      run.bytes,
      url,
      other?.bytes,
      compare ?? undefined,
      step ?? undefined,
    ),
  );
}

// Hands a picked or dropped file to the app, read in the browser. A zip
// is opened as the .rwd file inside it. A file that comes while the app
// loads is held until the app is on screen.
async function openFile(file) {
  if (phase === Phase.Starting) {
    heldFile = file;
    return;
  }
  say(null);

  // The file's bytes, which the browser can fail to read, as when the
  // file was removed after it was picked.
  let bytes;
  try {
    bytes = new Uint8Array(await file.arrayBuffer());
  } catch {
    say(`The browser could not read ${file.name}.`);
    return;
  }

  // A plain export goes to the app as it is.
  if (!isZip(bytes)) {
    callApp(() => web.open_file(file.name, bytes));
    return;
  }

  // A zip gives the export inside it, or the reason it is not opened.
  let inside;
  try {
    inside = await rwdFromZip(bytes);
  } catch (error) {
    if (error instanceof ZipRefusal) {
      say(`${file.name} is ${error.reason}.`);
      return;
    }
    throw error;
  }
  callApp(() => web.open_file(inside.name, inside.bytes));
}

// The address follows what is on screen once the playhead rests, without
// a history entry for every step.
let addressTimer;
function showAddress(query) {
  clearTimeout(addressTimer);
  addressTimer = setTimeout(() => {
    history.replaceState(
      null,
      "",
      `${location.pathname}${query}${location.hash}`,
    );
  }, ADDRESS_DELAY_MS);
}

window.rewindPage = {
  pickFile: () => pick.click(),
  fetchRuns,
  showAddress,
};

pick.addEventListener("change", () => {
  const [file] = pick.files;
  pick.value = "";
  if (file) {
    openFile(file);
  }
});

// A .rwd file dropped anywhere on the page opens. The app's canvas takes
// drops for itself and discards their files, so the page listens first.
// A dragged file is always taken from the browser, which would otherwise
// leave the page to show the file; the page opens the file only while the
// app is loading or on screen.
const dragsFiles = (event) => event.dataTransfer?.types.includes("Files");
const takesFiles = () => phase === Phase.Starting || phase === Phase.Running;
window.addEventListener(
  "dragover",
  (event) => {
    if (!dragsFiles(event)) {
      return;
    }
    event.preventDefault();
    veil.hidden = !takesFiles();
  },
  { capture: true },
);
window.addEventListener(
  "dragleave",
  (event) => {
    if (event.relatedTarget === null) {
      veil.hidden = true;
    }
  },
  { capture: true },
);
window.addEventListener(
  "drop",
  (event) => {
    if (!dragsFiles(event)) {
      return;
    }
    event.preventDefault();
    event.stopPropagation();
    veil.hidden = true;
    const [file] = event.dataTransfer.files;
    if (file && takesFiles()) {
      openFile(file);
    }
  },
  { capture: true },
);

// The fonts load beside the app's code; one that does not come only costs
// itself, as the browser draws what is missing with its own.
async function fetchFont(url) {
  const fetched = await fetchBytes(url);
  return fetched.bytes ?? null;
}

// Loads the app's code and fonts and starts it on the page's address,
// then opens the file that came while it loaded, if one did.
async function startApp() {
  narrowNote.hidden = true;
  status.hidden = false;
  frame.classList.add("running");
  phase = Phase.Starting;
  try {
    const [module, ...fonts] = await Promise.all([
      import("./pkg/rewind-web.js"),
      ...FONTS.map(fetchFont),
    ]);
    await module.default();
    for (const font of fonts) {
      if (font) {
        module.add_font(font);
      }
    }
    web = module;
    web.start(location.search);
  } catch (error) {
    phase = Phase.Stopped;
    heldFile = null;
    status.hidden = false;
    status.textContent = `The viewer did not start: ${error}`;
    return;
  }

  // An error while the app loaded has already stopped it.
  if (phase !== Phase.Starting) {
    return;
  }
  phase = Phase.Running;
  if (heldFile) {
    const file = heldFile;
    heldFile = null;
    openFile(file);
  }
}

// The app starts once the frame is as wide as its narrowest layout: at
// once on a computer, or when a narrow window is widened. Until then the
// note says why it is not there.
const watchWidth = new ResizeObserver(() => {
  if (app.clientWidth < APP_MIN_WIDTH) {
    narrowNote.hidden = false;
    status.hidden = true;
    return;
  }
  watchWidth.disconnect();
  startApp();
});
watchWidth.observe(app);
