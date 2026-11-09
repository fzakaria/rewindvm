#!/usr/bin/env python3
"""Render the tutorials and case studies in docs/ as site pages.

    tools/md2html.py && nix fmt -- site/tutorials/*.html site/case-studies/*.html

docs/tutorial-*.md become site/tutorials/*.html and docs/case-studies/*.md
become site/case-studies/*.html, in the site's article style. Only the
Markdown these documents use is handled: headings, paragraphs, "- " lists,
tables, fenced code, inline code, links and bold. A relative link to a
document the site also publishes points at its page; any other relative link
points at the file on GitHub, since the site has no copy of the repository.

A tutorial's or case study's console blocks are painted in the colors
rewind printed them in, which tools/record-tutorials.py keeps in
docs/colors/<page>.json, since the Markdown, which GitHub shows, holds plain
text.
"""

import html
import json
import posixpath
import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent

# Where tools/record-tutorials.py keeps each tutorial's colored stretches,
# and the prefix of the class each stretch's styles become.
COLORS = REPO / "docs" / "colors"
COLOR_CLASS_PREFIX = "ansi-"

# Where the repository's files are on GitHub, for links the site cannot serve.
GITHUB_BLOB = "https://github.com/fzakaria/rewindvm/blob/main/"
GITHUB_TREE = "https://github.com/fzakaria/rewindvm/tree/main/"

# The site's own address, which links in the documents use so that they
# also work on GitHub.
SITE_URL = "https://rewindvm.dev/"

# Documents the site publishes, by repository path, and their site paths.
LINK_MAP = {
    "docs/pmu.md": "counter-time.html",
    "docs/nix-builds.md": "nix-builds.html",
    "docs/tutorial-nix.md": "tutorials/nix.html",
    "docs/tutorial-container.md": "tutorials/container.html",
    "docs/tutorial-properties.md": "tutorials/properties.html",
    "docs/tutorial-points.md": "tutorials/points.html",
    "docs/tutorial-repro.md": "tutorials/repro.html",
    "docs/tutorial-derivations.md": "tutorials/derivations.html",
    "docs/tutorial-advanced.md": "tutorials/advanced.html",
    "docs/case-studies/nix-gc-closure-sigpipe.md": "case-studies/nix-gc-closure-sigpipe.html",
    "docs/case-studies/nix-schema-migration-hang.md": "case-studies/nix-schema-migration-hang.html",
    "docs/case-studies/devenv-task-output-race.md": "case-studies/devenv-task-output-race.html",
    "examples/case-studies/flake.nix": GITHUB_BLOB + "examples/case-studies/flake.nix",
}

# The app's screenshot, shown in the Nix tutorial after the first paragraph
# of the section that introduces the app.
NIX_FIGURE = """<figure class="shot">
  <picture>
    <source srcset="../img/app-failure.webp" type="image/webp" />
    <img
      src="../img/app-failure.png"
      width="1440"
      height="900"
      loading="lazy"
      alt="The Rewind desktop app on a failing mylib run: the timeline with the playhead in the check phase, the build log, the process tree, files written, and the SIGSEGV at 0x108 next to where the run diverged from its parent"
    />
  </picture>
  <figcaption>
    The app on a failing mylib run, compared with the passing run it was forked from.
  </figcaption>
</figure>
"""

# The app on a failing fork of the container run, at its SIGSEGV, shown in
# the container tutorial where the Nix tutorial shows its own.
CONTAINER_FIGURE = """<figure class="shot">
  <picture>
    <source srcset="../img/app-container.webp" type="image/webp" />
    <img
      src="../img/app-container.png"
      width="1440"
      height="900"
      loading="lazy"
      alt="The Rewind desktop app on a failing fork of the mylib container run: the timeline with the playhead at step 696, the test's output, the process tree, and the SIGSEGV at 0x108 next to where the run diverged from its parent at step 684"
    />
  </picture>
  <figcaption>
    The app on a failing mylib fork, compared with the passing run it was forked from.
  </figcaption>
</figure>
"""

# The app on the failing devenv run at the step where SIGCHLD reaches the
# test process, shown after the paragraph that walks through those events.
# Numbered pins sit over the screenshot with a card for each below it, as
# on the front page; the pins' positions are percentages of the 1440x900
# image. The screenshot and the numbers come from the recording of
# docs/templates/case-devenv-task-output-race.md, which checks that the
# numbers here are its own.
DEVENV_FIGURE = """<div class="tour tour-article">
  <figure class="tour-shot">
    <div class="tour-frame">
      <a class="tour-link" href="../img/devenv-sigchld.png">
        <picture>
          <source srcset="../img/devenv-sigchld.webp" type="image/webp" />
          <img
            src="../img/devenv-sigchld.png"
            width="1440"
            height="900"
            loading="lazy"
            alt="The Rewind desktop app on the failing devenv run at step 1,647, compared with the passing run dddef5dd: the build log stops at running 1 test, SIGCHLD has reached thread 41, and the runs are the same until step 1,644. Opens the full-size image."
          />
        </picture>
      </a>
      <button type="button" class="pin pin-1" style="left: 93.1%; top: 11.1%" aria-label="1: the timeline">1</button>
      <button type="button" class="pin pin-2" style="left: 20.8%; top: 34.8%" aria-label="2: the build log">2</button>
      <button type="button" class="pin pin-3" style="left: 96.9%; top: 36.7%" aria-label="3: the step's event, SIGCHLD">3</button>
      <button type="button" class="pin pin-4" style="left: 97.2%; top: 44.6%" aria-label="4: where the runs part">4</button>
    </div>
    <figcaption>
      The app on the failing run 39531d5d at step 1,647, compared with the passing run dddef5dd.
    </figcaption>
  </figure>
  <ol class="tour-cards">
    <li class="tour-card" id="tour-1" tabindex="0">
      <h3>The timeline</h3>
      <p>Step 1,647 of 1,714, three steps after the one where only the passing run has its running thread paused.</p>
    </li>
    <li class="tour-card" id="tour-2" tabindex="0">
      <h3>Build log</h3>
      <p>The test has started and nothing from the task is in the log. Its three lines are in the pipe, unread.</p>
    </li>
    <li class="tour-card" id="tour-3" tabindex="0">
      <h3>At this step</h3>
      <p>SIGCHLD reaches thread 41 of the test process: the task's shell has exited.</p>
    </li>
    <li class="tour-card" id="tour-4" tabindex="0">
      <h3>Divergence</h3>
      <p>The runs are the same until step 1,644, where only the passing run has its running thread paused for 640 µs. This SIGCHLD is the first call that differs.</p>
    </li>
  </ol>
</div>
"""

