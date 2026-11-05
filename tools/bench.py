#!/usr/bin/env python3
"""Measure Rewind builds on one machine and keep the history the site's
benchmark page (site/bench/) draws.

    nix run .#bench -- run --release v0.9.0 --release v0.10.0
    nix run .#bench -- run --build main=result/rewind-x86_64-linux.tar.gz
    nix run .#bench -- run --rewind wip=target/release/rewind --rounds 3 \\
        --only echo,hello --history /tmp/wip.json
    nix run .#bench -- compare --history /tmp/wip.json v0.10.0 wip
    nix run .#bench -- merge --history site/bench/history.json a.json b.json

A build is named LABEL=PATH, where PATH is a release tarball, the
directory it unpacks to, or a rewind executable whose guest the
environment names (REWIND_KERNEL, REWIND_INITRD). --release TAG downloads
that tag's release asset from GitHub. Every build runs the same
workloads against the same inputs, `nix build .#bench-inputs`
(nix/bench), which `nix run .#bench` passes as --inputs.

The workloads cover a short run, where boot and teardown are the whole
cost, and long ones, where the cost per step is: a program that only
computes, one that starts 2000 processes, one that unpacks 63 MiB into
the VM's memory, GNU hello's full nixpkgs build with and without
keyframes, and the commands that look at a run afterwards (replay, cat
at a late step) or explore one (check). The ones that need no Rewind run
natively too, so the page can say how much slower each is in the VM.
Each sample is wall, user and system seconds and peak RSS from GNU time,
plus the steps, keyframes and disk the run took.

Noise: a round measures every build once, in a fresh REWIND_HOME, in an
order shuffled per round, so a slow hour of the machine falls on every
build alike. Each build's round is pinned to the NUMA node that was
idlest as it started, and --workers runs that many at once, each on a
node of its own. The native workloads run first in each build's round,
on the same node, so a slowdown divides two measurements taken side by
side. The page draws
the median of the rounds and the range around it; compare says a
difference is real only when it is larger than both sides' spread.

Every build runs with --cpu host and --clock branches: the only CPU
model every release accepts on a host without ERMS, and the clock that
every release can be told to use. Numbers from different machines are
not comparable, so a record names its machine and compare refuses to mix
them.
"""

import argparse
import concurrent.futures
import datetime
import enum
import hashlib
import json
import lzma
import os
import platform
import random
import re
import shutil
import socket
import statistics
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import urllib.request

DESCRIPTION = (
    "Rewind builds measured on one machine by tools/bench.py: one record per "
    "build, each workload's samples one per round, every round in a fresh "
    "REWIND_HOME. See site/bench/."
)

RELEASE_URL = "https://github.com/fzakaria/rewindvm/releases/download/{tag}/rewind-x86_64-linux.tar.gz"
RELEASE_DIR = "rewind-x86_64-linux"
LAUNCHER = os.path.join("bin", "rewind")

# Options every run is given, so every build records the same machine.
# The epoch fixes the guest's clock, which otherwise starts at today, UTC.
EPOCH = "1790985600"
RUN_OPTIONS = [
    "--cpu",
    "host",
    "--clock",
    "branches",
    "--epoch",
    EPOCH,
    "--mem",
    "1024",
]

DEFAULT_ROUNDS = 5
DEFAULT_WORKERS = 1
# A sample that runs longer than this is stopped and recorded as a failure.
SAMPLE_LIMIT_SECONDS = 1800

# The workloads' parameters, sized for seconds to a few minutes each.
COMPUTE_LOOPS = 3000000
SPAWN_PROCESSES = 2000
CHECK_SCHEDULES = 8
CHECK_JOBS = 4
CHECK_NIX_SCHEDULES = 4
PINGPONG_THREADS = 4
PINGPONG_PASSES = 2000
# How far into the unpacking run `cat` reads, as a share of its steps.
CAT_AT = 0.95
# A keyframe's file name ends in this; the directory also holds a marker.
KEYFRAME_SUFFIX = ".kf"

# GNU time's fields: wall, user and system seconds, peak RSS in KiB.
TIME_FORMAT = "%e %U %S %M"
KIB_PER_MIB = 1024
BYTES_PER_MIB = 1 << 20

