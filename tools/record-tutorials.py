#!/usr/bin/env python3
"""Record the tutorials: run every command they show and write the output in.

    tools/record-tutorials.py --rev HEAD              # every tutorial
    tools/record-tutorials.py --rev v0.3.2 nix        # one of them

Each tutorial is a template in docs/templates/, written as the page reads,
and this writes the page, docs/tutorial-<name>.md, which tools/md2html.py
then turns into the site's. A template is Markdown with three additions.

A console block whose info string starts with `run` is run, one `$ ` line at
a time, in a shell in the tutorial's work directory, and written as a plain
console block with each command's output under it. Options follow `run`:

    name=NAME     keep the output, as {{out:NAME}}, a file, for later commands
    show=SLICES   print only these lines, Python slices such as `:1,-2:`,
                  with `...` where lines were left out
    elide         shorten every store path to /nix/store/...-name
    cut=N         end lines longer than N characters there, with ` ...`
    hide          leave the output out, for a command whose output is not
                  rewind's and says nothing, such as docker build's progress
    time=VAR      the block's wall time in whole seconds, as {{VAR}}
    bg=REGEX      run the command in the background until its output matches,
                  then run the `after` directive that follows, then wait

A console block without `run` is copied as it is: output from the host,
which no build of rewind changes.

`{{NAME}}` anywhere, prose or command, is a value: `{{NAME|short}}` is its
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
emptied before each tutorial, so `rewind ls`, the app's Runs panel and
wall times are those of a reader's fresh install.

Any command that fails, capture that does not match, or assertion that does
not hold stops the recording with the template line, so a build that tells
the story differently is noticed rather than published.
"""

import argparse
import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
TEMPLATES = REPO / "docs" / "templates"

# The flake outputs a recording runs: the command and the app, the
# release tarballs the container tutorial installs, and the guest every
# run boots, which RECORDED_WITH names.
OUTPUTS = ["rewind", "app", "release", "release-debug", "initrd", "kernel"]

# Which guest the tutorials were recorded with. A run's id hashes the
# guest's initramfs and kernel, so a release whose guest is another makes
# runs the tutorials do not show; the release workflow refuses one.
RECORDED_WITH = TEMPLATES / "recorded-with"
GUEST = ["initrd", "kernel"]

# A store path's hash, to shorten as the tutorials do.
STORE_HASH = re.compile(r"/nix/store/[0-9a-z]{32}-")

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


def build(rev):
    """The store paths of OUTPUTS at `rev`, from the repository as git has
    it, so neither uncommitted files nor build products go into the store."""
    commit = subprocess.run(
        ["git", "-C", str(REPO), "rev-parse", rev],
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()
    refs = [f"git+file://{REPO}?rev={commit}#{o}" for o in OUTPUTS]
    paths = subprocess.run(
        ["nix", "build", "--no-link", "--print-out-paths", *refs],
        capture_output=True,
        text=True,
        check=True,
    ).stdout.split()
    return dict(zip(OUTPUTS, paths)), commit


class Recording:
    """One tutorial being recorded: its values, its shell's directory and
    environment, and the page so far."""

    def __init__(self, name, work, home, paths):
        self.name = name
        self.work = work
        self.cwd = work
        self.values = {
            "dir": str(TEMPLATES),
            "work": str(work),
            "home": str(home),
            "release": paths["release"],
            "release_debug": paths["release-debug"],
        }
        self.env = dict(os.environ)
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

    def shell(self, command, check=True):
        """Runs `command` the way the page's reader would, returning what it
        printed, standard error included. `cd` changes the directory for the
        commands after it, as in the reader's shell."""
        if m := re.fullmatch(r"cd (\S+)", command):
            self.cwd = (self.cwd / m.group(1)).resolve()
            return ""
        done = subprocess.run(
            ["bash", "-c", command],
            cwd=self.cwd,
            env=self.env,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )
        if check and done.returncode != 0:
            raise Failed(f"exit {done.returncode}: {command}\n{done.stdout}")
        return done.stdout

    def background(self, command, until, after):
        """Runs `command` until its output matches `until`, then `after`,
        then waits for it."""
        proc = subprocess.Popen(
            ["bash", "-c", command],
            cwd=self.cwd,
            env=self.env,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )
        assert proc.stdout is not None
        lines = []
        for line in proc.stdout:
            lines.append(line)
            if re.search(until, line):
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
            if "bg" in opts:
                if after is None:
                    raise Failed("a bg block needs an after directive")
                output = self.background(command, opts["bg"], self.fill(after))
            else:
                output = self.shell(command, check=False)
            raw.append(output)
            lines = [line.rstrip() for line in output.splitlines()]
            if "hide" in opts:
                lines = []
            if "elide" in opts:
                lines = [STORE_HASH.sub("/nix/store/...-", line) for line in lines]
            if "show" in opts:
                lines = slices(lines, opts["show"])
            if "cut" in opts:
                n = int(opts["cut"])
                lines = [
                    line if len(line) <= n else line[:n] + " ..." for line in lines
                ]
            if shown and shown[-1] != "" and not shown[-1].startswith("$ "):
                shown.append("")
            shown.append(f"$ {command}")
            shown.extend(lines)
        self.last = "".join(raw)
        if "name" in opts:
            (self.work / f"{opts['name']}.out").write_text(self.last)
        if "time" in opts:
            self.values[opts["time"]] = str(round(time.monotonic() - start))
        return ["```console", *shown, "```"]

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
            out.append("...")
        out.extend(lines[j] for j in rng)
        end = rng[-1] + 1
    if end < len(lines):
        out.append("...")
    return out


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


def record(name, work, home, paths):
    """Records one tutorial and writes its page."""
    template = TEMPLATES / f"tutorial-{name}.md"
    rec = Recording(name, work, home, paths)
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
    out = REPO / "docs" / f"tutorial-{name}.md"
    out.write_text("\n".join(collapse(page)) + "\n")

    # Formatted as CI checks it, so a recording commits as it is.
    subprocess.run(
        ["nix", "fmt", "--", str(out)], cwd=REPO, check=True, capture_output=True
    )
    print(f"wrote {out.relative_to(REPO)}")


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
        "tutorials", nargs="*", help="names, such as nix; all by default"
    )
    args = parser.parse_args()

    names = args.tutorials or sorted(
        p.stem.removeprefix("tutorial-") for p in TEMPLATES.glob("tutorial-*.md")
    )
    paths, commit = build(args.rev)
    print(f"recording with {commit[:12]}")
    # Each tutorial starts from an empty home, as a reader's first does,
    # so a page does not depend on which tutorials were recorded with it.
    home = args.work / "home"
    for name in names:
        shutil.rmtree(home, ignore_errors=True)
        home.mkdir(parents=True)
        work = args.work / name
        shutil.rmtree(work, ignore_errors=True)
        work.mkdir(parents=True)
        record(name, work, home, paths)

    lines = [
        "# The guest the tutorials were recorded with, by tools/record-tutorials.py.",
        "# The release workflow refuses a tag whose guest is another.",
        *(f"{name} {paths[name]}" for name in GUEST),
    ]
    RECORDED_WITH.write_text("\n".join(lines) + "\n")
    print(f"wrote {RECORDED_WITH.relative_to(REPO)}")


if __name__ == "__main__":
    main()