# The lost hit, drawn: one thread's store lands between the other's read
# and store, so the count goes up once for two hits. Shown in the points
# tutorial after the paragraph that says what the call does.
POINTS_FIGURE = """<figure class="race">
  <svg viewBox="0 0 640 190" role="img" aria-label="Thread A reads 7, a switch at the schedule point lets thread B read 7 and store 8, then thread A stores 8, and B's hit is lost">
    <text x="0" y="52" fill="var(--muted)">thread A</text>
    <text x="0" y="132" fill="var(--muted)">thread B</text>
    <line x1="80" y1="48" x2="630" y2="48" stroke="var(--line)" stroke-width="2" />
    <line x1="80" y1="128" x2="630" y2="128" stroke="var(--line)" stroke-width="2" />
    <rect x="96" y="32" width="120" height="32" rx="5" fill="var(--panel)" stroke="var(--blue)" />
    <text x="110" y="53" fill="var(--blue)">read 7</text>
    <line x1="236" y1="14" x2="236" y2="176" stroke="var(--amber)" stroke-width="2" stroke-dasharray="4 4" />
    <text x="244" y="22" fill="var(--amber)">rewind_point()</text>
    <rect x="258" y="112" width="120" height="32" rx="5" fill="var(--panel)" stroke="var(--blue)" />
    <text x="272" y="133" fill="var(--blue)">read 7</text>
    <rect x="392" y="112" width="120" height="32" rx="5" fill="var(--panel)" stroke="var(--blue)" />
    <text x="404" y="133" fill="var(--blue)">store 8</text>
    <rect x="500" y="32" width="120" height="32" rx="5" fill="var(--panel)" stroke="var(--red)" />
    <text x="512" y="53" fill="var(--red)">store 8</text>
    <text x="410" y="176" fill="var(--red)">two hits, count 8</text>
  </svg>
  <figcaption>
    A switch at the point: thread B's hit lands between thread A's read and
    store, and thread A's store overwrites it.
  </figcaption>
</figure>
"""

# From a byte back to its cause, drawn: the copy blame looked past, the
# program that wrote the differing bytes, and the one input that changed.
# Shown in the repro tutorial after the paragraph that reads blame's
# output.
BLAME_FIGURE = """<figure class="drawing">
  <svg viewBox="0 0 640 290" role="img" aria-label="install copied tiles.pc, which already differed. python3 gen-pc.py wrote the Requires.private line in another order under seed 0 and seed 1, with the same command line and the same input. The seed is the one input that changed.">
    <rect x="1" y="1" width="638" height="62" rx="6" fill="var(--panel)" stroke="var(--line-2)" />
    <text x="16" y="26" fill="var(--text)">install</text>
    <text x="624" y="26" text-anchor="end" fill="var(--muted)">last writer</text>
    <text x="16" y="48" fill="var(--muted)">copies /build/tiles/tiles.pc, which already differs</text>
    <line x1="40" y1="64" x2="40" y2="90" stroke="var(--line-2)" stroke-width="2" stroke-dasharray="3 4" />
    <rect x="1" y="91" width="638" height="106" rx="6" fill="var(--panel)" stroke="var(--amber)" />
    <text x="16" y="116" fill="var(--amber)">python3 gen-pc.py</text>
    <text x="624" y="116" text-anchor="end" fill="var(--muted)">first writer</text>
    <text x="16" y="140" fill="var(--amber)">seed 0: Requires.private: pixman-1, libpng, zlib</text>
    <text x="16" y="162" fill="var(--blue)">seed 1: Requires.private: zlib, pixman-1, libpng</text>
    <text x="16" y="184" fill="var(--muted)">same command line, same gen-pc.py in both runs</text>
    <line x1="40" y1="198" x2="40" y2="224" stroke="var(--line-2)" stroke-width="2" stroke-dasharray="3 4" />
    <rect x="1" y="225" width="638" height="62" rx="6" fill="var(--panel)" stroke="var(--amber)" stroke-dasharray="5 4" />
    <text x="16" y="250" fill="var(--amber)">the seed</text>
    <text x="624" y="250" text-anchor="end" fill="var(--muted)">the cause</text>
    <text x="16" y="272" fill="var(--muted)">the one input that changed: it decides Python's hash seed</text>
  </svg>
  <figcaption>
    Blame looks past the copy to the program that wrote the bytes. That
    program had the same inputs in both runs, so the cause is the seed.
  </figcaption>
</figure>
"""

