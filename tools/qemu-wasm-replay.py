#!/usr/bin/env python3
"""Replay a run on the browser build of Rewind's QEMU machine.

    tools/qemu-wasm-replay.py <engine directory> <prepared directory>

The engine directory holds what tools/build-qemu-wasm.sh writes. The
prepared directory is what `rewind-qemu RUN --prepare DIR` writes: the
kernel, initrd, image and schedule, and args.json. This serves both to
a headless Chrome with the headers that give a page SharedArrayBuffer,
loads tools/qemu-wasm-replay.html, which runs QEMU on those files, and
takes back the machine's step log, saved as steps.log in the prepared
directory, with how long QEMU ran. `rewind-qemu RUN --log
DIR/steps.log` then compares it with the run's trace.
"""

import argparse
import functools
import http.server
import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading

HERE = os.path.dirname(os.path.abspath(__file__))
PAGE = "qemu-wasm-replay.html"

# How long a replay may take before the browser is stopped.
DEFAULT_TIMEOUT_SECONDS = 3600


class Handler(http.server.SimpleHTTPRequestHandler):
    """Serves the page, the engine under /engine/ and the prepared run
    under /run/, and takes the page's results by POST."""

    def __init__(self, *args, engine, prepared, results, **kwargs):
        self.engine = engine
        self.prepared = prepared
        self.results = results
        super().__init__(*args, directory=HERE, **kwargs)

    def translate_path(self, path):
        path = path.split("?", 1)[0]
        if path.startswith("/engine/"):
            return os.path.join(self.engine, path[len("/engine/") :])
        if path.startswith("/run/"):
            return os.path.realpath(os.path.join(self.prepared, path[len("/run/") :]))
        return super().translate_path(path)

    def end_headers(self):
        # Cross-origin isolation, which SharedArrayBuffer and so QEMU's
        # threads need.
        self.send_header("Cross-Origin-Opener-Policy", "same-origin")
        self.send_header("Cross-Origin-Embedder-Policy", "require-corp")
        self.send_header("Cache-Control", "no-store")
        super().end_headers()

    def do_POST(self):
        length = int(self.headers["Content-Length"])
        body = self.rfile.read(length)
        if self.path == "/console":
            sys.stderr.write(body.decode(errors="replace") + "\n")
        elif self.path == "/log":
            with open(os.path.join(self.prepared, "steps.log"), "wb") as f:
                f.write(body)
        elif self.path == "/done":
            self.results.update(json.loads(body))
            self.results["event"].set()
        self.send_response(204)
        self.end_headers()

    def log_message(self, format, *args):
        pass


def chrome():
    for name in ["chromium", "google-chrome-stable", "google-chrome"]:
        path = shutil.which(name)
        if path:
            return path
    sys.exit("no chromium or google-chrome on PATH")


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("engine")
    parser.add_argument("prepared")
    parser.add_argument("--timeout", type=int, default=DEFAULT_TIMEOUT_SECONDS)
    args = parser.parse_args()

    results = {"event": threading.Event()}
    handler = functools.partial(
        Handler,
        engine=os.path.abspath(args.engine),
        prepared=os.path.abspath(args.prepared),
        results=results,
    )
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
    port = server.server_address[1]
    threading.Thread(target=server.serve_forever, daemon=True).start()

    profile = tempfile.mkdtemp(prefix="rewind-qemu-wasm-")
    browser = subprocess.Popen(
        [
            chrome(),
            "--headless=new",
            "--no-first-run",
            f"--user-data-dir={profile}",
            f"http://127.0.0.1:{port}/{PAGE}",
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        if not results["event"].wait(args.timeout):
            sys.exit(f"the replay did not finish in {args.timeout} s")
    finally:
        browser.terminate()
        browser.wait()
        shutil.rmtree(profile, ignore_errors=True)
        server.shutdown()

    print(json.dumps({k: v for k, v in results.items() if k != "event"}))
    if results.get("error"):
        sys.exit(1)


if __name__ == "__main__":
    main()
