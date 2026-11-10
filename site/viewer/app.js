// The viewer page: the desktop app's window, built for the browser
// (crates/rewind-app/src/bin/rewind-web.rs), on a canvas below the site's
// header. The app has no file chooser, no way to fetch and no address bar
// of its own; this page does those for it, through window.rewindPage,
// which crates/rewind-app/src/web.rs calls.
//
//     viewer/?url=<rwd>&compare=<rwd>&step=<n>&tab=<tab>&thread=<tid>&search=<text>
//
// opens an export, compared with another, at a step, on a tab of the
// right column, with a thread picked in the process tree and text in the
// search box; crates/rewind-app/src/address.rs reads and writes the
// address, and only the first part is needed. Relative links resolve
// against this page, so the examples beside it open as
// ?url=examples/philosophers.rwd.

import { ZipRefusal, isZip, rwdFromZip } from "./zip.js";

// The app's module, loaded once the frame is wide enough for the app, or
// on a narrower screen to summarize the run a link names. A phone opening
// the page without a link never downloads it.
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

// What the page says while it reads the run to summarize, and before the
// reason it could not.
const SUMMARY_READING = "Reading the recording\u2026";
const SUMMARY_UNREAD = "The recording did not open here:";

// What the page says once the app's code has stopped.
const STOPPED_NOTE =
  "The viewer stopped on an error. Reload the page to start it again.";

// How much of an export the page reads before it first asks the app
// whether the bytes so far hold the run, and by how much the bytes grow
// between one asking and the next. The manifest, the trace and the
// bookmarks come first in an export, before the memory and inputs that
// only a replay needs, which are most of a replayable export; asking at
// each doubling reads a run for at most twice its bytes and a few
// askings.
const FIRST_PROBE_BYTES = 64 * 1024;
const PROBE_GROWTH = 2;

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
const summaryBox = document.getElementById("viewer-summary");
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

// Calls into the app and gives back what the call returns. A call that
// throws is the app's code stopping.
function callApp(call) {
  if (phase !== Phase.Running) {
    return;
  }
  try {
    return call();
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

// The response to a fetch of a link. A server's error answer comes back
// as its status; a fetch the browser refused, as a server without CORS
// headers gets, as null.
async function fetchLink(url) {
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
  return { response, answer };
}

// The bytes at a link, fetched whole. A body that stops short comes back
// as the server's status and what happened.
async function fetchBytes(url) {
  const fetched = await fetchLink(url);
  if (!fetched.response) {
    return fetched;
  }
  try {
    return { bytes: new Uint8Array(await fetched.response.arrayBuffer()) };
  } catch {
    return { answer: `${fetched.answer}, ${DROPPED_ANSWER}` };
  }
}

// One array of the bytes in `chunks`.
function joined(chunks) {
  if (chunks.length === 1) {
    return chunks[0];
  }
  const bytes = new Uint8Array(
    chunks.reduce((n, chunk) => n + chunk.length, 0),
  );
  let at = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, at);
    at += chunk.length;
  }
  return bytes;
}

// Whether the first bytes of an export end before the run does, as the
// running app's code says through the page's call into it.
const appNeedsMore = (bytes) =>
  callApp(() => web.export_prefix(bytes)) === web.Prefix.Short;

// Reads a run from `source`, which hands back the next bytes of an
// export, or null at its end, and stops the read when told to. The read
// goes on until `needsMore`, which asks the app's code, says the bytes so
// far hold the run (see FIRST_PROBE_BYTES), so the memory and inputs behind the trace
// in a replayable export are never read; a source that ends first gives
// all its bytes, for the app to say what they are. A zip is read whole,
// since its directory is at its end.
async function readRun(source, needsMore) {
  const chunks = [];
  let length = 0;
  let probeAt = FIRST_PROBE_BYTES;
  for (;;) {
    const chunk = await source.read(probeAt - length);
    if (chunk === null) {
      break;
    }
    chunks.push(chunk);
    length += chunk.length;
    if (length < probeAt) {
      continue;
    }
    const bytes = joined(chunks);
    chunks.length = 0;
    chunks.push(bytes);
    if (isZip(bytes)) {
      probeAt = Infinity;
      continue;
    }
    if (needsMore(bytes)) {
      probeAt = length * PROBE_GROWTH;
      continue;
    }
    await source.stop();
    break;
  }
  return joined(chunks);
}

// A response's body as readRun reads one: the bytes as they arrive, and
// a stop that ends the download.
function streamed(body) {
  const reader = body.getReader();
  return {
    read: async () => {
      const { value, done } = await reader.read();
      return done ? null : value;
    },
    stop: () => reader.cancel().catch(() => {}),
  };
}

// A file as readRun reads one: a slice of the asked size at a time, so a
// read that stops leaves the rest of the file on disk.
function sliced(file) {
  let at = 0;
  return {
    read: async (want) => {
      if (at >= file.size) {
        return null;
      }
      const end = Math.min(file.size, at + want);
      const bytes = new Uint8Array(await file.slice(at, end).arrayBuffer());
      at = end;
      return bytes;
    },
    stop: async () => {},
  };
}