# The order plugins.txt names the plugins in, drawn: the VM's own order,
# newest first, two rewind.dirents keys, and the fix's sorted(). The repro
# tutorial's template asserts the first three. Shown after the paragraph
# that introduces the index under each key.
DIRENTS_FIGURE = """<figure class="drawing">
  <svg viewBox="0 0 640 184" role="img" aria-label="The order plugins.txt lists the five plugins in. stock order: sharpen rotate resize crop blur. rewind.dirents=1: sharpen crop blur rotate resize. rewind.dirents=2: sharpen resize crop rotate blur. sorted(): blur crop resize rotate sharpen">
    <text x="0" y="28" fill="var(--muted)">stock order</text>
    <rect x="168" y="8" width="84" height="30" rx="5" fill="var(--panel)" stroke="var(--line-2)" />
    <text x="210" y="28" text-anchor="middle" fill="var(--soft)">sharpen</text>
    <rect x="262" y="8" width="84" height="30" rx="5" fill="var(--panel)" stroke="var(--line-2)" />
    <text x="304" y="28" text-anchor="middle" fill="var(--soft)">rotate</text>
    <rect x="356" y="8" width="84" height="30" rx="5" fill="var(--panel)" stroke="var(--line-2)" />
    <text x="398" y="28" text-anchor="middle" fill="var(--soft)">resize</text>
    <rect x="450" y="8" width="84" height="30" rx="5" fill="var(--panel)" stroke="var(--line-2)" />
    <text x="492" y="28" text-anchor="middle" fill="var(--soft)">crop</text>
    <rect x="544" y="8" width="84" height="30" rx="5" fill="var(--panel)" stroke="var(--line-2)" />
    <text x="586" y="28" text-anchor="middle" fill="var(--soft)">blur</text>
    <text x="0" y="72" fill="var(--muted)">rewind.dirents=1</text>
    <rect x="168" y="52" width="84" height="30" rx="5" fill="var(--panel)" stroke="var(--amber)" />
    <text x="210" y="72" text-anchor="middle" fill="var(--amber)">sharpen</text>
    <rect x="262" y="52" width="84" height="30" rx="5" fill="var(--panel)" stroke="var(--amber)" />
    <text x="304" y="72" text-anchor="middle" fill="var(--amber)">crop</text>
    <rect x="356" y="52" width="84" height="30" rx="5" fill="var(--panel)" stroke="var(--amber)" />
    <text x="398" y="72" text-anchor="middle" fill="var(--amber)">blur</text>
    <rect x="450" y="52" width="84" height="30" rx="5" fill="var(--panel)" stroke="var(--amber)" />
    <text x="492" y="72" text-anchor="middle" fill="var(--amber)">rotate</text>
    <rect x="544" y="52" width="84" height="30" rx="5" fill="var(--panel)" stroke="var(--amber)" />
    <text x="586" y="72" text-anchor="middle" fill="var(--amber)">resize</text>
    <text x="0" y="116" fill="var(--muted)">rewind.dirents=2</text>
    <rect x="168" y="96" width="84" height="30" rx="5" fill="var(--panel)" stroke="var(--blue)" />
    <text x="210" y="116" text-anchor="middle" fill="var(--blue)">sharpen</text>
    <rect x="262" y="96" width="84" height="30" rx="5" fill="var(--panel)" stroke="var(--blue)" />
    <text x="304" y="116" text-anchor="middle" fill="var(--blue)">resize</text>
    <rect x="356" y="96" width="84" height="30" rx="5" fill="var(--panel)" stroke="var(--blue)" />
    <text x="398" y="116" text-anchor="middle" fill="var(--blue)">crop</text>
    <rect x="450" y="96" width="84" height="30" rx="5" fill="var(--panel)" stroke="var(--blue)" />
    <text x="492" y="116" text-anchor="middle" fill="var(--blue)">rotate</text>
    <rect x="544" y="96" width="84" height="30" rx="5" fill="var(--panel)" stroke="var(--blue)" />
    <text x="586" y="116" text-anchor="middle" fill="var(--blue)">blur</text>
    <text x="0" y="160" fill="var(--muted)">sorted()</text>
    <rect x="168" y="140" width="84" height="30" rx="5" fill="var(--panel)" stroke="var(--line-2)" />
    <text x="210" y="160" text-anchor="middle" fill="var(--text)">blur</text>
    <rect x="262" y="140" width="84" height="30" rx="5" fill="var(--panel)" stroke="var(--line-2)" />
    <text x="304" y="160" text-anchor="middle" fill="var(--text)">crop</text>
    <rect x="356" y="140" width="84" height="30" rx="5" fill="var(--panel)" stroke="var(--line-2)" />
    <text x="398" y="160" text-anchor="middle" fill="var(--text)">resize</text>
    <rect x="450" y="140" width="84" height="30" rx="5" fill="var(--panel)" stroke="var(--line-2)" />
    <text x="492" y="160" text-anchor="middle" fill="var(--text)">rotate</text>
    <rect x="544" y="140" width="84" height="30" rx="5" fill="var(--panel)" stroke="var(--line-2)" />
    <text x="586" y="160" text-anchor="middle" fill="var(--text)">sharpen</text>
  </svg>
  <figcaption>
    The order the unpatched plugins build writes its index in: the VM's own
    order, which lists the newest file first, then two keys. The plugins were
    copied into /build in name order, the order sorted() gives back.
  </figcaption>
</figure>
"""

