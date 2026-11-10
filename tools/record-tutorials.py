#!/usr/bin/env python3
"""Record the tutorials: run every command they show and write the output in.

    tools/record-tutorials.py --rev HEAD              # every tutorial
    tools/record-tutorials.py --rev v0.3.2 nix        # one of them
    tools/record-tutorials.py --local properties      # before a push
    tools/record-tutorials.py --local --case-studies  # the case studies

Each tutorial is a template in docs/templates/, written as the page reads,
and this writes the page, docs/tutorial-<name>.md, which tools/md2html.py
then turns into the site's. The case studies are recorded the same way,
with --case-studies: docs/templates/case-<name>.md writes
docs/case-studies/<name>.md. A template is Markdown with three additions.

A console block whose info string starts with `run` is run, one `$ ` line at
a time, in a shell in the tutorial's work directory, and written as a plain
console block with each command's output under it. Options follow `run`:

    name=NAME     keep the output, as {{out:NAME}}, a file, for later commands
    show=SLICES   print only these lines, Python slices such as `:1,-2:`,
                  with `...` where lines were left out
    keep=REGEX    print only the lines REGEX matches, with `...` where lines
                  were left out; options split at spaces, so `\\s` for one
    elide         shorten every store path to /nix/store/...-name
    cut=N         end lines longer than N characters there, with ` ...`
    hide          leave the output out, for a command whose output is not
                  rewind's and says nothing, such as docker build's progress
    time=VAR      the block's wall time in whole seconds, as {{VAR}}
    bg=REGEX      run the command in the background until its output matches,
                  then run the `after` directive that follows, then wait

A console block without `run` is copied as it is: output from the host,
which no build of rewind changes.

Rewind colors its output on a terminal, so a shown command runs with
REWIND_COLOR=always unless it pipes rewind's output into another program,
where a reader's rewind writes plain text too. The page keeps the plain
text, which GitHub shows; the colored stretches of each block go to
docs/colors/<page>.json, named as the page is, by line and column, for
tools/md2html.py to paint the site's page with. Captures, named outputs and every command
the page does not show see plain text.

`{{NAME}}` anywhere, prose or command, is a value, among them {{dir}}, the
templates' directory, {{work}}, the page's own, and {{site}}, the site's: `{{NAME|short}}` is its
first eight characters, a run id as the tutorials write it, and
`{{NAME|and}}` joins its words as "a, b and c". Values are set by one-line
HTML comments, which are not written out:

    <!-- let NAME = VALUE -->     a fixed value
    <!-- set NAME: COMMAND -->    the output of a command the page does not show
    <!-- run: COMMAND -->         a command the page does not show
    <!-- env NAME: COMMAND -->    an environment variable for the commands
                                  after it, set to a command's output
    <!-- capture NAME: REGEX -->  group 1 of REGEX in the last block's output
    <!-- assert: COMMAND -->      the page's story, which must exit 0
    <!-- after: COMMAND -->       what a `bg` block waits for, run after it
    <!-- diff: FILE -->           a patch next to the template, as a diff
                                  block; `run: patch ... < {{dir}}/FILE`
                                  applies it
    <!-- screenshot PATH: ARGS ;; STEP ;; ... -->
                                  the desktop app opened with ARGS, then
                                  each STEP: `click X Y`, `type TEXT` and
                                  Enter, `key NAME`, or `wait SECONDS`;
                                  saved as PATH.png and PATH.webp under
                                  the repository

The commands run against a run directory of the recording's own, {{home}},
emptied before each page, so `rewind ls`, the app's Runs panel and
wall times are those of a reader's fresh install.

Any command that fails, capture that does not match, or assertion that does
not hold stops the recording with the template line, so a build that tells
the story differently is noticed rather than published.

The pages name workloads by GitHub, as `github:fzakaria/rewindvm#bank`.
With --local, the commands build them from the recorded commit instead,
and `git clone https://github.com/fzakaria/rewindvm` clones that commit,
while the page still shows GitHub: a page about an example that is not
pushed yet can be recorded, and the runs are the ones a reader's build of
the same source makes. Such a clone prints nothing, where a reader's
prints "Cloning into 'rewindvm'...".
"""

import argparse
import enum
import hashlib
import json
import os
import re
import shlex
import shutil
import stat
import subprocess
import sys
import tempfile
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
TEMPLATES = REPO / "docs" / "templates"


