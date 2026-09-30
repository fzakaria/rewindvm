# Counter time

Rewind VM can let the VM's clock follow the work done inside the VM, counted
by your CPU's performance counters. It does so only when the counter is exact
on your machine. When it is not, `rewind` records with exit time instead and
says so:

```console
rewind: recording with exit time: this AMD CPU's branch counter is not exact until rr's workaround is set. ...
```

This page explains what the difference is, and how to turn counter time on.
The site publishes it at <https://rewindvm.dev/counter-time.html>, the link
`rewind` prints.

## Exit time and counter time

A run is deterministic because the VM only ever hears from the outside world
at exits: points where the VM itself hands control to Rewind, such as a
system call that reports an event, a clock read, or going idle. Virtual time
moves at those exits and nowhere else.

With **exit time**, each exit adds 5 microseconds, and idling jumps to the
next timer. Computation between exits takes no time at all. Two things
follow:

- The VM's clock does not reflect computation. A thread that computes for a
  second of real time sees microseconds pass, so timeouts and anything timed
  behave unrealistically in CPU-bound code.
- A thread that computes without system calls is never preempted. Races
  that need a switch in the middle of pure computation are out of reach, and
  a thread spinning on a flag another thread should set stalls the VM.

With **counter time**, every exit also adds the work done since the previous
one. The host's performance counter counts the conditional branches the VM
retires, and each branch adds about a nanosecond. The count is read only at
exits, which are fixed points in the VM's instruction stream, so it is the
same on every run, provided the counter is exact. This is the counter rr
uses.

## Is my counter exact?

```console
$ rewind pmu status
cpu: Amd { family: 25 }
amd workaround (MSR 0xc0011020 bit 54): not known to be set this boot
perf_event_paranoid: 2
self-test: 40045844 and 40045844 branches, exact at every exit
the self-test agreed this time, but without the workaround this CPU's counter is not reliably exact; set it with `sudo rewind pmu enable`
runs will use exit time; see https://rewindvm.dev/counter-time.html
```

`rewind pmu status` runs a small workload twice in the VM: threads adding to
one atomic variable, the pattern that trips the known miscount. It compares
the count at every exit. The result is cached for the current boot, and
`rewind run`, `nix` and `check` use it when `--clock auto`, the default,
decides.

- **Intel:** the counter is exact on the cores rr supports.
- **AMD Zen (Ryzen, EPYC):** the counter miscounts by a few branches in tens
  of millions, around lock-prefixed instructions, because of a speculation
  feature. Tens of millions of branches pass in a fraction of a second, so
  two runs of the same inputs disagree almost at once. rr documents the same
  problem and its fix. The self-test cannot catch this every time: its two
  runs sometimes agree by chance, as in the output above. So on AMD, runs
  use counter time only when the workaround is known to be set, and the
  self-test passes too.

## Turning it on for AMD

```console
$ sudo rewind pmu enable
set the branch counter workaround on 16 CPUs, until reboot
$ rewind pmu status
```

This sets bit 54 of the `LS_CFG` model-specific register (`0xc0011020`) on
every CPU. That disables the speculation the counter miscounts. It is the
same change rr's `zen_workaround.py` makes, it needs root because it writes
MSRs through `/dev/cpu/*/msr`, and it lasts until you reboot. Reading the
MSRs needs root too, so the command also leaves a note for this boot in
`/run/rewind/amd-branch-workaround`, which is how runs as your own user know
the workaround is set. It changes how
the CPU speculates around locked instructions for everything on the machine;
Rewind VM has not measured what that costs other programs.

To set it at every boot on NixOS, use the `programs.rewind` module from the
release tarball's flake:

```nix
inputs.rewind.url = "https://rewindvm.dev/download/rewind-0.1.0-x86_64-linux.tar.gz";

# in your configuration, with inputs.rewind.nixosModules.default imported
programs.rewind.enable = true;
programs.rewind.amdBranchCounterWorkaround = true;
```

The module loads the `msr` kernel module and runs `rewind pmu enable` once
at boot.

## Choosing explicitly

`--clock exits` and `--clock branches` override the self-test. A run records
which clock it used, and a replay uses the same one. A run made with counter
time replays exactly only where the counter is exact too.
