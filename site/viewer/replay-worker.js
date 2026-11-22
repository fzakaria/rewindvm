// Runs Rewind's QEMU machine for site/viewer/replay.js in a worker of its
// own, so the machine's file system calls, which emscripten hands to the
// thread that started it, never wait on the page's thread while the app
// draws. The page posts QEMU's arguments and its files; the worker posts
// back the machine's step log, or why there is none.

// Where the machine finds its files in emscripten's file system.
const DIR = "/rewind";
const LOG = `${DIR}/steps.log`;

self.onmessage = async ({ data: { engine, args, files } }) => {
  try {
    const { default: init } = await import(engine);
    let exit;
    const exited = new Promise((resolve) => (exit = resolve));
    const mod = await init({
      print: (line) => console.log(`replay: ${line}`),
      printErr: (line) => console.log(`replay: ${line}`),
      locateFile: (file) => new URL(file, engine).href,
      mainScriptUrlOrBlob: engine,
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
    const log = mod.FS.readFile(LOG);
    self.postMessage({ log }, [log.buffer]);
  } catch (error) {
    self.postMessage({ error: String(error.message ?? error) });
  }
};
