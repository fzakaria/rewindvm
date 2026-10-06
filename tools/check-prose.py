#!/usr/bin/env python3
"""Fail on the house style's banned constructions, and on one script bug.

Prose in this repository, including code comments, is held to one rule set so
that nobody has to remember it and no review has to argue about it. The checks
here are the mechanical ones. Everything else is a matter of taste and stays
that way.

    tools/check-prose.py            # the whole tree
    tools/check-prose.py README.md

Taken from tidemark and pointed at this repository's languages: the Rust
crates, the C in the QEMU patches and the guest module, and the site.
"""

import argparse
import re
import sys
from pathlib import Path

# Extensions whose prose is held to the rules. Data files are excluded: a JSON
# artifact full of store path names is not prose and would only produce noise.
CHECKED_SUFFIXES = {
    ".md",
    ".py",
    ".sh",
    ".nix",
    ".yml",
    ".rs",
    ".c",
    ".h",
    ".toml",
    ".html",
    ".css",
    ".txt",
}

# Directories of generated or third-party files: vendored crates are
# someone else's prose.
SKIP_DIRS = {
    ".git",
    ".jj",
    "target",
    "vendor",
    "result",
    "__pycache__",
    ".mypy_cache",
}

# The loudest signal of machine written text. Matched case insensitively on a
# word boundary.
BANNED_WORDS = [
    "delve",
    "tapestry",
    "meticulous",
    "pivotal",
    "intricate",
    "interplay",
    "underscore",
    "garner",
    "bolster",
    "vibrant",
    "bustling",
    "multifaceted",
    "seamless",
    "commendable",
    "ever-evolving",
    "realm",
    "testament",
    "showcase",
    "foster",
    "unlock",
    "elevate",
    "embark",
    "robust",
    "crucial",
    "essential",
    "profound",
    "nuanced",
    "holistic",
    "myriad",
    "plethora",
    "leverage",
]

# Connective tics and stock phrases.
BANNED_PHRASES = [
    "it is important to note",
    "it is worth noting",
    "it should be noted",
    "keep in mind that",
    "plays a crucial role",
    "stands as a testament",
    "in the heart of",
    "nestled in",
    "hidden gem",
    "it just works",
    "batteries included",
    "zero config",
    "from the ground up",
    "first-class citizen",
    "game changer",
    "despite these challenges",
    "challenges remain",
    "only time will tell",
    "to be honest",
    "let me be clear",
    "that is the whole point",
    "not just",
    "not only",
]

# An em dash. The style allows them rarely; this repository does without.
EM_DASH = "—"

# Not prose, but checked here too because nothing else reads the scripts:
# a pipe into a reader that stops early. `grep -q`, `grep -m` and `head`
# exit once they have seen enough, the writer's next line then dies of
# SIGPIPE, and under pipefail the script fails with status 141, but only
# when the reader wins the race. Write the output to a file and read that.
# A single `|`, not the `||` of "or else".
EARLY_EXIT_PIPE = re.compile(
    r"(?<!\|)\|(?!\|)\s*(head\b|grep\s+-\w*[qm]\b|grep\s+--(quiet|max-count))"
)

# The files whose commands the pipe rule reads.
SCRIPT_SUFFIXES = {".nix", ".sh"}

# The case studies reproduce this bug on purpose, Nix's gc-closure.sh among
# them, so the pipe rule leaves them alone.
PIPE_RULE_EXEMPT = Path("examples") / "case-studies"


def is_exempt(path):
    """Whether `path` lies under the directory the pipe rule leaves alone."""
    parts = path.resolve().parts
    exempt = PIPE_RULE_EXEMPT.parts
    return any(
        parts[i : i + len(exempt)] == exempt
        for i in range(len(parts) - len(exempt) + 1)
    )


def offenders(path):
    """Every (line number, rule, line) this file breaks."""
    found = []
    pipes_checked = path.suffix in SCRIPT_SUFFIXES and not is_exempt(path)
    for n, line in enumerate(path.read_text(errors="replace").splitlines(), start=1):
        if EM_DASH in line:
            found.append((n, "em dash", line.strip()))

        lowered = line.lower()
        for phrase in BANNED_PHRASES:
            if phrase in lowered:
                found.append((n, f"phrase {phrase!r}", line.strip()))

        for word in BANNED_WORDS:
            if re.search(rf"\b{re.escape(word)}\b", lowered):
                found.append((n, f"word {word!r}", line.strip()))

        if pipes_checked and EARLY_EXIT_PIPE.search(line):
            found.append((n, "pipe into a reader that exits early", line.strip()))

    return found


def walk(root):
    for path in sorted(root.rglob("*")):
        # Only the parts below the root: the Nix sandbox itself runs under
        # /build, which would otherwise match a build directory of the same name.
        if any(part in SKIP_DIRS for part in path.relative_to(root).parts):
            continue
        if path.is_file() and path.suffix in CHECKED_SUFFIXES:
            yield path


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument(
        "paths", nargs="*", help="files to check; the whole tree when omitted"
    )
    args = ap.parse_args()

    root = Path(__file__).resolve().parent.parent
    paths = [Path(p) for p in args.paths] if args.paths else list(walk(root))

    # This file names every banned word in order to check for them, so it is
    # the one file the rules cannot be applied to.
    paths = [p for p in paths if p.resolve() != Path(__file__).resolve()]

    total = 0
    for path in paths:
        for n, rule, line in offenders(path):
            rel = (
                path.resolve().relative_to(root)
                if path.resolve().is_relative_to(root)
                else path
            )
            print(f"{rel}:{n}: {rule}\n    {line}")
            total += 1

    if total:
        print(f"\n{total} violation(s)", file=sys.stderr)
        sys.exit(1)

    print(f"{len(paths)} files checked, no violations")


if __name__ == "__main__":
    main()