# The gc-closure case study's video on YouTube, shown after the page's
# opening paragraph.
GC_CLOSURE_VIDEO = """<figure class="shot video" id="video">
  <iframe
    src="https://www.youtube.com/embed/7v7x7O82nxY?rel=0"
    title="Catching a flaky Nix test on its first run with Rewind VM"
    loading="lazy"
    allow="encrypted-media; picture-in-picture"
    allowfullscreen
  ></iframe>
  <figcaption>This bug from first run to fix, in 2:24.</figcaption>
</figure>
"""

# Each page: its source, where it goes, the line above its title, its meta
# description, what its table of contents is called, its figures, each with
# the section and the paragraph of that section it follows (None for the
# paragraphs above the first section), and the card that closes it.
PAGES = [
    dict(
        source="docs/tutorial-nix.md",
        eyebrow="Tutorial &middot; Nix",
        description="Install Rewind VM, find the thread interleaving that breaks a Nix derivation's tests, look at the crash step by step, fork it, and check the fix.",
        toc="Steps",
        figures=[("scrub-it-in-the-app", 1, NIX_FIGURE)],
        next=(
            "tutorials/container.html",
            "Tutorial &middot; Container",
            "A flaky test in a container",
            "The same bug from a Docker image, with no Nix.",
        ),
    ),
    dict(
        source="docs/tutorial-container.md",
        eyebrow="Tutorial &middot; Container",
        description="Install Rewind VM, find the thread interleaving that breaks a container image's tests, look at the crash step by step, fork it, and check the fix. No Nix needed.",
        toc="Steps",
        figures=[("scrub-it-in-the-app", 1, CONTAINER_FIGURE)],
        next=(
            "tutorials/properties.html",
            "Tutorial &middot; Properties",
            "Property-based testing",
            "Properties every run must have, assertions from inside the program, and seeds.",
        ),
    ),
    dict(
        source="docs/tutorial-properties.md",
        eyebrow="Tutorial &middot; Properties",
        description="Property-based testing with rewind check: properties every run must have, assertions a program makes with the Rewind SDK, and seeds that vary a program's randomness as well as its schedule.",
        toc="Steps",
        figures=[],
        next=(
            "tutorials/points.html",
            "Tutorial &middot; Points",
            "Schedule points",
            "One line that puts a thread switch where no system call is.",
        ),
    ),
    dict(
        source="docs/tutorial-points.md",
        eyebrow="Tutorial &middot; Points",
        description="Schedule points with the Rewind SDK: one line that lets rewind check switch threads between a read and a store, reaching a race that no schedule reaches at system calls alone.",
        toc="Steps",
        figures=[("mark-the-place", 3, POINTS_FIGURE)],
        next=(
            "tutorials/repro.html",
            "Tutorial &middot; Reproducible builds",
            "Reproducible builds",
            "A build whose output changes with its seed or its directory order, and the program that made it change.",
        ),
    ),
    dict(
        source="docs/tutorial-repro.md",
        eyebrow="Tutorial &middot; Reproducible builds",
        description="Reproducible builds with Rewind VM: build a derivation under several seeds and directory orders, see which files differ, find the program that wrote the differing bytes with rewind blame, and check the fix.",
        toc="Steps",
        figures=[
            ("who-wrote-it", 2, BLAME_FIGURE),
            ("directory-order", 4, DIRENTS_FIGURE),
        ],
        next=(
            "tutorials/derivations.html",
            "Tutorial &middot; Derivations",
            "Deterministic derivations",
            "Pin a build's environment with inRewind and check that a rebuild matches.",
        ),
    ),
    dict(
        source="docs/tutorial-derivations.md",
        eyebrow="Tutorial &middot; Derivations",
        description="Use inRewind to pin a Nix build's environment, rebuild it, and compare the output. Change the clock to make a different derivation.",
        toc="Steps",
        figures=[],
        next=(
            "tutorials/advanced.html",
            "Tutorial &middot; Advanced",
            "Advanced features",
            "Watchpoints, the kernel's side of a crash, tools inside the VM, and the rest.",
        ),
    ),
    dict(
        source="docs/nix-builds.md",
        eyebrow="Reference &middot; Nix builds",
        description="Machine settings, pinned Rewind runtimes, the external-builder interface, and exporting files from a recorded build.",
        toc="Contents",
        figures=[],
        next=(
            "tutorials/derivations.html",
            "Tutorial &middot; Derivations",
            "Deterministic derivations",
            "Pin a build's environment with inRewind and check that a rebuild matches.",
        ),
    ),
    dict(
        source="docs/tutorial-advanced.md",
        eyebrow="Tutorial &middot; Advanced",
        description="Short recipes on Rewind VM's other tools: watchpoints, kernel breakpoints, your own gdb, tools inside the VM, more CPUs, steering the perturbation, comparing and sharing runs.",
        toc="Recipes",
        figures=[],
        next=(
            "case-studies/nix-gc-closure-sigpipe.html",
            "Case study",
            "A SIGPIPE in Nix's gc-closure test",
            "The tools on a real bug in Nix's own test suite.",
        ),
    ),
    dict(
        source="docs/case-studies/nix-gc-closure-sigpipe.md",
        eyebrow="Case study",
        description="Rewind VM's first run of Nix's functional tests hit a SIGPIPE in gc-closure.sh: a pipe into head -n1 under pipefail, and bash writing a two-line printf in two writes. Reported and fixed upstream.",
        toc="Sections",
        figures=[(None, 1, GC_CLOSURE_VIDEO)],
        next=(
            "case-studies/nix-schema-migration-hang.html",
            "Case study",
            "A hang in Nix's store schema migration",
            "A known Nix hang, reproduced in Rewind VM and pinned to one SQLite error code with gdb inside the VM.",
        ),
    ),
    dict(
        source="docs/case-studies/nix-schema-migration-hang.md",
        eyebrow="Case study",
        description="A known hang in Nix's store schema migration, reproduced in Rewind VM and pinned to SQLITE_BUSY_SNAPSHOT with gdb inside the VM, with both upstream fixes checked.",
        toc="Sections",
        figures=[],
        next=(
            "case-studies/devenv-task-output-race.html",
            "Case study",
            "Lost task output in devenv",
            "A known devenv bug that dropped a task's last lines, reproduced in Rewind VM and traced with gdb to a line left in a reader's buffer.",
        ),
    ),
    dict(
        source="docs/case-studies/devenv-task-output-race.md",
        eyebrow="Case study",
        description="A known devenv bug that dropped a task's last lines of output, reproduced in Rewind VM, traced with gdb to tokio::select! taking the child's exit before a buffered line, and the fix checked under every schedule.",
        toc="Sections",
        figures=[("where-the-third-line-went", 2, DEVENV_FIGURE)],
        next=(
            "case-studies/nix-gc-closure-sigpipe.html",
            "Case study",
            "A SIGPIPE in Nix's gc-closure test",
            "A Nix test flake no one had reported, found on Rewind VM's first run of the test suite.",
        ),
    ),
]