def git_repo() -> Path:
    """The git repository holding the recorded commit. A jj workspace
    other than the first has no .git of its own; jj names the repository
    its commits are in."""
    if (REPO / ".git").exists():
        return REPO
    git_dir = subprocess.run(
        ["jj", "git", "root"], cwd=REPO, capture_output=True, text=True, check=True
    ).stdout.strip()
    return Path(git_dir).parent


GIT_REPO = git_repo()

# The repository as the pages name it.
GITHUB_FLAKE = re.compile(r"github:fzakaria/rewindvm(\?[^#\s'\"]*)?")

# A page's clone of the repository, and the directory it clones into.
GITHUB_CLONE = re.compile(r"git clone https://github\.com/fzakaria/rewindvm(?: (\S+))?")

# The flake outputs a recording runs: the command and the app, the
# release tarballs the container tutorial installs, and the guest every
# run boots, which RECORDED_WITH names.
OUTPUTS = ["rewind", "app", "release", "release-debug", "initrd", "kernel"]


class Kind(enum.Enum):
    """What a template is, by the start of its file's name: a tutorial,
    whose page goes next to the others in docs/, or a case study, whose
    page goes in docs/case-studies/."""

    TUTORIAL = "tutorial-"
    CASE_STUDY = "case-"

    def template(self, name):
        return TEMPLATES / f"{self.value}{name}.md"

    def page(self, name):
        """The page the template called `name` writes."""
        if self == Kind.CASE_STUDY:
            return REPO / "docs" / "case-studies" / f"{name}.md"
        return REPO / "docs" / f"tutorial-{name}.md"


# Which guest the tutorials were recorded with. A run's id hashes the
# guest's initramfs and kernel, so a release whose guest is another makes
# runs the tutorials do not show; the release workflow refuses one.
RECORDED_WITH = TEMPLATES / "recorded-with"
GUEST = ["initrd", "kernel"]

# A store path's hash, to shorten as the tutorials do.
STORE_HASH = re.compile(r"/nix/store/[0-9a-z]{32}-")
ELIDED_STORE = "/nix/store/...-"

# Where each page's colored stretches go, and how a shown command asks
# rewind for color, on a terminal that draws it, as the reader's does: gdb,
# which rewind asks to style its output then, warns on a dumb one.
COLORS = REPO / "docs" / "colors"
COLOR_ENV = {"REWIND_COLOR": "always", "TERM": "xterm-256color"}

# The escape sequences rewind colors with: Select Graphic Rendition, and
# the parameters it uses, by the class the site paints each in. Any other
# escape stops the recording, so a new style is given a class first.
SGR = re.compile(r"\x1b\[([0-9;]*)m")

# Erase in Line, which a program redrawing a line, such as meson's progress
# line, writes after a carriage return. The output is read with universal
# newlines, so the carriage return has already ended the line, and the
# escape is dropped.
ERASE_LINE = "\x1b[K"
ESCAPE = "\x1b"
SGR_RESET = {"", "0"}
SGR_ATTRIBUTES = {"1": "bold", "2": "dim", "4": "underline"}
SGR_NORMAL_INTENSITY = "22"
SGR_NO_UNDERLINE = "24"
SGR_DEFAULT_FOREGROUND = "39"
SGR_FOREGROUNDS = {
    "30": "black",
    "31": "red",
    "32": "green",
    "33": "yellow",
    "34": "blue",
    "35": "magenta",
    "36": "cyan",
    "37": "white",
}

# What stands for lines left out of a block.
ELIDED = "..."

# What a cut line ends with.
CUT_MARK = " ..."

# How the desktop app is screenshotted: a virtual screen the size of its
# window, mesa's software Vulkan, and long enough to draw.
SCREEN = "1440x900x24"
DISPLAY = ":97"
LAVAPIPE = "/run/opengl-driver/share/vulkan/icd.d/lvp_icd.x86_64.json"
DRAW_SECONDS = 10
WEBP_QUALITY = "90"
# Where the pointer waits after a click: the screen's corner, on no control.
PARKED = "1439 899"

DIRECTIVE = re.compile(
    r"^<!-- (let|set|env|run|capture|assert|after|diff|screenshot)\b\s*(.*?)\s*-->$"
)
VALUE = re.compile(r"\{\{([a-z_:.0-9]+)(?:\|([a-z]+))?\}\}")


class Failed(Exception):
    pass


