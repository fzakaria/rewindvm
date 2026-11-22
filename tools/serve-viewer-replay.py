#!/usr/bin/env python3
"""Serve the site with the browser viewer able to replay, for trying it.

    tools/serve-viewer-replay.py <site> <engine> <exports> [--port N]

<site> is `nix build .#site`. <engine> is what tools/build-qemu-wasm.sh
writes, served as viewer/qemu/. <exports> holds exports beside their
replay bundles, X.rwd and X.replay/ from `rewind-qemu RUN --bundle`,
served as viewer/replays/, so

    http://127.0.0.1:N/viewer/?url=replays/X.rwd&step=S&file=/build/f

opens X at step S with /build/f in the file viewer, read by a replay in
the browser. The server sends the cross-origin isolation headers QEMU's
threads need, which a static host cannot.
"""

import argparse
import functools
import http.server
import os

DEFAULT_PORT = 8642


class Handler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *args, site, engine, exports, **kwargs):
        self.site = site
        self.engine = engine
        self.exports = exports
        super().__init__(*args, directory=site, **kwargs)

    def translate_path(self, path):
        path = path.split("?", 1)[0].split("#", 1)[0]
        for prefix, root in [
            ("/viewer/qemu/", self.engine),
            ("/viewer/replays/", self.exports),
        ]:
            if path.startswith(prefix):
                return os.path.realpath(os.path.join(root, path[len(prefix) :]))
        return super().translate_path(path)

    def end_headers(self):
        self.send_header("Cross-Origin-Opener-Policy", "same-origin")
        self.send_header("Cross-Origin-Embedder-Policy", "require-corp")
        self.send_header("Cache-Control", "no-store")
        super().end_headers()


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("site")
    parser.add_argument("engine")
    parser.add_argument("exports")
    parser.add_argument("--port", type=int, default=DEFAULT_PORT)
    args = parser.parse_args()
    handler = functools.partial(
        Handler,
        site=os.path.abspath(args.site),
        engine=os.path.abspath(args.engine),
        exports=os.path.abspath(args.exports),
    )
    server = http.server.ThreadingHTTPServer(("127.0.0.1", args.port), handler)
    print(f"http://127.0.0.1:{args.port}/viewer/", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