FONTS = "https://fonts.googleapis.com/css2?family=Bricolage+Grotesque:opsz,wght@12..96,500;12..96,700&family=IBM+Plex+Sans:wght@400;500;600&family=JetBrains+Mono:wght@400;600&display=swap"

HEAD = """<!doctype html>
<html lang="en">
  <head>
    <title>{title}: Rewind VM</title>
    <meta
      name="description"
      content="{description}"
    />
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <link rel="canonical" href="https://rewindvm.dev/{page}" />
    <link rel="icon" href="{up}img/mark.svg" type="image/svg+xml" />
    <link rel="preconnect" href="https://fonts.googleapis.com" />
    <link rel="preconnect" href="https://fonts.gstatic.com" crossorigin />
    <link
      href="{fonts}"
      rel="stylesheet"
    />
    <link rel="stylesheet" href="{up}style.css" />
    <!-- Google Analytics, with consent defaults set before gtag.js runs. -->
    <script src="{up}analytics.js"></script>
    <script async src="https://www.googletagmanager.com/gtag/js?id=G-L0515BFLG0"></script>
  </head>
  <body>
    <header class="wrap nav">
      <a class="brand" href="{up}">
        <svg viewBox="0 0 24 24" aria-hidden="true">
          <path d="M11 19l-7-7 7-7" />
          <path d="M20 19l-7-7 7-7" />
        </svg>
        <span>Rewind VM</span>
      </a>
      <nav class="page-links">
        <a href="{up}#tutorials">Tutorials</a>
        <a href="{up}#case-studies">Case studies</a>
        <a href="{up}nix-builds.html">Nix build reference</a>
        <a href="{up}counter-time.html">Counter time</a>
        <a href="{up}viewer/">Viewer</a>
      </nav>
    </header>

    <!-- Generated by tools/md2html.py from {source}. The transcripts
         are real output; a tutorial's are recorded by
         tools/record-tutorials.py from its template in docs/templates/,
         which is the file to edit. -->
    <main class="wrap article">
      <p class="eyebrow">{eyebrow}</p>
"""

FOOT = """    </main>

    <footer class="wrap footer">
      <p>
        &copy; 2026 <a href="https://lunchtimesurf.com/">Lunch Time Surf LLC</a> &middot; Santa
        Cruz, California &middot;
        <a href="mailto:tacos@lunchtimesurf.com">tacos@lunchtimesurf.com</a> &middot;
        <a href="{up}privacy.html">Privacy</a> &middot;
        <a href="{up}refunds.html">Refunds</a> &middot;
        <a href="{up}bugs.html">Bugs found</a> &middot;
        <a href="{up}bench/">Benchmarks</a> &middot;
        <a href="https://github.com/fzakaria/rewindvm">GitHub</a>
      </p>
    </footer>
  </body>
</html>
"""

NEXT = """
<a class="card next" href="{href}">
  <span class="eyebrow">{eyebrow}</span>
  <span class="next-title">{title}</span>
  <span class="next-text">{text}</span>
</a>
"""

# The box that closes every page: the one link to the pricing section, and
# the prices in a sentence, as the landing page states them.
BUY = """
<div class="cta">
  <a class="btn" href="{up}#pricing">Get Rewind VM</a>
  <p>The engine is free and MIT licensed. The desktop app is $49 personal or $99 per seat.</p>
</div>
"""


def site_path(source):
    """The page a document becomes, relative to the site's root."""
    return LINK_MAP[source]


def slug(text):
    """GitHub's heading anchors: lowercase, punctuation dropped, spaces to dashes."""
    text = re.sub(r"[^\w\- ]", "", text.lower())
    return text.replace(" ", "-")