// The export at a link: the link's bytes, as far as the run needs, or the
// .rwd file inside them when they are a zip. A link the browser would
// refuse is not tried, a body that stops short comes back as the server's
// status and what happened, and a zip that is not opened comes back as
// the reason, each in the place of a server's answer. `needsMore` is as
// readRun takes it.
async function fetchExport(url, needsMore) {
  if (isMixedContent(url)) {
    return { answer: MIXED_ANSWER };
  }
  const fetched = await fetchLink(url);
  if (!fetched.response) {
    return fetched;
  }
  let bytes;
  try {
    bytes = await readRun(streamed(fetched.response.body), needsMore);
  } catch {
    return { answer: `${fetched.answer}, ${DROPPED_ANSWER}` };
  }
  if (!isZip(bytes)) {
    return { bytes };
  }
  try {
    const inside = await rwdFromZip(bytes);
    return { bytes: inside.bytes };
  } catch (error) {
    if (error instanceof ZipRefusal) {
      return { answer: error.reason };
    }
    throw error;
  }
}

// Fetches the two exports the app asks for and hands them back under
// their links, or tells the app which one did not come. The app keeps the
// rest of the address, the view to open them on, with the fetch.
async function fetchRuns(url, compare) {
  say(null);
  const [run, other] = await Promise.all([
    fetchExport(url, appNeedsMore),
    compare ? fetchExport(compare, appNeedsMore) : null,
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
    web.open_fetched(run.bytes, url, other?.bytes, compare ?? undefined),
  );
}

// Hands a picked or dropped file to the app, read in the browser as far
// as the run needs. A zip is opened as the .rwd file inside it. A file
// that comes while the app loads is held until the app is on screen. The
// app numbers the read when it starts, so of two files, or a file and a
// fetch, the one asked for last wins however their bytes arrive; the
// token says which read the bytes, or the reason there are none, belong
// to.
async function openFile(file) {
  if (phase === Phase.Starting) {
    heldFile = file;
    return;
  }
  say(null);
  const token = callApp(() => web.begin_file(file.name));

  // The file's bytes, which the browser can fail to read, as when the
  // file was removed after it was picked.
  let bytes;
  try {
    bytes = await readRun(sliced(file), appNeedsMore);
  } catch {
    callApp(() =>
      web.unread_file(
        file.name,
        `The browser could not read ${file.name}.`,
        token,
      ),
    );
    return;
  }

  // A plain export goes to the app as it is.
  if (!isZip(bytes)) {
    callApp(() => web.open_file(file.name, bytes, token));
    return;
  }

  // A zip gives the export inside it, or the reason it is not opened.
  let inside;
  try {
    inside = await rwdFromZip(bytes);
  } catch (error) {
    if (error instanceof ZipRefusal) {
      callApp(() =>
        web.unread_file(file.name, `${file.name} is ${error.reason}.`, token),
      );
      return;
    }
    throw error;
  }
  callApp(() => web.open_file(inside.name, inside.bytes, token));
}

// The address follows what is on screen once the playhead, the tab, the
// thread and the search rest, without a history entry for every step or
// every letter typed.
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
      loadModule(),
      ...FONTS.map(fetchFont),
    ]);
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

// The app's module, loaded and set up, without starting the app: what the
// app needs before start, and all the summary needs.
async function loadModule() {
  const module = await import("./pkg/rewind-web.js");
  await module.default();
  return module;
}

// On a screen too narrow for the app, the run the page's link names, in
// plain text below the note: the export's summary.txt, or the summary the
// app's code makes from its manifest and trace, beside the compared run
// when the link names one. Only the bytes the run needs are fetched, as
// the app fetches them. Anything that goes wrong is said in the summary's
// place.
async function showSummary() {
  const query = new URLSearchParams(location.search);
  const url = query.get("url");
  if (!url) {
    return;
  }
  const compare = query.get("compare");
  summaryBox.textContent = SUMMARY_READING;
  summaryBox.hidden = false;
  try {
    const module = await loadModule();
    const needsMore = (bytes) =>
      module.export_prefix(bytes) === module.Prefix.Short;
    const [run, other] = await Promise.all([
      fetchExport(url, needsMore),
      compare ? fetchExport(compare, needsMore) : null,
    ]);
    const missing = [
      [url, run],
      [compare, other],
    ].find(([, got]) => got && !got.bytes);
    if (missing) {
      const [link, got] = missing;
      summaryBox.textContent = `${SUMMARY_UNREAD} ${link} answered ${got.answer ?? "nothing"}.`;
      return;
    }
    summaryBox.textContent = module.summary(run.bytes, other?.bytes);
  } catch (error) {
    summaryBox.textContent = `${SUMMARY_UNREAD} ${error}`;
  }
}

// The app starts once the frame is as wide as its narrowest layout: at
// once on a computer, or when a narrow window is widened. Until then the
// note says why it is not there, above the summary of the run the link
// names.
let summarized = false;
const watchWidth = new ResizeObserver(() => {
  if (app.clientWidth < APP_MIN_WIDTH) {
    narrowNote.hidden = false;
    status.hidden = true;
    if (!summarized) {
      summarized = true;
      showSummary();
    }
    return;
  }
  watchWidth.disconnect();
  startApp();
});
watchWidth.observe(app);
