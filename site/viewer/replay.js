// Replays a step of an export in the browser, for the app's file viewer
// at steps the export carries no answer for (crates/rewind-app/src/web.rs
// replay_file). The machine is Rewind's QEMU machine built as WebAssembly
// (qemu/, tools/build-qemu-wasm.sh), a module of its own under qemu/
// beside this page, which shares nothing with the app's module: this page
// hands it files and takes back its step log.
//
// Beside an export X.rwd, X.replay/ holds what `rewind-qemu RUN --bundle`
// writes: manifest.json, with QEMU's arguments and the steps of the run's
// keyframes, state-<step>.gz for each keyframe, the schedule, and the
// image as the pages replays of the run read, image.sparse.gz. A replay
// that reads a page the bundle left out stops and says so.
// A replay starts from the latest keyframe at or before the step, runs
// to the step, asks the guest what the app asks, and stops once the
// guest has answered.

// Where the engine's files are, beside this page.
const ENGINE = new URL("qemu/out.js", import.meta.url).href;

// The placeholder the bundle's arguments hold for the step.
const UNTIL = "{until}";

// What is fetched once per bundle and kept for later replays: the
// manifest, the image and the schedule, by the bundle's address.
const bundles = new Map();

// The bundle beside the export at `url`.
function bundleOf(url) {
  const base = new URL(url, location.href);
  base.pathname = base.pathname.replace(/\.rwd$/, "") + ".replay/";
  return base.href;
}

async function fetchBytes(url) {
  const response = await fetch(url);
  if (!response.ok) {
    throw new Error(`${url}: ${response.status} ${response.statusText}`);
  }
  return new Uint8Array(await response.arrayBuffer());
}

// A gzipped file of the bundle, unpacked with the browser's own gzip.
async function fetchGzipped(url) {
  const response = await fetch(url);
  if (!response.ok) {
    throw new Error(`${url}: ${response.status} ${response.statusText}`);
  }
  const unpacked = response.body.pipeThrough(new DecompressionStream("gzip"));
  return new Uint8Array(await new Response(unpacked).arrayBuffer());
}

async function loadBundle(base) {
  if (!bundles.has(base)) {
    bundles.set(
      base,
      (async () => {
        const response = await fetch(`${base}manifest.json`);
        if (!response.ok) {
          throw new Error(
            "this export has no replay bundle beside it, so the browser cannot replay it",
          );
        }
        const manifest = await response.json();
        const [image, schedule] = await Promise.all([
          manifest.image ? fetchGzipped(`${base}image.sparse.gz`) : null,
          manifest.schedule ? fetchBytes(`${base}schedule`) : null,
        ]);
        return { manifest, image, schedule };
      })(),
    );
  }
  return bundles.get(base);
}

// Runs the machine on `files`, a map of names under /rewind to bytes,
// with `args`, in a worker of its own (replay-worker.js), and resolves to
// its step log once it exits.
function runMachine(args, files) {
  if (!crossOriginIsolated) {
    return Promise.reject(
      new Error(
        "the page is not cross-origin isolated, which the replay's threads need",
      ),
    );
  }
  const worker = new Worker(new URL("replay-worker.js", import.meta.url), {
    type: "module",
  });
  return new Promise((resolve, reject) => {
    worker.onmessage = ({ data }) => {
      worker.terminate();
      if (data.error) {
        reject(new Error(data.error));
        return;
      }
      resolve(data.log);
    };
    worker.onerror = (event) => {
      worker.terminate();
      reject(new Error(event.message));
    };
    worker.postMessage({ engine: ENGINE, args, files });
  });
}

// Replays the export at `url` to `step` and asks the guest `request`;
// resolves to the machine's step log, which the app reads the answer
// from.
export async function replay(url, step, request) {
  const started = performance.now();
  const base = bundleOf(url);
  const { manifest, image, schedule } = await loadBundle(base);
  const from = manifest.keyframes.filter((k) => k <= step).pop();
  if (from === undefined) {
    throw new Error(`the run has no keyframe at or before step ${step}`);
  }
  const state = await fetchGzipped(`${base}state-${from}.gz`);
  const args = manifest.args.map((arg) => arg.replace(UNTIL, String(step)));
  const log = await runMachine(args, {
    "image.sparse": image,
    schedule,
    state,
    request,
  });
  console.log(
    `replay: step ${step} from the keyframe at ${from} in ${Math.round(performance.now() - started)} ms`,
  );
  return log;
}