def resolve(href, source, page):
    """A link in SOURCE as it must read on PAGE."""
    # A link to the site itself stays on the copy of the site being read,
    # so a local build opens its own viewer and examples.
    if href.startswith(SITE_URL):
        target, sep, query = href.removeprefix(SITE_URL).partition("?")
        relative = posixpath.relpath(target or ".", posixpath.dirname(page) or ".")
        if target.endswith("/") or not target:
            relative += "/"
        return relative + sep + query

    if re.match(r"^[a-z]+:", href) or href.startswith("#"):
        return href
    target, _, frag = href.partition("#")
    frag = "#" + frag if frag else ""
    path = posixpath.normpath(posixpath.join(posixpath.dirname(source), target))

    # A document the site publishes, or a file mapped to a URL.
    if path in LINK_MAP:
        mapped = LINK_MAP[path]
        if re.match(r"^[a-z]+:", mapped):
            return mapped + frag
        return posixpath.relpath(mapped, posixpath.dirname(page) or ".") + frag

    # Anything else in the repository is on GitHub.
    base = GITHUB_TREE if (REPO / path).is_dir() else GITHUB_BLOB
    return base + path + frag


def inline(text, source, page):
    """Inline Markdown to HTML: code spans, links and bold."""
    out = []

    # Split out code spans first so nothing inside them is touched.
    for i, part in enumerate(re.split(r"(`[^`]*`)", text)):
        if i % 2 == 1:
            out.append("<code>" + html.escape(part[1:-1], quote=False) + "</code>")
            continue
        part = html.escape(part, quote=False)
        part = re.sub(r"\*\*([^*]+)\*\*", r"<strong>\1</strong>", part)

        def link(m):
            return f'<a href="{resolve(m.group(2), source, page)}">{m.group(1)}</a>'

        part = re.sub(r"\[([^\]]+)\]\(([^)\s]+)\)", link, part)
        out.append(part)
    return "".join(out)


def colors_of(source):
    """The colored console blocks of `source`, from docs/colors/, by each
    block's text: its stretches as line, start, end and styles."""
    path = COLORS / (Path(source).stem + ".json")
    if not path.exists():
        return path, {}
    return path, {b["text"]: b["spans"] for b in json.loads(path.read_text())}


def painted(line, spans):
    """`line` as HTML, each of `spans` (start, end, styles) in a span whose
    classes are its styles."""
    out = []
    end = 0
    for start, stop, styles in sorted(spans):
        out.append(html.escape(line[end:start], quote=False))
        classes = " ".join(COLOR_CLASS_PREFIX + s for s in styles.split())
        out.append(
            f'<span class="{classes}">'
            + html.escape(line[start:stop], quote=False)
            + "</span>"
        )
        end = stop
    out.append(html.escape(line[end:], quote=False))
    return "".join(out)


def console(lines, spans=()):
    """A console block: "$ " starts a command, "> " continues one. `spans`
    are the output's colored stretches, as line, start, end and styles."""
    by_line = {}
    for i, start, stop, styles in spans:
        by_line.setdefault(i, []).append((start, stop, styles))
    body = []
    in_command = False
    for i, line in enumerate(lines):
        prompt = (
            "$"
            if line.startswith("$ ")
            else ">" if in_command and line.startswith("> ") else None
        )
        in_command = prompt is not None
        if prompt is None:
            body.append(painted(line, by_line.get(i, [])))
            continue
        body.append(
            f'<span class="cmd"><span class="prompt">{html.escape(prompt)}</span> '
            + source_text(line[2:], SHELL_TOKEN)
            + "</span>"
        )
    return '<pre\n  class="console"\n><code>' + "\n".join(body) + "</code></pre>\n"


def diff(lines):
    kinds = {"+": "add", "-": "del", "@": "hunk"}
    body = [
        f'<span class="{kinds.get(line[:1], "ctx")}">'
        + html.escape(line, quote=False)
        + "</span>"
        for line in lines
    ]
    return '<pre class="console diff"><code>' + "\n".join(body) + "</code></pre>\n"


# Rust tokens, in the order they are tried: a comment or string swallows
# anything that looks like another token inside it.
RUST_KEYWORDS = (
    "as async await break const continue crate else enum false fn for if impl in "
    "let loop match mod move mut pub ref return self Self static struct trait true "
    "type unsafe use where while"
).split()
RUST_TOKEN = re.compile(
    r"(?P<comment>//.*)"
    r'|(?P<string>"(?:\\.|[^"\\])*")'
    r"|(?P<attr>#\[[^\]]*\])"
    r"|(?P<elide>\.\.\.)"
    r"|(?P<macro>\b[a-z_][a-z0-9_]*!)"
    r"|(?P<keyword>\b(?:" + "|".join(RUST_KEYWORDS) + r")\b)"
    r"|(?P<type>\b[A-Z][A-Za-z0-9_]*\b)"
    r"|(?P<number>\b\d[\d_]*\b)"
)

# C tokens, tried the same way. A macro is a name in capitals, as
# REWIND_ALWAYS is; a type is one of C's own or a name ending in _t.
C_KEYWORDS = (
    "break case char const continue default do double else enum extern float for "
    "goto if int long return short signed sizeof static struct switch typedef "
    "union unsigned void volatile while"
).split()
C_TOKEN = re.compile(
    r"(?P<comment>//.*|/\*[\s\S]*?(?:\*/|$))"
    r'|(?P<string>"(?:\\.|[^"\\])*")'
    r"|(?P<attr>(?m:^[ \t]*#[ \t]*\w+))"
    r"|(?P<elide>\.\.\.)"
    r"|(?P<keyword>\b(?:" + "|".join(C_KEYWORDS) + r")\b)"
    r"|(?P<macro>\b[A-Z][A-Z0-9_]+\b)"
    r"|(?P<type>\b[a-z_][a-z0-9_]*_t\b)"
    r"|(?P<number>\b\d+[lLuU]*\b)"
)

