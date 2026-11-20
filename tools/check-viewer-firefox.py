#!/usr/bin/env python3
"""Open the built site's viewer in Firefox and check that the app shows.

    tools/check-viewer-firefox.py [--window] <site>

Firefox on Linux has no WebGPU, so there the app takes its first canvas off
the page and draws on a second one through WebGL2. The viewer page moves the
app's canvas into its frame, and must not move the canvas the app took back:
a canvas nothing draws on fills the frame and pushes the app out of sight.

Serves <site>, the output of `nix build .#site`, on a loopback port, opens
an example in the viewer, and reads the frame once the page has taken the
app's canvas: the frame must hold one canvas, the size of the frame, at the
frame's top. firefox and geckodriver must be on PATH.

Firefox runs headless, or with --window in a window on $DISPLAY. Headless
Firefox finds no WebGL2 in the Nix sandbox, where no GPU driver is
installed, and a Firefox in a window on Xvfb does, drawn in software by
Mesa, so the flake's check passes --window.
"""

import argparse
import enum
import functools
import http.server
import shutil
import sys
import threading
import time

from selenium import webdriver
from selenium.webdriver.firefox.options import Options
from selenium.webdriver.firefox.service import Service

# The example the viewer opens, relative to the viewer's page.
EXAMPLE = "viewer/?url=examples/philosophers.rwd"

# A window wide enough for the app, which the page shows a note instead of
# on a frame narrower than 960 pixels.
WINDOW_WIDTH = 1500
WINDOW_HEIGHT = 950

# How long the app may take to load and put its canvas on the page, and
# how often the page is asked whether it has.
START_TIMEOUT_SECONDS = 120
POLL_SECONDS = 0.5

# How far the canvas's box may be from the frame's, in CSS pixels: the
# frame's border.
BOX_SLACK_PIXELS = 2

# What the page is asked: whether its word on loading is gone, and the
# frame's box beside the box of every canvas in the frame and the text of
# every paragraph the app left there to say that the browser cannot draw.
STARTED = "return document.getElementById('viewer-status').hidden"
FRAME = """
const frame = document.getElementById('viewer-frame');
const box = (node) => {
  const r = node.getBoundingClientRect();
  return { top: r.top, left: r.left, width: r.width, height: r.height };
};
return {
  frame: box(frame),
  canvases: [...frame.querySelectorAll('canvas')].map(box),
  messages: [...frame.querySelectorAll(':scope > p:not(#viewer-status)')]
    .map((p) => p.textContent.trim()),
};
"""


class QuietHandler(http.server.SimpleHTTPRequestHandler):
    """Serves files without a line on stderr for each request."""

    def log_message(self, format, *args):
        pass


def serve(site):
    """Serves the directory `site` on a free loopback port and returns the server."""
    handler = functools.partial(QuietHandler, directory=site)
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


class Screen(enum.Enum):
    """Where Firefox draws: nowhere, or in a window on $DISPLAY."""

    HEADLESS = "headless"
    WINDOW = "window"


def firefox(screen):
    """A Firefox of the window size the app needs, drawing on `screen`."""
    options = Options()
    if screen is Screen.HEADLESS:
        options.add_argument("-headless")
    options.binary_location = shutil.which("firefox")
    driver = webdriver.Firefox(
        options=options, service=Service(executable_path=shutil.which("geckodriver"))
    )
    driver.set_window_size(WINDOW_WIDTH, WINDOW_HEIGHT)
    return driver


def wait_for_start(driver):
    """Waits until the page has taken a canvas or a message from the app."""
    deadline = time.monotonic() + START_TIMEOUT_SECONDS
    while time.monotonic() < deadline:
        if driver.execute_script(STARTED):
            return
        time.sleep(POLL_SECONDS)
    sys.exit(f"the viewer did not start within {START_TIMEOUT_SECONDS} seconds")


def check(found):
    """Exits with what is wrong with the frame `found`, if anything is."""
    if found["messages"]:
        sys.exit(f"the app could not draw in Firefox: {found['messages']}")

    canvases = found["canvases"]
    if len(canvases) != 1:
        sys.exit(
            f"the frame holds {len(canvases)} canvases, not the app's one: {canvases}"
        )

    frame, canvas = found["frame"], canvases[0]
    for side in ("top", "left", "width", "height"):
        if abs(frame[side] - canvas[side]) > BOX_SLACK_PIXELS:
            sys.exit(f"the app's canvas {canvas} does not fill the frame {frame}")


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("site", help="the built site, as `nix build .#site` makes it")
    parser.add_argument(
        "--window",
        dest="screen",
        action="store_const",
        const=Screen.WINDOW,
        default=Screen.HEADLESS,
        help="run Firefox in a window on $DISPLAY and not headless",
    )
    args = parser.parse_args()

    server = serve(args.site)
    driver = firefox(args.screen)
    try:
        driver.get(f"http://127.0.0.1:{server.server_address[1]}/{EXAMPLE}")
        wait_for_start(driver)

        # The canvas the app draws on reaches the page with the one the
        # app took back, so one poll later both are where they stay.
        time.sleep(POLL_SECONDS)
        check(driver.execute_script(FRAME))
    finally:
        driver.quit()
        server.shutdown()
    print("the viewer shows the app in Firefox")


if __name__ == "__main__":
    main()