class Color(enum.Enum):
    """Whether a command's rewind is asked for color: a shown command whose
    output reaches the reader's terminal is, any other is not."""

    SHOWN = "shown"
    PLAIN = "plain"


class Styled:
    """A line of output as plain text, with the class each character is
    painted in, None for plain."""

    def __init__(self, text, classes=None):
        self.text = text
        self.classes = classes if classes is not None else [None] * len(text)

    def __add__(self, other):
        return Styled(self.text + other.text, self.classes + other.classes)

    def __getitem__(self, cut):
        return Styled(self.text[cut], self.classes[cut])

    def __len__(self):
        return len(self.text)

    def rstrip(self):
        return self[: len(self.text.rstrip())]

    def sub(self, pattern, replacement):
        """The line with `pattern` replaced, the replacement painted as the
        first character it replaces."""
        out = Styled("")
        end = 0
        for m in pattern.finditer(self.text):
            out = out + self[end : m.start()]
            out = out + Styled(
                replacement, [self.classes[m.start()]] * len(replacement)
            )
            end = m.end()
        return out + self[end:]

    def spans(self):
        """Each stretch of one class: its start, end and class."""
        out = []
        start = 0
        for i in range(1, len(self.classes) + 1):
            if i < len(self.classes) and self.classes[i] == self.classes[start]:
                continue
            if self.classes[start] is not None:
                out.append((start, i, self.classes[start]))
            start = i
        return out


def styled_lines(output):
    """`output`, which may color with SGR escapes, as Styled lines."""
    output = output.replace(ERASE_LINE, "")

    attributes = set()
    foreground = None
    text = []
    classes = []
    pos = 0
    for m in [*SGR.finditer(output), None]:
        # The text before this escape, in the style so far.
        chunk = output[pos : m.start() if m else len(output)]
        if ESCAPE in chunk:
            raise Failed(f"an escape the site cannot paint: {chunk!r}")
        names = sorted(attributes) + ([foreground] if foreground else [])
        style = " ".join(names) or None
        text.append(chunk)
        classes.extend([style] * len(chunk))
        if m is None:
            break
        pos = m.end()

        # The escape's parameters change the style.
        for code in m.group(1).split(";"):
            if code in SGR_RESET:
                attributes.clear()
                foreground = None
            elif code in SGR_ATTRIBUTES:
                attributes.add(SGR_ATTRIBUTES[code])
            elif code == SGR_NORMAL_INTENSITY:
                attributes -= {"bold", "dim"}
            elif code == SGR_NO_UNDERLINE:
                attributes.discard("underline")
            elif code == SGR_DEFAULT_FOREGROUND:
                foreground = None
            elif code in SGR_FOREGROUNDS:
                foreground = SGR_FOREGROUNDS[code]
            else:
                raise Failed(f"an SGR parameter the site cannot paint: {m.group(0)!r}")

    # Split into lines, each keeping its characters' classes.
    whole = Styled("".join(text), classes)
    lines = []
    start = 0
    for i, c in enumerate(whole.text):
        if c == "\n":
            lines.append(whole[start:i])
            start = i + 1
    if start < len(whole.text):
        lines.append(whole[start:])
    return lines


def pipes_rewind(command):
    """Whether `command` pipes rewind's output into another program, so a
    reader's rewind writes plain text."""
    lexer = shlex.shlex(command, posix=True, punctuation_chars=True)
    # A flake reference's # starts its output's name, not a comment.
    lexer.commenters = ""
    seen = False
    for token in lexer:
        if token == "rewind":
            seen = True
        elif seen and token == "|":
            return True
    return False