# The line rewind ends a run with, which every release prints.
SUMMARY = re.compile(r"run ([0-9a-f]{16}) .*?after (\d+) steps, ([\d.]+)s virtual")

# compare's bar for a difference: the medians must differ by more than
# this share, and the two sides' ranges must not overlap.
COMPARE_MIN_CHANGE = 0.03


class Group(enum.Enum):
    """Which question a workload answers, as the page groups them."""

    SHORT = "short"
    RECORD = "record"
    LOOK = "look"
    EXPLORE = "explore"


class Home(enum.Enum):
    """Whether a workload starts with REWIND_HOME emptied."""

    FRESH = "fresh"
    KEPT = "kept"


class Workload:
    """One command a build is measured on. `argv` takes the context of
    the round so far and returns rewind's arguments; `native` is the same
    work run on the host, when it has one."""

    def __init__(
        self,
        name,
        group,
        title,
        description,
        argv,
        native=None,
        home=Home.KEPT,
        needs=None,
    ):
        self.name = name
        self.group = group
        self.title = title
        self.description = description
        self.argv = argv
        self.native = native
        self.home = home
        self.needs = needs

    def describe(self):
        return {
            "name": self.name,
            "group": self.group.value,
            "title": self.title,
            "description": self.description,
            "native": self.native is not None,
        }


def compute_program():
    return f"BEGIN{{s=0;for(i=0;i<{COMPUTE_LOOPS};i++){{s=(s+i*i)%1000003}};print s}}"


def spawn_script(true):
    return f"i=0; while [ $i -lt {SPAWN_PROCESSES} ]; do {true}; i=$((i+1)); done"


def untar_script(tar, find, wc, src, dest):
    return f"{tar} -xJf {src} -C {dest} && {find} {dest} -type f | {wc} -l"


# Where the guest unpacks the tarball: the root's /tmp, on the VM's
# writable overlay, which is memory.
GUEST_UNTAR_DIR = "/tmp/x"
GUEST_TARBALL = "/data/src.tar.xz"


def run_argv(ctx, *command, seed=0, extra=()):
    return [
        "run",
        *RUN_OPTIONS,
        "--seed",
        str(seed),
        *extra,
        "--root",
        ctx.root,
        "--",
        *command,
    ]


def nix_argv(ctx, seed=0, extra=()):
    # Since 0.5.0 `rewind nix` asks the binary caches about the outputs;
    # the network is not what is measured.
    compare = ["--no-compare"] if ctx.build.has("nix", "--no-compare") else []
    return ["nix", *RUN_OPTIONS, "--seed", str(seed), *compare, *extra, ctx.hello]


