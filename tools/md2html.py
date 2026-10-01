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
    "docs/case-studies/nix-gc-closure-sigpipe.md": "case-studies/nix-gc-closure-sigpipe.html",
    "docs/case-studies/nix-schema-migration-hang.md": "case-studies/nix-schema-migration-hang.html",
    "docs/case-studies/devenv-task-output-race.md": "case-studies/devenv-task-output-race.html",
    "examples/case-studies/flake.nix": GITHUB_BLOB + "examples/case-studies/flake.nix",
}

# The app's screenshot, shown in the Nix tutorial after the first paragraph
# of the section that introduces the app.
FIGURE = """<figure class="shot">
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

# Each page: its source, where it goes, the line above its title, its meta
# description, what its table of contents is called, an optional figure
# section, and the card that closes it.
PAGES = [
    dict(
        source="docs/tutorial-nix.md",
        eyebrow="Tutorial &middot; Nix",
        description="Install Rewind VM, find the thread interleaving that breaks a Nix derivation's tests, look at the crash step by step, fork it, and check the fix.",
        toc="Steps",
        figure_after="scrub-it-in-the-app",
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
        description="Run a test suite from a Docker image in Rewind VM, find the thread interleaving that breaks it, and replay the failure exactly. No Nix needed.",
        toc="Steps",
        figure_after=None,
        next=(
            "tutorials/nix.html",
            "Tutorial &middot; Nix",
            "A flaky Nix build",
            "The same bug as a Nix derivation, with more on inspecting the failure, forking it and fixing it.",
        ),
    ),
    dict(
        source="docs/case-studies/nix-gc-closure-sigpipe.md",
        eyebrow="Case study",
        description="Rewind VM's first run of Nix's functional tests hit a SIGPIPE in gc-closure.sh that no one had reported: a pipe into head -n1 under pipefail, and bash writing a two-line printf in two writes.",
        toc="Sections",
        figure_after=None,
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
        figure_after=None,
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
        figure_after=None,
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

        # Fenced code.
        if line.startswith("```"):
            lang = line[3:].strip()
            j = i + 1
            while not lines[j].startswith("```"):
                j += 1
            blocks.append(("code", (lang, lines[i + 1 : j])))
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
            out.append(
                f'\n<section class="step">\n<h2 id="{in_section}">{inline(payload, source, page)}</h2>\n'
            )
            continue
        if kind == "p":
            out.append(f"<p>{inline(payload, source, page)}</p>\n")
            if (
                cfg["figure_after"]
                and cfg["figure_after"] == in_section
                and FIGURE not in out
            ):
                out.append(FIGURE)
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
        lang, code = payload
        if lang == "console":
            out.append(console(code))
        elif lang == "diff":
            out.append(diff(code))
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