# Nix strings and block comments can span lines. Keep shell text inside
# indented strings in the string's color, including escaped quote pairs.
NIX_KEYWORDS = "assert else if in inherit let or rec then with true false null".split()
NIX_TOKEN = re.compile(
    r"(?P<comment>\#.*|/\*[\s\S]*?(?:\*/|$))"
    r'|(?P<string>"(?:\\.|[^"\\])*"|'
    r"''(?:''['$]|''\\.|'(?!')|[^'])*'')"
    r"|(?P<keyword>(?<![\w'-])(?:" + "|".join(NIX_KEYWORDS) + r")(?![\w'-]))"
    r"|(?P<property>\b[a-zA-Z_][\w'-]*(?=\s*=(?!=)))"
    r"|(?P<number>\b\d+(?:\.\d+)?\b)"
)

# A shell comment follows whitespace or starts a line, so a flake's
# #fragment stays in its argument. Quotes consume apparent tokens inside.
SHELL_KEYWORDS = (
    "case do done elif else esac fi for function if in select then until while "
    "cd echo eval exec exit export local printf read readonly return set shift "
    "source test trap unset"
).split()
SHELL_TOKEN = re.compile(
    r"(?P<comment>(?<!\S)\#.*)"
    r"|(?P<string>'[^']*'|\"(?:\\.|[^\"\\])*\")"
    r"|(?P<variable>\$\{[^}]*\}|\$[a-zA-Z_][a-zA-Z0-9_]*|\$[0-9@*#?$!-])"
    r"|(?P<option>(?<!\S)--?[a-zA-Z0-9][a-zA-Z0-9-]*)"
    r"|(?P<keyword>(?<![\w./-])(?:" + "|".join(SHELL_KEYWORDS) + r")(?![\w./-]))"
    r"|(?P<operator>\$\(|[|&;<>()[\]]+)"
    r"|(?P<number>\b\d+\b)"
)

# The languages a fenced block is highlighted in, by the name after its
# fence.
SOURCE_TOKENS = {
    "rust": RUST_TOKEN,
    "c": C_TOKEN,
    "nix": NIX_TOKEN,
    "sh": SHELL_TOKEN,
    "bash": SHELL_TOKEN,
    "shell": SHELL_TOKEN,
}


def source_text(source, tokens):
    """Source as HTML, with each token's spans closed at line boundaries."""
    out = []
    pos = 0
    for m in tokens.finditer(source):
        out.append(html.escape(source[pos : m.start()], quote=False))

        # Separate spans let a marked line enclose part of a multiline token.
        out.append(
            "\n".join(
                f'<span class="{m.lastgroup}">'
                + html.escape(part, quote=False)
                + "</span>"
                for part in m.group().split("\n")
            )
        )
        pos = m.end()
    out.append(html.escape(source[pos:], quote=False))
    return "".join(out)


def highlighted(lines, marked, tokens):
    """A block of source, highlighted; the 1-based lines in MARKED get a
    band."""
    body = []
    for n, text in enumerate(
        source_text("\n".join(lines), tokens).split("\n"), start=1
    ):
        if n in marked:
            text = f'<span class="mark">{text}</span>'
        body.append(text)
    return '<pre class="console source"><code>' + "\n".join(body) + "</code></pre>\n"


def plain(lines):
    return (
        '<pre\n  class="console"\n><code>'
        + "\n".join(html.escape(l, quote=False) for l in lines)
        + "</code></pre>\n"
    )


def table(rows, source, page):
    """A pipe table; the second row is the header's rule."""
    cells = [[c.strip() for c in r.strip().strip("|").split("|")] for r in rows]
    head, body = cells[0], cells[2:]
    out = ['<div class="table"><table>\n<thead><tr>']
    out += [f"<th>{inline(c, source, page)}</th>" for c in head]
    out.append("</tr></thead>\n<tbody>\n")
    for r in body:
        out.append(
            "<tr>"
            + "".join(f"<td>{inline(c, source, page)}</td>" for c in r)
            + "</tr>\n"
        )
    out.append("</tbody></table></div>\n")
    return "".join(out)


# A Markdown image of the site's, and its figure: a WebP with the PNG for
# browsers without, at the PNG's size, captioned with the image's text. The
# path climbs out of docs/, or out of docs/case-studies/, to site/img.
FIGURE = re.compile(r"!\[(.+)\]\((?:\.\./)+site/(img/[\w-]+)\.png\)")
FIGURE_HTML = """<figure class="shot">
  <picture>
    <source srcset="../{name}.webp" type="image/webp" />
    <img src="../{name}.png" width="{width}" height="{height}" loading="lazy" alt="{alt}" />
  </picture>
  <figcaption>{alt}</figcaption>
</figure>
"""


def figure(caption, name):
    """The figure for the screenshot at site/<name>.png, sized from the
    PNG's header."""
    header = (REPO / "site" / f"{name}.png").read_bytes()[16:24]
    width, height = int.from_bytes(header[:4], "big"), int.from_bytes(header[4:], "big")
    alt = html.escape(caption, quote=True)
    return FIGURE_HTML.format(name=name, width=width, height=height, alt=alt)