WORKLOADS = [
    Workload(
        "echo-cold",
        Group.SHORT,
        "First run",
        "`echo hi` in a REWIND_HOME emptied first: boot and teardown, plus packing the root into an image.",
        lambda ctx: run_argv(ctx, "/bin/echo", "hi"),
        home=Home.FRESH,
    ),
    Workload(
        "echo",
        Group.SHORT,
        "Short run",
        "`echo hi` again with another seed, the image already packed: what any run costs before its work.",
        lambda ctx: run_argv(ctx, "/bin/echo", "hi", seed=1),
    ),
    Workload(
        "compute",
        Group.RECORD,
        "Computation",
        f"busybox awk adding {COMPUTE_LOOPS:,} squares: no system calls, so the cost is the branch counter's clock.",
        lambda ctx: run_argv(ctx, "/bin/awk", compute_program()),
        native=lambda ctx: [ctx.tool("awk"), compute_program()],
    ),
    Workload(
        "spawn",
        Group.RECORD,
        "Processes",
        f"A shell loop starting `true` {SPAWN_PROCESSES:,} times: fork, exec and exit, the shape of a build's many small processes.",
        lambda ctx: run_argv(ctx, "/bin/sh", "-c", spawn_script("/bin/true")),
        native=lambda ctx: [ctx.tool("sh"), "-c", spawn_script(ctx.tool("true"))],
    ),
    Workload(
        "untar",
        Group.RECORD,
        "Memory writes",
        "Unpacking the coreutils source, 63 MiB, into the VM's memory and counting the files: pages written, which keyframes store.",
        lambda ctx: run_argv(
            ctx,
            "/bin/sh",
            "-c",
            f"mkdir {GUEST_UNTAR_DIR} && "
            + untar_script("tar", "find", "wc", GUEST_TARBALL, GUEST_UNTAR_DIR),
        ),
        native=lambda ctx: [
            ctx.tool("sh"),
            "-c",
            untar_script(
                ctx.tool("tar"),
                ctx.tool("find"),
                ctx.tool("wc"),
                ctx.tarball,
                ctx.scratch_dir(),
            ),
        ],
    ),
    Workload(
        "cat",
        Group.LOOK,
        "Late seek",
        f"`rewind cat` of a file at {int(CAT_AT * 100)}% of the unpacking run: restore the last keyframe before it and run forward.",
        lambda ctx: [
            "cat",
            ctx.runs["untar"]["run"],
            str(int(ctx.runs["untar"]["steps"] * CAT_AT)),
            ctx.cat_path,
        ],
        needs="untar",
    ),
    Workload(
        "check",
        Group.EXPLORE,
        "Exploring",
        f"`rewind check` of {PINGPONG_THREADS} threads passing a token {PINGPONG_PASSES:,} times, {CHECK_SCHEDULES} schedules {CHECK_JOBS} at a time.",
        lambda ctx: [
            "check",
            *RUN_OPTIONS,
            "--schedules",
            str(CHECK_SCHEDULES),
            "-j",
            str(CHECK_JOBS),
            "--root",
            ctx.root,
            "--",
            "/bin/pingpong",
            str(PINGPONG_THREADS),
            str(PINGPONG_PASSES),
        ],
    ),
    Workload(
        "hello-cold",
        Group.RECORD,
        "First Nix build",
        "`rewind nix` of GNU hello in a REWIND_HOME emptied first: packing the build's inputs into an image, then the full nixpkgs build.",
        lambda ctx: nix_argv(ctx),
        home=Home.FRESH,
    ),
    Workload(
        "hello",
        Group.RECORD,
        "Nix build",
        "GNU hello's nixpkgs build again with another seed, its inputs already packed: configure, make and make check, with keyframes.",
        lambda ctx: nix_argv(ctx, seed=1),
    ),
    Workload(
        "hello-nokf",
        Group.RECORD,
        "Nix build, no keyframes",
        "The same build with --no-keyframes: what recording costs without the snapshots that seeking needs.",
        lambda ctx: nix_argv(ctx, seed=2, extra=["--no-keyframes"]),
    ),
    Workload(
        "replay",
        Group.LOOK,
        "Replay",
        "`rewind replay` of the Nix build: run its inputs again from boot and check the trace comes out the same.",
        lambda ctx: ["replay", ctx.runs["hello"]["run"]],
        needs="hello",
    ),
    Workload(
        "check-nix",
        Group.EXPLORE,
        "Exploring a Nix build",
        f"`rewind check` of GNU hello's build in a REWIND_HOME emptied first: its unperturbed run, then {CHECK_NIX_SCHEDULES} schedules {CHECK_JOBS} at a time.",
        lambda ctx: [
            "check",
            *RUN_OPTIONS,
            "--schedules",
            str(CHECK_NIX_SCHEDULES),
            "-j",
            str(CHECK_JOBS),
            ctx.hello,
        ],
        home=Home.FRESH,
    ),
]

WORKLOADS_BY_NAME = {w.name: w for w in WORKLOADS}


class Build:
    """A rewind to measure: its launcher and what its --help offers."""

    def __init__(self, label, launcher, env):
        self.label = label
        self.launcher = launcher
        self.env = env
        self.version = subprocess.run(
            [launcher, "--version"], capture_output=True, text=True, env=env
        ).stdout.strip()
        self.helps = {}

    def has(self, command, flag):
        if command not in self.helps:
            self.helps[command] = subprocess.run(
                [self.launcher, command, "--help"],
                capture_output=True,
                text=True,
                env=self.env,
            ).stdout
        return re.search(rf"(^|\s){re.escape(flag)}\b", self.helps[command]) is not None