def build(rev):
    """The store paths of OUTPUTS at `rev`, from the repository as git has
    it, so neither uncommitted files nor build products go into the store."""
    commit = subprocess.run(
        ["git", "-C", str(GIT_REPO), "rev-parse", rev],
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()
    refs = [f"git+file://{GIT_REPO}?rev={commit}#{o}" for o in OUTPUTS]
    paths = subprocess.run(
        ["nix", "build", "--no-link", "--print-out-paths", *refs],
        capture_output=True,
        text=True,
        check=True,
    ).stdout.split()
    return dict(zip(OUTPUTS, paths)), commit


class Recording:
    """One page being recorded: its values, its shell's directory and
    environment, and the page so far."""

    def __init__(self, name, work, home, paths, commit=None):
        self.name = name
        # The commit the commands build and clone `github:fzakaria/rewindvm`
        # from instead, with --local.
        self.commit = commit
        self.work = work
        self.cwd = work
        self.values = {
            "dir": str(TEMPLATES),
            "site": str(REPO / "site"),
            "work": str(work),
            "home": str(home),
            "release": paths["release"],
            "release_debug": paths["release-debug"],
        }
        # The caller's REWIND_ variables are left out: a REWIND_KERNEL that
        # a development shell exported would boot every run on that shell's
        # guest rather than the recorded commit's.
        self.env = {k: v for k, v in os.environ.items() if not k.startswith("REWIND_")}
        self.env["PATH"] = (
            f"{paths['rewind']}/bin:{paths['app']}/bin:{self.env['PATH']}"
        )
        # A fresh debuginfod cache, so gdb downloads what a reader's does.
        self.env["DEBUGINFOD_CACHE_PATH"] = str(work / "debuginfod")
        self.env["TERM"] = "dumb"
        self.env["REWIND_HOME"] = str(home)
        self.app = f"{paths['app']}/bin/rewind-app"
        self.last = ""
        self.out = []
        # Each shown block's colored stretches, for the site.
        self.colors = []

    def fill(self, text):
        """`text` with its values put in."""

        def one(m):
            key, how = m.group(1), m.group(2)
            if key.startswith("out:"):
                path = self.work / f"{key[4:]}.out"
                if not path.exists():
                    raise Failed(f"no block named {key[4:]}")
                return str(path)
            if key not in self.values:
                raise Failed(f"no value {key}")
            value = self.values[key]
            if how == "short":
                return value[:8]
            if how == "and":
                words = value.split()
                return (
                    words[0]
                    if len(words) == 1
                    else ", ".join(words[:-1]) + " and " + words[-1]
                )
            if how:
                raise Failed(f"no filter {how}")
            return value

        return VALUE.sub(one, text)

    def localize(self, command):
        """`command` with the repository's GitHub flake and clone replaced
        by the recorded commit, with --local, keeping any query it has."""
        if self.commit is None:
            return command

        # A clone of the repository checks out the recorded commit. The
        # clone checks nothing out itself: a quiet clone still prints the
        # detached HEAD advice when the source's HEAD is on no branch,
        # which a jj repository's is whenever the working copy is ahead of
        # main, and the quiet checkout after it prints nothing.
        if m := GITHUB_CLONE.fullmatch(command):
            into = shlex.quote(m.group(1) or "rewindvm")
            return f"git clone -q --no-checkout {shlex.quote(str(GIT_REPO))} {into} && git -C {into} checkout -q {self.commit}"

        local = f"git+file://{GIT_REPO}?rev={self.commit}"

        def one(m):
            query = m.group(1)
            return local + ("&" + query[1:] if query else "")

        return GITHUB_FLAKE.sub(one, command)

    def env_for(self, color):
        """The commands' environment, asking rewind for color when `color`
        is Color.SHOWN."""
        if color == Color.SHOWN:
            return self.env | COLOR_ENV
        return self.env

    def shell(self, command, check=True, color=Color.PLAIN):
        """Runs `command` the way the page's reader would, returning what it
        printed, standard error included, colored when `color` is
        Color.SHOWN. `cd` changes the directory for the commands after it, as
        in the reader's shell."""
        command = self.localize(command)
        if m := re.fullmatch(r"cd (\S+)", command):
            self.cwd = (self.cwd / m.group(1)).resolve()
            return ""
        done = subprocess.run(
            ["bash", "-c", command],
            cwd=self.cwd,
            env=self.env_for(color),
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )
        if check and done.returncode != 0:
            raise Failed(f"exit {done.returncode}: {command}\n{done.stdout}")
        return done.stdout

    def background(self, command, until, after, color=Color.PLAIN):
        """Runs `command` until its output matches `until`, then `after`,
        then waits for it; colored when `color` is Color.SHOWN."""
        command = self.localize(command)
        proc = subprocess.Popen(
            ["bash", "-c", command],
            cwd=self.cwd,
            env=self.env_for(color),
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )
        assert proc.stdout is not None
        lines = []
        for line in proc.stdout:
            lines.append(line)
            if re.search(until, SGR.sub("", line)):
                break
        else:
            raise Failed(f"never printed {until!r}: {command}\n{''.join(lines)}")
        self.shell(after)
        lines.extend(proc.stdout)
        proc.wait()
        return "".join(lines)

    def block(self, info, body, after):
        """Runs a `run` block and returns it as the page shows it."""
        opts = dict(o.split("=", 1) if "=" in o else (o, "") for o in info.split()[2:])
        start = time.monotonic()
        shown = []
        raw = []
        for command in [c[2:] for c in body if c.startswith("$ ")]:
            command = self.fill(command)
            color = Color.PLAIN if pipes_rewind(command) else Color.SHOWN
            if "bg" in opts:
                if after is None:
                    raise Failed("a bg block needs an after directive")
                output = self.background(command, opts["bg"], self.fill(after), color)
            else:
                output = self.shell(command, check=False, color=color)
            raw.append(SGR.sub("", output))
            lines = [line.rstrip() for line in styled_lines(output)]
            if "hide" in opts:
                lines = []
            if "elide" in opts:
                lines = [line.sub(STORE_HASH, ELIDED_STORE) for line in lines]
            if "show" in opts:
                lines = [
                    Styled(line) if isinstance(line, str) else line
                    for line in slices(lines, opts["show"])
                ]
            if "keep" in opts:
                lines = kept(lines, re.compile(opts["keep"]))
            if "cut" in opts:
                n = int(opts["cut"])
                lines = [
                    line if len(line) <= n else line[:n] + Styled(CUT_MARK)
                    for line in lines
                ]
            if shown and shown[-1].text != "" and not shown[-1].text.startswith("$ "):
                shown.append(Styled(""))
            shown.append(Styled(f"$ {command}"))
            shown.extend(lines)
        self.last = "".join(raw)
        if "name" in opts:
            (self.work / f"{opts['name']}.out").write_text(self.last)
        if "time" in opts:
            self.values[opts["time"]] = str(round(time.monotonic() - start))

        # The block's colored stretches, by line within the block.
        spans = [[i, *span] for i, line in enumerate(shown) for span in line.spans()]
        text = [line.text for line in shown]
        if spans:
            self.colors.append({"text": "\n".join(text), "spans": spans})
        return ["```console", *text, "```"]

    def directive(self, kind, rest):
        if kind == "let":
            name, value = (s.strip() for s in rest.split("=", 1))
            self.values[name] = self.fill(value)
        elif kind == "set":
            name, command = (s.strip() for s in rest.split(":", 1))
            value = self.shell(self.fill(command)).strip()
            if not value:
                raise Failed(f"{name} came out empty: {command}")
            self.values[name] = value
        elif kind == "env":
            name, command = (s.strip() for s in rest.split(":", 1))
            self.env[name] = self.shell(self.fill(command)).strip()
        elif kind == "run":
            self.shell(self.fill(rest.split(":", 1)[1].strip()))
        elif kind == "capture":
            name, pattern = (s.strip() for s in rest.split(":", 1))
            m = re.search(self.fill(pattern), self.last)
            if not m:
                raise Failed(
                    f"{pattern!r} is not in the last block's output:\n{self.last}"
                )
            self.values[name] = m.group(1)
        elif kind == "assert":
            command = self.fill(rest.split(":", 1)[1].strip())
            done = subprocess.run(
                ["bash", "-c", command],
                cwd=self.cwd,
                env=self.env,
                capture_output=True,
                text=True,
            )
            if done.returncode != 0:
                raise Failed(
                    f"the page's story does not hold: {command}\n{done.stdout}{done.stderr}"
                )
        elif kind == "diff":
            patch = (TEMPLATES / rest.split(":", 1)[1].strip()).read_text().splitlines()
            body = [
                line for line in patch if not line.startswith(("--- ", "+++ ", "diff "))
            ]
            return ["```diff", *body, "```"]
        elif kind == "screenshot":
            path, rest = (s.strip() for s in rest.split(":", 1))
            args, *steps = (s.strip() for s in self.fill(rest).split(";;"))
            self.screenshot(REPO / path, args, steps)
        return []

    def screenshot(self, path, args, steps):
        """The app opened with `args` on a virtual screen, driven by
        `steps`, saved as a PNG and a WebP."""
        png = path.with_suffix(".png")
        actions = []
        for step in steps:
            verb, _, arg = step.partition(" ")
            if verb == "click":
                x, y = arg.split()
                # Then out of the way, so no hover note covers the shot.
                actions.append(
                    f"xdotool mousemove {x} {y} click 1; sleep 1; xdotool mousemove {PARKED}"
                )
            elif verb == "type":
                actions.append(
                    f"xdotool type --delay 20 {shlex.quote(arg)}; xdotool key Return"
                )
            elif verb == "key":
                actions.append(f"xdotool key {shlex.quote(arg)}; sleep 1")
            elif verb == "wait":
                actions.append(f"sleep {float(arg)}")
            else:
                raise Failed(f"no screenshot step {verb}")
        script = f"""
            Xvfb {DISPLAY} -screen 0 {SCREEN} >/dev/null 2>&1 & xvfb=$!
            sleep 1
            env -u WAYLAND_DISPLAY DISPLAY={DISPLAY} VK_ICD_FILENAMES={LAVAPIPE} \\
              {self.app} {args} >/dev/null 2>&1 & app=$!
            sleep {DRAW_SECONDS}
            export DISPLAY={DISPLAY}
            {chr(10).join(actions)}
            import -display {DISPLAY} -window root {png}
            kill $app $xvfb
            oxipng -q -o 4 --strip safe {png}
            cwebp -quiet -q {WEBP_QUALITY} {png} -o {path.with_suffix('.webp')}
        """
        tools = ["xorg.xvfb", "imagemagick", "xdotool", "oxipng", "libwebp"]
        nix = [
            "nix",
            "shell",
            *[f"nixpkgs#{t}" for t in tools],
            "-c",
            "bash",
            "-c",
            script,
        ]
        # The app keeps its panels' sizes and its evaluation's days under
        # the user's config and state directories; a fresh pair for each
        # shot shows it as a reader first sees it, whatever this machine
        # has kept.
        fresh = Path(tempfile.mkdtemp(prefix="app-", dir=self.work))
        env = dict(
            self.env,
            XDG_CONFIG_HOME=str(fresh / "config"),
            XDG_STATE_HOME=str(fresh / "state"),
        )
        done = subprocess.run(nix, env=env, capture_output=True, text=True)
        if done.returncode != 0 or not png.exists():
            raise Failed(f"screenshot {path} failed:\n{done.stderr}")


def slices(lines, spec):
    """The lines `spec` picks, Python slices separated by commas, with
    `...` between pieces that are not next to each other."""
    picked = []
    for part in spec.split(","):
        a, b = part.split(":")
        sl = slice(int(a) if a else None, int(b) if b else None)
        picked.append(range(len(lines))[sl])
    out = []
    end = 0
    for i, rng in enumerate(picked):
        if len(rng) == 0:
            continue
        if rng[0] > end or (i > 0 and rng[0] != end):
            out.append(ELIDED)
        out.extend(lines[j] for j in rng)
        end = rng[-1] + 1
    if end < len(lines):
        out.append(ELIDED)
    return out


def kept(lines, pattern):
    """The lines `pattern` matches, with `...` in place of each run of
    lines it does not."""
    out = []
    for line in lines:
        if pattern.search(line.text):
            out.append(line)
        elif not out or out[-1].text != ELIDED:
            out.append(Styled(ELIDED))
    return out


def remove_tree(tree):
    """Removes the directory `tree` and everything in it, if it is there.
    A Nix store a page's commands made under the page's work directory
    keeps its directories read-only, where nothing can be unlinked, so a
    directory that refuses is made writable and tried again."""

    def writable(function, path, _error):
        os.chmod(os.path.dirname(path), stat.S_IRWXU)
        function(path)

    if tree.exists():
        shutil.rmtree(tree, onexc=writable)


def collapse(page):
    """The page with the blank lines a dropped directive leaves next to
    another blank line taken out, outside code blocks."""
    out = []
    fenced = False
    for line in page:
        if line.startswith("```"):
            fenced = not fenced
        if not fenced and line == "" and out and out[-1] == "":
            continue
        out.append(line)
    return out


def check_guest(home, paths):
    """Every run in `home` booted the guest of the recorded commit, the one
    RECORDED_WITH names, compared by content, since a release boots its own
    copy of it. A run on any other guest has an id no reader's install
    makes."""

    def digest(path):
        return hashlib.sha256(Path(path).read_bytes()).hexdigest()

    files = {"kernel": "vmlinux", "initrd": "initrd"}
    want = {name: digest(Path(paths[name]) / file) for name, file in files.items()}
    for manifest in sorted((home / "runs").glob("*/manifest.json")):
        spec = json.loads(manifest.read_text())["spec"]
        for name in files:
            if digest(spec[name]) != want[name]:
                raise Failed(
                    f"run {manifest.parent.name} booted {spec[name]}, not the "
                    f"{name} of the recorded commit, {paths[name]}"
                )


def record(name, kind, work, home, paths, commit=None):
    """Records one template of `kind` and writes its page."""
    template = kind.template(name)
    rec = Recording(name, work, home, paths, commit)
    lines = template.read_text().splitlines()
    page = []
    i = 0
    while i < len(lines):
        line = lines[i]
        where = f"{template.relative_to(REPO)}:{i + 1}"
        try:
            if m := DIRECTIVE.match(line):
                page.extend(rec.directive(m.group(1), m.group(2)))
                i += 1
                continue
            if line.startswith("```console run"):
                end = lines.index("```", i + 1)
                # The `after` directive for a bg block is the next line that is
                # not blank, which the formatter puts after the fence.
                after = None
                nxt = end + 1
                while nxt < len(lines) and not lines[nxt].strip():
                    nxt += 1
                if nxt < len(lines) and (m := DIRECTIVE.match(lines[nxt])):
                    if m.group(1) == "after":
                        after = m.group(2).split(":", 1)[1].strip()
                page.extend(rec.block(line, lines[i + 1 : end], after))
                i = nxt + 1 if after is not None else end + 1
                continue
            page.append(rec.fill(line))
            i += 1
        except Failed as e:
            sys.exit(f"{where}: {e}")
    try:
        check_guest(home, paths)
    except Failed as e:
        sys.exit(f"{template.relative_to(REPO)}: {e}")
    out = kind.page(name)
    out.write_text("\n".join(collapse(page)) + "\n")

    # The colored stretches beside it, or none for a page without color.
    colors = COLORS / f"{out.stem}.json"
    colors.unlink(missing_ok=True)
    written = [out]
    if rec.colors:
        COLORS.mkdir(exist_ok=True)
        colors.write_text(json.dumps(rec.colors, indent=1) + "\n")
        written.append(colors)

    # Formatted as CI checks it, so a recording commits as it is.
    subprocess.run(
        ["nix", "fmt", "--", *map(str, written)],
        cwd=REPO,
        check=True,
        capture_output=True,
    )
    for path in written:
        print(f"wrote {path.relative_to(REPO)}")


def main():
    parser = argparse.ArgumentParser(description="Record the tutorials.")
    parser.add_argument(
        "--rev", default="HEAD", help="the commit whose build to record with"
    )
    parser.add_argument(
        "--work",
        type=Path,
        default=Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache"))
        / "rewind-record",
        help="where each tutorial's commands run",
    )
    parser.add_argument(
        "--local",
        action="store_true",
        help="build the pages' github:fzakaria/rewindvm workloads from the recorded commit",
    )
    parser.add_argument(
        "--case-studies",
        action="store_true",
        help="record the case studies, docs/templates/case-*.md, instead",
    )
    parser.add_argument(
        "tutorials", nargs="*", help="names, such as nix; all by default"
    )
    args = parser.parse_args()

    kind = Kind.CASE_STUDY if args.case_studies else Kind.TUTORIAL
    names = args.tutorials or sorted(
        p.stem.removeprefix(kind.value) for p in TEMPLATES.glob(f"{kind.value}*.md")
    )
    paths, commit = build(args.rev)
    print(f"recording with {commit[:12]}")
    # Each page starts from an empty home, as a reader's first does, so a
    # page does not depend on which pages were recorded with it.
    home = args.work / "home"
    for name in names:
        remove_tree(home)
        home.mkdir(parents=True)
        work = args.work / name
        remove_tree(work)
        work.mkdir(parents=True)
        record(name, kind, work, home, paths, commit if args.local else None)

    # Only the tutorials name the guest: the release workflow checks theirs.
    if kind != Kind.TUTORIAL:
        return
    lines = [
        "# The guest the tutorials were recorded with, by tools/record-tutorials.py.",
        "# The release workflow refuses a tag whose guest is another.",
        *(f"{name} {paths[name]}" for name in GUEST),
    ]
    RECORDED_WITH.write_text("\n".join(lines) + "\n")
    print(f"wrote {RECORDED_WITH.relative_to(REPO)}")


if __name__ == "__main__":
    main()
