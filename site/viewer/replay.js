// Replays a step of an export in the browser, for the app's file viewer
// at steps the export carries no answer for (crates/rewind-app/src/web.rs
// replay_file). The machine is Rewind's QEMU machine built as WebAssembly
// (qemu/, tools/build-qemu-wasm.sh), a module of its own under qemu/
// beside this page, which shares nothing with the app's module: this page
// hands it files and takes back its step log.
//
// Beside an export X.rwd, X.replay/ holds what `rewind-qemu RUN --bundle`
// writes: manifest.json, with QEMU's arguments and the steps of the run's
// keyframes, state-<step> for each keyframe, and the image and schedule.
// A replay starts from the latest keyframe at or before the step, runs
// to the step, asks the guest what the app asks, and stops once the
// guest has answered.

// Where the engine's files are, beside this page.
const ENGINE = new URL("qemu/out.js", import.meta.url).href;

// Where the machine finds its files in emscripten's file system, as the
// bundle's arguments name them.
const DIR = "/rewind";
const LOG = `${DIR}/steps.log`;

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
          manifest.image ? fetchBytes(`${base}image.erofs`) : null,
          manifest.schedule ? fetchBytes(`${base}schedule`) : null,
        ]);
        return { manifest, image, schedule };
      })(),
    );
  }
  return bundles.get(base);
}

// Runs the machine on `files`, a map of names under /rewind to bytes,
// with `args`, and resolves to its step log once it exits.
async function runMachine(args, files) {
  if (!crossOriginIsolated) {
    throw new Error(
      "the page is not cross-origin isolated, which the replay's threads need",
    );
  }
  const { default: init } = await import(ENGINE);
  let exit;
  const exited = new Promise((resolve) => (exit = resolve));
  const mod = await init({
    print: (line) => console.log(`replay: ${line}`),
    printErr: (line) => console.log(`replay: ${line}`),
    locateFile: (file) => new URL(`qemu/${file}`, import.meta.url).href,
    mainScriptUrlOrBlob: ENGINE,
    onExit: exit,
  });
  mod.FS.mkdir(DIR);
  for (const [name, bytes] of Object.entries(files)) {
    if (bytes) {
      mod.FS.writeFile(`${DIR}/${name}`, bytes);
    }
  }
  mod.callMain(args);
  const code = await exited;
  if (code !== 0) {
    throw new Error(`the machine exited with ${code}`);
  }
  return mod.FS.readFile(LOG);
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
  const state = await fetchBytes(`${base}state-${from}`);
  const args = manifest.args.map((arg) => arg.replace(UNTIL, String(step)));
  const log = await runMachine(args, {
    "image.erofs": image,
    schedule,
    state,
    request,
  });
  console.log(
    `replay: step ${step} from the keyframe at ${from} in ${Math.round(performance.now() - started)} ms`,
  );
  return log;
}