class Context:
    """What one round of one build knows: the inputs, and the runs the
    workloads before made, by workload."""

    def __init__(self, build, inputs, scratch):
        self.build = build
        self.root = os.path.realpath(os.path.join(inputs, "root"))
        self.tarball = os.path.join(self.root, "data", "src.tar.xz")
        with open(os.path.join(inputs, "hello.drv")) as f:
            self.hello = f.read().strip()
        self.cat_path = os.path.join(GUEST_UNTAR_DIR, first_member(self.tarball))
        self.scratch = scratch
        self.runs = {}

    def tool(self, name):
        return os.path.join(self.root, "bin", name)

    def scratch_dir(self):
        return tempfile.mkdtemp(dir=self.scratch)


def first_member(tarball):
    """The tarball's first file, which every step of the unpacking after
    the first few has written."""
    with lzma.open(tarball) as f, tarfile.open(fileobj=f, mode="r|") as tar:
        for member in tar:
            if member.isfile():
                return member.name
    raise RuntimeError(f"{tarball} holds no file")


class Worker:
    """A NUMA node a task's commands are pinned to, by its CPUs: memory
    follows, since Linux places a page on the node that first touches
    it."""

    def __init__(self, node, cpus):
        self.node = node
        self.cpus = cpus

    def prefix(self):
        if self.node is None:
            return []
        return ["taskset", "-c", self.cpus]


def numa_nodes():
    """Each NUMA node and its CPU list, from sysfs."""
    base = "/sys/devices/system/node"
    nodes = []
    for name in sorted(os.listdir(base)):
        match = re.fullmatch(r"node(\d+)", name)
        if not match:
            continue
        with open(os.path.join(base, name, "cpulist")) as f:
            nodes.append((int(match.group(1)), f.read().strip()))
    return nodes


class Nodes:
    """Hands each task the NUMA node whose CPUs were idlest over the
    last second, among those no other task holds. The machine may be
    shared, and a node another job has moved onto would slow whichever
    build ran there; picking at each task's start follows the idle
    nodes as they move."""

    def __init__(self, count):
        self.nodes = numa_nodes()
        self.held = set()
        self.lock = threading.Lock()
        if len(self.nodes) > 1 and count > len(self.nodes):
            sys.exit(
                f"--workers {count}: this machine has {len(self.nodes)} NUMA nodes"
            )

    def take(self):
        with self.lock:
            # One node, or none: nothing to pick between.
            if len(self.nodes) <= 1:
                return Worker(None, None)

            before = busy_jiffies()
            time.sleep(1)
            after = busy_jiffies()

            def load(cpulist):
                used = total = 0
                for cpu in expand_cpulist(cpulist):
                    used += after[cpu][0] - before[cpu][0]
                    total += after[cpu][1] - before[cpu][1]
                return used / total if total else 0

            free = [node for node in self.nodes if node[0] not in self.held]
            node, cpus = min(free, key=lambda node: load(node[1]))
            self.held.add(node)
            return Worker(node, cpus)

    def give(self, worker):
        with self.lock:
            self.held.discard(worker.node)


def busy_jiffies():
    """Busy and total jiffies per CPU, from /proc/stat."""
    out = {}
    with open("/proc/stat") as f:
        for line in f:
            match = re.match(r"cpu(\d+) (.*)", line)
            if not match:
                continue
            fields = [int(x) for x in match.group(2).split()]
            idle = fields[3] + fields[4]
            out[int(match.group(1))] = (sum(fields) - idle, sum(fields))
    return out


def expand_cpulist(cpulist):
    cpus = []
    for part in cpulist.split(","):
        if "-" in part:
            lo, hi = part.split("-")
            cpus.extend(range(int(lo), int(hi) + 1))
            continue
        cpus.append(int(part))
    return cpus


