#!/usr/bin/env python3
"""Render the tutorials and case studies in docs/ as site pages.

    tools/md2html.py && nix fmt -- site/tutorials/*.html site/case-studies/*.html

docs/tutorial-*.md become site/tutorials/*.html and docs/case-studies/*.md
become site/case-studies/*.html, in the site's article style. Only the
Markdown these documents use is handled: headings, paragraphs, "- " lists,
tables, fenced code, inline code, links and bold. A relative link to a
document the site also publishes points at its page; any other relative link
points at the file on GitHub, since the site has no copy of the repository.
"""

import html
import posixpath
import re
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent

# Where the repository's files are on GitHub, for links the site cannot serve.
GITHUB_BLOB = "https://github.com/fzakaria/rewindvm/blob/main/"
GITHUB_TREE = "https://github.com/fzakaria/rewindvm/tree/main/"

# Documents the site publishes, by repository path, and their site paths.
LINK_MAP = {
    "docs/pmu.md": "counter-time.html",
    "docs/tutorial-nix.md": "tutorials/nix.html",
    "docs/tutorial-container.md": "tutorials/container.html",
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
# on the front page; the pins' positions are percentages of the 1440x600
# image.
DEVENV_FIGURE = """<div class="tour tour-article">
  <figure class="tour-shot">
    <div class="tour-frame">
      <a class="tour-link" href="../img/devenv-sigchld.png">
        <picture>
          <source srcset="../img/devenv-sigchld.webp" type="image/webp" />
          <img
            src="../img/devenv-sigchld.png"
            width="1440"
            height="600"
            loading="lazy"
            alt="The Rewind desktop app on the failing devenv run at step 1,167, compared with the passing run 96da9ada: the build log stops at running 1 test, SIGCHLD has reached thread 41, and the run diverged at step 1,163. Opens the full-size image."
          />
        </picture>
      </a>
      <button type="button" class="pin pin-1" style="left: 90.3%; top: 16.7%" aria-label="1: the timeline">1</button>
      <button type="button" class="pin pin-2" style="left: 20.8%; top: 52.2%" aria-label="2: the build log">2</button>
      <button type="button" class="pin pin-3" style="left: 96.9%; top: 57%" aria-label="3: the step's event, SIGCHLD">3</button>
      <button type="button" class="pin pin-4" style="left: 97.2%; top: 80%" aria-label="4: where the run diverged">4</button>
    </div>
    <figcaption>
      The app on the failing run ed301877 at step 1,167, compared with the passing run 96da9ada.
    </figcaption>
  </figure>
  <ol class="tour-cards">
    <li class="tour-card" id="tour-1" tabindex="0">
      <h3>The timeline</h3>
      <p>Step 1,167 of 1,235, four steps after the run left the passing one.</p>
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
      <p>The run left 96da9ada at step 1,163, where the shell starts the task's script.</p>
    </li>
  </ol>
</div>
"""

# Each page: its source, where it goes, the line above its title, its meta
# description, what its table of contents is called, an optional figure
# with the section and the paragraph of that section it follows, and the
# card that closes it.
PAGES = [
    dict(
        source="docs/tutorial-nix.md",
        eyebrow="Tutorial &middot; Nix",
        description="Install Rewind VM, find the thread interleaving that breaks a Nix derivation's tests, look at the crash step by step, fork it, and check the fix.",
        toc="Steps",
        figure=("scrub-it-in-the-app", 1, NIX_FIGURE),
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
        figure=("scrub-it-in-the-app", 1, CONTAINER_FIGURE),
        next=(
            "tutorials/advanced.html",
            "Tutorial &middot; Advanced",
            "Advanced features",
            "Watchpoints, the kernel's side of a crash, tools inside the VM, and the rest.",
        ),
    ),
    dict(
        source="docs/tutorial-advanced.md",
        eyebrow="Tutorial &middot; Advanced",
        description="Short recipes on Rewind VM's other tools: watchpoints, kernel breakpoints, your own gdb, tools inside the VM, more CPUs, steering the perturbation, comparing and sharing runs.",
        toc="Recipes",
        figure=None,
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
        description="Rewind VM's first run of Nix's functional tests hit a SIGPIPE in gc-closure.sh that no one had reported: a pipe into head -n1 under pipefail, and bash writing a two-line printf in two writes.",
        toc="Sections",
        figure=None,
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
        figure=None,
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
        figure=("where-the-third-line-went", 2, DEVENV_FIGURE),
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
        <a href="{up}counter-time.html">Counter time</a>
      </nav>
    </header>

    <!-- Generated by tools/md2html.py from {source}.
         The transcripts are real output and are copied as they are; edit
         the Markdown and run the script again. -->
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


def site_path(source):
    """The page a document becomes, relative to the site's root."""
    return LINK_MAP[source]


def slug(text):
    """GitHub's heading anchors: lowercase, punctuation dropped, spaces to dashes."""
    text = re.sub(r"[^\w\- ]", "", text.lower())
    return text.replace(" ", "-")


def resolve(href, source, page):
    """A link in SOURCE as it must read on PAGE."""
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


def console(lines):
    """A console block: "$ " starts a command, "> " continues one."""
    body = []
    in_command = False
    for line in lines:
        prompt = (
            "$"
            if line.startswith("$ ")
            else ">" if in_command and line.startswith("> ") else None
        )
        in_command = prompt is not None
        if prompt is None:
            body.append(html.escape(line, quote=False))
            continue
        body.append(
            f'<span class="cmd"><span class="prompt">{html.escape(prompt)}</span> '
            + html.escape(line[2:], quote=False)
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


def rust_line(line):
    """One line of Rust as HTML, each token in a span named for its kind."""
    out = []
    pos = 0
    for m in RUST_TOKEN.finditer(line):
        out.append(html.escape(line[pos : m.start()], quote=False))
        out.append(
            f'<span class="{m.lastgroup}">'
            + html.escape(m.group(), quote=False)
            + "</span>"
        )
        pos = m.end()
    out.append(html.escape(line[pos:], quote=False))
    return "".join(out)


def rust(lines, marked):
    """A Rust block, highlighted; the 1-based lines in MARKED get a band."""
    body = []
    for n, line in enumerate(lines, start=1):
        text = rust_line(line)
        if n in marked:
            text = f'<span class="mark">{text}</span>'
        body.append(text)
    return '<pre class="console rust"><code>' + "\n".join(body) + "</code></pre>\n"


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
    up = "../" * page.count("/")
    lines = (REPO / source).read_text().splitlines()
    title = lines[0].removeprefix("# ").strip()
    blocks = parse(lines[1:])

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

            # The page's figure follows the paragraph its config names.
            if cfg["figure"] and cfg["figure"][:2] == (in_section, paragraphs):
                out.append(cfg["figure"][2])
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
            out.append(console(code))
        elif lang == "diff":
            out.append(diff(code))
        elif lang == "rust":
            out.append(rust(code, marked))
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
    out.append(FOOT.format(up=up))
    return page, "".join(out)


def main():
    for cfg in PAGES:
        page, text = convert(cfg)
        path = REPO / "site" / page
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)


main()