def parse(lines):
    """Markdown lines to blocks: (kind, payload)."""
    blocks = []
    i = 0
    while i < len(lines):
        line = lines[i]
        if not line.strip():
            i += 1
            continue

        # Fenced code. After the language, "{3,7}" marks lines of the block.
        if line.startswith("```"):
            lang, _, marks = line[3:].strip().partition(" ")
            marked = {int(n) for n in re.findall(r"\d+", marks)}
            j = i + 1
            while not lines[j].startswith("```"):
                j += 1
            blocks.append(("code", (lang, marked, lines[i + 1 : j])))
            i = j + 1
            continue

        # A screenshot on a line of its own: ![caption](../site/img/name.png),
        # or ../../site/img from a case study, the path the Markdown works
        # with on GitHub.
        if m := FIGURE.fullmatch(line.strip()):
            blocks.append(("figure", (m.group(1), m.group(2))))
            i += 1
            continue

        # Section heading.
        if line.startswith("## "):
            blocks.append(("h2", line[3:].strip()))
            i += 1
            continue

        # A table runs while lines start with a pipe.
        if line.startswith("|"):
            j = i
            while j < len(lines) and lines[j].startswith("|"):
                j += 1
            blocks.append(("table", lines[i:j]))
            i = j
            continue

        # A list: items start with "- ", continuation lines are indented.
        if line.startswith("- "):
            items = []
            while i < len(lines) and (
                lines[i].startswith("- ") or lines[i].startswith("  ")
            ):
                if lines[i].startswith("- "):
                    items.append(lines[i][2:].strip())
                else:
                    items[-1] += " " + lines[i].strip()
                i += 1
            blocks.append(("ul", items))
            continue

        # A paragraph runs to the next blank line.
        para = []
        while i < len(lines) and lines[i].strip() and not lines[i].startswith("```"):
            para.append(lines[i].strip())
            i += 1
        blocks.append(("p", " ".join(para)))
    return blocks


def convert(cfg):
    source = cfg["source"]
    page = site_path(source)
    up = "../" * page.count("/") or "./"
    lines = (REPO / source).read_text().splitlines()
    title = lines[0].removeprefix("# ").strip()
    blocks = parse(lines[1:])
    colors_path, colors = colors_of(source)
    painted_blocks = set()

    out = [
        HEAD.format(
            title=html.escape(title, quote=False),
            description=html.escape(cfg["description"], quote=False).replace(
                '"', "&quot;"
            ),
            page=page,
            up=up,
            fonts=FONTS,
            source=source,
            eyebrow=cfg["eyebrow"],
        ),
        f"<h1>{inline(title, source, page)}</h1>\n",
    ]

    # The table of contents goes before the first section.
    headings = [b[1] for b in blocks if b[0] == "h2"]
    in_section = None
    paragraphs = 0
    for kind, payload in blocks:
        if kind == "h2":
            if in_section is None:
                out.append(f'\n<nav class="toc" aria-label="{cfg["toc"]}">\n')
                out.append(f'<p class="toc-head">{cfg["toc"]}</p>\n<ol>\n')
                out += [
                    f'<li><a href="#{slug(h)}">{inline(h, source, page)}</a></li>\n'
                    for h in headings
                ]
                out.append("</ol>\n</nav>\n")
            else:
                out.append("</section>\n")
            in_section = slug(payload)
            paragraphs = 0

            # The heading links to itself, so a reader can copy the section's URL.
            out.append(
                f'\n<section class="step">\n<h2 id="{in_section}"><a class="anchor" href="#{in_section}">{inline(payload, source, page)}</a></h2>\n'
            )
            continue
        if kind == "p":
            out.append(f"<p>{inline(payload, source, page)}</p>\n")
            paragraphs += 1

            # A figure of the page's follows the paragraph its config names.
            for section, paragraph, drawing in cfg["figures"]:
                if (section, paragraph) == (in_section, paragraphs):
                    out.append(drawing)
            continue
        if kind == "figure":
            out.append(figure(*payload))
            continue
        if kind == "ul":
            out.append(
                "<ul>\n"
                + "".join(f"<li>{inline(it, source, page)}</li>\n" for it in payload)
                + "</ul>\n"
            )
            continue
        if kind == "table":
            out.append(table(payload, source, page))
            continue
        lang, marked, code = payload
        if lang == "console":
            text = "\n".join(code)
            spans = colors.get(text, ())
            if spans:
                painted_blocks.add(text)
            out.append(console(code, spans))
        elif lang == "diff":
            out.append(diff(code))
        elif lang in SOURCE_TOKENS:
            out.append(highlighted(code, marked, SOURCE_TOKENS[lang]))
        else:
            out.append(plain(code))
    if in_section:
        out.append("</section>\n")

    href, eyebrow, next_title, text = cfg["next"]
    out.append(
        NEXT.format(
            href=posixpath.relpath(href, posixpath.dirname(page)),
            eyebrow=eyebrow,
            title=next_title,
            text=text,
        )
    )
    out.append(BUY.format(up=up))
    out.append(FOOT.format(up=up))

    # Colors kept for a block the page no longer has are a recording
    # the page has moved on from.
    stale = set(colors) - painted_blocks
    if stale:
        sys.exit(
            f"{colors_path.relative_to(REPO)} has colors for {len(stale)} console "
            f"block(s) {source} no longer has; record it again with "
            "tools/record-tutorials.py"
        )
    return page, "".join(out)


def main():
    for cfg in PAGES:
        page, text = convert(cfg)
        path = REPO / "site" / page
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)


if __name__ == "__main__":
    main()