def measure(argv, worker, env, workdir):
    """Run argv under GNU time on the worker; return the sample and
    rewind's standard error."""
    timing = os.path.join(workdir, "time")
    stdout = os.path.join(workdir, "stdout")
    stderr = os.path.join(workdir, "stderr")
    command = [*worker.prefix(), "time", "-o", timing, "-f", TIME_FORMAT, *argv]
    with open("/proc/loadavg") as f:
        load = float(f.read().split()[0])
    with open(stdout, "w") as out, open(stderr, "w") as err:
        try:
            result = subprocess.run(
                command, stdout=out, stderr=err, env=env, timeout=SAMPLE_LIMIT_SECONDS
            )
            status = result.returncode
        except subprocess.TimeoutExpired:
            status = "timeout"
    with open(stderr, errors="replace") as f:
        err_text = f.read()
    with open(stdout, errors="replace") as f:
        out_text = f.read()
    sample = {"load": load, "node": worker.node}
    if status != 0:
        sample["error"] = f"exit {status}: " + err_text[-600:]
        return sample, err_text, out_text

    # GNU time writes its line last, after any note of a signal.
    with open(timing) as f:
        wall, user, system, rss = f.read().split("\n")[-2].split()
    sample.update(
        wall=float(wall),
        user=float(user),
        sys=float(system),
        rss_mb=round(int(rss) / KIB_PER_MIB, 1),
    )
    return sample, err_text, out_text


def tree_bytes(path):
    total = 0
    for dirpath, _, names in os.walk(path):
        for name in names:
            try:
                total += os.lstat(os.path.join(dirpath, name)).st_size
            except OSError:
                continue
    return total


def count_keyframes(directory):
    """The keyframes in a run's keyframe directory: its .kf files, not
    the marker saying the run took them as it executed."""
    if not os.path.isdir(directory):
        return 0
    return sum(1 for name in os.listdir(directory) if name.endswith(KEYFRAME_SUFFIX))


def mib(n):
    return round(n / BYTES_PER_MIB, 1)


def run_round(build, workloads, inputs, worker, log):
    """Every workload once, for one build, in a REWIND_HOME of its own.
    Returns samples by workload and the home's size at the end."""
    scratch = tempfile.mkdtemp(prefix="rewind-bench-")
    home = os.path.join(scratch, "home")
    env = {**build.env, "REWIND_HOME": home, "REWIND_PROGRESS": "off", "NO_COLOR": "1"}
    ctx = Context(build, inputs, scratch)
    samples = {}
    try:
        for workload in workloads:
            # A look at a run needs the run.
            if workload.needs and workload.needs not in ctx.runs:
                samples[workload.name] = {
                    "error": f"no {workload.needs} run to look at",
                    "node": worker.node,
                }
                continue
            if workload.home is Home.FRESH:
                shutil.rmtree(home, ignore_errors=True)

            argv = [build.launcher, *workload.argv(ctx)]
            workdir = tempfile.mkdtemp(dir=scratch)
            sample, err, out = measure(argv, worker, env, workdir)
            with open(log, "a") as f:
                f.write(
                    f"== {build.label} {workload.name} {sample}\n{out[-1000:]}{err[-2000:]}\n"
                )

            # The run the command made, for the workloads that look at it.
            summary = SUMMARY.search(err)
            if summary and "error" not in sample:
                run, steps, virtual = (
                    summary.group(1),
                    int(summary.group(2)),
                    float(summary.group(3)),
                )
                run_dir = os.path.join(home, "runs", run)
                keyframes = os.path.join(run_dir, "keyframes")
                sample.update(
                    steps=steps,
                    virtual=virtual,
                    run_mb=mib(tree_bytes(run_dir)),
                    keyframes=count_keyframes(keyframes),
                )
                ctx.runs[workload.name] = {"run": run, "steps": steps}
            samples[workload.name] = sample
        disk = {
            part: mib(tree_bytes(os.path.join(home, part)))
            for part in ("store", "images", "runs")
        }
        return samples, disk
    finally:
        shutil.rmtree(scratch, ignore_errors=True)


def run_native(workloads, inputs, worker):
    """The workloads that have a native form, run on the host."""
    scratch = tempfile.mkdtemp(
        prefix="rewind-bench-native-",
        dir="/dev/shm" if os.path.isdir("/dev/shm") else None,
    )
    ctx = Context(None, inputs, scratch)
    samples = {}
    try:
        for workload in workloads:
            if workload.native is None:
                continue
            workdir = tempfile.mkdtemp(dir=scratch)
            sample, _, _ = measure(
                workload.native(ctx), worker, dict(os.environ), workdir
            )
            samples[workload.name] = sample
        return samples
    finally:
        shutil.rmtree(scratch, ignore_errors=True)


def cache_dir():
    base = os.environ.get("XDG_CACHE_HOME") or os.path.expanduser("~/.cache")
    path = os.path.join(base, "rewind-bench")
    os.makedirs(path, exist_ok=True)
    return path


def unpack(tarball, label):
    """A release tarball, unpacked once into the cache, by its label and
    which file it is: a label measured again with a new tarball unpacks
    again."""
    stat = os.stat(tarball)
    which = f"{os.path.realpath(tarball)}:{stat.st_size}:{stat.st_mtime_ns}"
    dest = os.path.join(
        cache_dir(),
        "builds",
        f"{label}-{hashlib.sha256(which.encode()).hexdigest()[:12]}",
    )
    launcher = os.path.join(dest, RELEASE_DIR, LAUNCHER)
    if os.path.exists(launcher):
        return launcher
    tmp = dest + ".tmp"
    shutil.rmtree(tmp, ignore_errors=True)
    os.makedirs(tmp)
    subprocess.run(["tar", "-xzf", tarball, "-C", tmp], check=True)
    shutil.rmtree(dest, ignore_errors=True)
    os.replace(tmp, dest)
    return launcher


def release_env():
    """The environment for a release's launcher, which uses the guest
    shipped next to it only when the environment names none: a dev
    shell's REWIND_KERNEL would otherwise boot every release on one
    kernel."""
    return {k: v for k, v in os.environ.items() if not k.startswith("REWIND_")}


def resolve(label, path):
    """A Build from LABEL=PATH: a release tarball, its unpacked
    directory, or an executable, which keeps the environment's guest."""
    path = os.path.abspath(path)
    if os.path.isdir(path):
        for candidate in (
            os.path.join(path, RELEASE_DIR, LAUNCHER),
            os.path.join(path, LAUNCHER),
        ):
            if os.path.exists(candidate):
                return Build(label, candidate, release_env())
        tarball = os.path.join(path, RELEASE_DIR + ".tar.gz")
        if os.path.exists(tarball):
            return Build(label, unpack(tarball, label), release_env())
        sys.exit(f"{path}: no {LAUNCHER} or {RELEASE_DIR}.tar.gz in it")
    if path.endswith(".tar.gz"):
        return Build(label, unpack(path, label), release_env())
    return Build(label, path, dict(os.environ))


def download_release(tag):
    tarball = os.path.join(cache_dir(), "releases", f"{tag}.tar.gz")
    if not os.path.exists(tarball):
        os.makedirs(os.path.dirname(tarball), exist_ok=True)
        print(f"downloading {tag}", flush=True)
        with urllib.request.urlopen(RELEASE_URL.format(tag=tag)) as response, open(
            tarball + ".tmp", "wb"
        ) as f:
            shutil.copyfileobj(response, f)
        os.replace(tarball + ".tmp", tarball)
    return Build(tag, unpack(tarball, tag), release_env())


def git_info(repo, rev):
    """The commit and its date, when the label names one in the repo."""
    if not repo:
        return {}
    result = subprocess.run(
        ["git", "-C", repo, "log", "-1", "--format=%H%x00%cI%x00%s", rev],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        return {}
    commit, date, subject = result.stdout.strip().split("\0")
    return {"commit": commit, "date": date, "subject": subject}


def machine():
    cpu = None
    with open("/proc/cpuinfo") as f:
        for line in f:
            if line.startswith("model name"):
                cpu = line.split(":", 1)[1].strip()
                break
    with open("/proc/meminfo") as f:
        memory_gib = round(int(f.readline().split()[1]) / (1 << 20))
    return {
        "host": socket.gethostname(),
        "cpu": cpu,
        "cores": os.cpu_count(),
        "memory_gib": memory_gib,
        "kernel": platform.release(),
    }


def load_history(path):
    if not path or not os.path.exists(path):
        return {"records": []}
    with open(path) as f:
        return json.load(f)


def record_order(record):
    return (record.get("date") or "", record["label"])


def save_history(path, history):
    history = {
        "description": DESCRIPTION,
        "workloads": [w.describe() for w in WORKLOADS],
        "options": RUN_OPTIONS,
        "records": sorted(history["records"], key=record_order),
    }
    tmp = path + ".tmp"
    with open(tmp, "w") as f:
        json.dump(history, f, indent=1)
        f.write("\n")
    os.replace(tmp, path)


def cmd_run(args):
    builds = []
    for spec in args.rewind or []:
        label, _, path = spec.partition("=")
        builds.append(resolve(label, path))
    for tag in args.release or []:
        builds.append(download_release(tag))
    if not builds:
        sys.exit("name a build: --release TAG or --rewind LABEL=PATH")
    workloads = (
        [WORKLOADS_BY_NAME[name] for name in args.only.split(",")]
        if args.only
        else WORKLOADS
    )

    history = load_history(args.history)
    log = args.history + ".log"
    host = machine()
    nodes = Nodes(args.workers)
    print(
        f"measuring on {host['host']} ({host['cpu']}), {args.workers} at once",
        flush=True,
    )

    # One record per build, measured afresh: an earlier record of the
    # same label is replaced. With --only, the earlier record is kept and
    # only the workloads named are measured again.
    existing = {r["label"]: r for r in history["records"]}
    partial = args.only is not None
    records = {}
    for build in builds:
        if partial and build.label in existing:
            record = existing[build.label]
            for w in workloads:
                record["workloads"][w.name] = []
                if w.native:
                    record["native"][w.name] = []
            records[build.label] = record
            continue
        info = git_info(args.repo, build.label)
        records[build.label] = {
            "label": build.label,
            "version": build.version,
            **info,
            "measured": datetime.datetime.now(datetime.timezone.utc).isoformat(
                timespec="seconds"
            ),
            "machine": host,
            "inputs": os.path.realpath(args.inputs),
            "rounds": 0,
            "workloads": {w.name: [] for w in workloads},
            "native": {w.name: [] for w in workloads if w.native},
            "disk": [],
        }
    lock = threading.Lock()

    def save():
        kept = [r for r in history["records"] if r["label"] not in records]
        save_history(args.history, {"records": kept + list(records.values())})

    # Each round is every build once, in a shuffled order; workers take
    # the next task as they free up. A task runs the native workloads
    # first, on the same node, so each slowdown compares two
    # measurements taken side by side.
    tasks = []
    shuffle = random.Random(args.seed)
    for round_index in range(args.rounds):
        order = list(builds)
        shuffle.shuffle(order)
        tasks.extend((round_index, build) for build in order)

    def perform(task):
        round_index, build = task
        worker = nodes.take()
        try:
            native = run_native(workloads, args.inputs, worker)
            samples, disk = run_round(build, workloads, args.inputs, worker, log)
        finally:
            nodes.give(worker)
        with lock:
            record = records[build.label]
            for name, sample in samples.items():
                record["workloads"][name].append({**sample, "round": round_index})
            for name, sample in native.items():
                record["native"][name].append({**sample, "round": round_index})
            # A pass over some workloads leaves the home's size and the
            # round count to the full passes.
            if not partial:
                record["disk"].append(disk)
                record["rounds"] += 1
            save()
        print(
            f"round {round_index + 1}: {build.label} on node {worker.node}: "
            + summarize(samples)
            + "; native "
            + summarize(native),
            flush=True,
        )

    with concurrent.futures.ThreadPoolExecutor(max_workers=args.workers) as pool:
        for future in [pool.submit(perform, task) for task in tasks]:
            future.result()


def summarize(samples):
    parts = []
    for name, sample in samples.items():
        if "error" in sample:
            parts.append(f"{name} FAILED")
            continue
        parts.append(f"{name} {sample['wall']:.2f}s")
    return ", ".join(parts)


class Verdict(enum.Enum):
    SAME = "same"
    FASTER = "better"
    SLOWER = "worse"


def judge(base, head):
    """Whether head differs from base by more than both their spreads."""
    if not base or not head:
        return None, None
    mb, mh = statistics.median(base), statistics.median(head)
    if mb == 0:
        return None, Verdict.SAME
    change = (mh - mb) / mb
    if abs(change) < COMPARE_MIN_CHANGE:
        return change, Verdict.SAME
    if change > 0 and min(head) > max(base):
        return change, Verdict.SLOWER
    if change < 0 and max(head) < min(base):
        return change, Verdict.FASTER
    return change, Verdict.SAME


COMPARED = [("wall", "wall s"), ("cpu", "cpu s"), ("rss_mb", "rss MiB")]


def values(record, workload, metric):
    out = []
    for sample in record["workloads"].get(workload, []):
        if "error" in sample:
            continue
        if metric == "cpu":
            out.append(sample["user"] + sample["sys"])
            continue
        if metric in sample:
            out.append(sample[metric])
    return out


def cmd_compare(args):
    history = load_history(args.history)
    by_label = {r["label"]: r for r in history["records"]}
    for label in (args.base, args.head):
        if label not in by_label:
            sys.exit(
                f"{args.history} has no record {label}; it has {', '.join(by_label)}"
            )
    base, head = by_label[args.base], by_label[args.head]
    if base["machine"]["host"] != head["machine"]["host"]:
        sys.exit(
            f"{args.base} was measured on {base['machine']['host']}, {args.head} on {head['machine']['host']}"
        )

    print(f"{'workload':<12}" + "".join(f"{title:>26}" for _, title in COMPARED))
    for workload in WORKLOADS:
        cells = []
        for metric, _ in COMPARED:
            b, h = values(base, workload.name, metric), values(
                head, workload.name, metric
            )
            change, verdict = judge(b, h)
            if change is None:
                cells.append(f"{'-':>26}")
                continue
            mark = {Verdict.SAME: " ", Verdict.FASTER: "-", Verdict.SLOWER: "+"}[
                verdict
            ]
            text = f"{statistics.median(b):.2f} -> {statistics.median(h):.2f} {change:+.0%}{mark}"
            cells.append(f"{text:>26}")
        print(f"{workload.name:<12}" + "".join(cells))
    print(
        "+ worse, - better: the medians differ by more than "
        f"{COMPARE_MIN_CHANGE:.0%} and the two sides' ranges do not overlap"
    )


def cmd_merge(args):
    by_label = {}
    for path in [args.history, *args.files]:
        for record in load_history(path)["records"]:
            by_label[record["label"]] = record
    save_history(args.history, {"records": list(by_label.values())})
    print(f"{args.history}: {len(by_label)} records")


def main():
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument(
        "--inputs", help="the built benchmark inputs, nix build .#bench-inputs"
    )
    sub = parser.add_subparsers(dest="command", required=True)

    run = sub.add_parser("run", help="measure builds into a history file")
    run.add_argument(
        "--rewind",
        action="append",
        metavar="LABEL=PATH",
        help="a build to measure (repeatable)",
    )
    run.add_argument(
        "--release",
        action="append",
        metavar="TAG",
        help="a release to download and measure (repeatable)",
    )
    run.add_argument("--history", default="site/bench/history.json")
    run.add_argument("--rounds", type=int, default=DEFAULT_ROUNDS)
    run.add_argument(
        "--workers",
        type=int,
        default=DEFAULT_WORKERS,
        help="NUMA nodes to measure on at once",
    )
    run.add_argument("--only", help="comma-separated workloads, in the order given")
    run.add_argument(
        "--repo", help="a git checkout to date the labels from, as tags or commits"
    )
    run.add_argument(
        "--seed", type=int, default=0, help="seeds the order builds run in each round"
    )

    compare = sub.add_parser(
        "compare", help="print how one record differs from another"
    )
    compare.add_argument("--history", default="site/bench/history.json")
    compare.add_argument("base")
    compare.add_argument("head")

    merge = sub.add_parser("merge", help="join history files into one")
    merge.add_argument("--history", default="site/bench/history.json")
    merge.add_argument("files", nargs="+")

    args = parser.parse_args()
    if args.command == "run":
        if not args.inputs:
            sys.exit("pass --inputs, or run through `nix run .#bench`")
        cmd_run(args)
    elif args.command == "compare":
        cmd_compare(args)
    else:
        cmd_merge(args)


if __name__ == "__main__":
    main()
