# Time inside the VM

Rewind replays a run exactly, so the programs inside the VM cannot read your
machine's real clock: two runs would see different times and could go
different ways. Rewind keeps a clock of its own for the VM instead, in one of
two ways. This page explains both, and the one setting AMD machines need. The
site publishes it at <https://rewindvm.dev/counter-time.html>, the link
`rewind` prints.

## The short version

- **Intel:** nothing to do. Rewind uses counter time.
- **AMD Ryzen or EPYC:** run `sudo rewind pmu enable` once after each boot,
  or turn on the NixOS setting below. Until then Rewind records with exit
  time and prints:

  ```console
  rewind: recording with exit time: this AMD CPU's branch counter is not exact until rr's workaround is set. ...
  ```

  Runs still work and still replay exactly. What changes is how the VM's
  clock moves.

- `rewind pmu status` says which clock your runs will use.

## What the two clocks do

Rewind sees the VM only at the points where the VM stops and hands control to
Rewind: a system call that reports a file or a process, a read of the clock,
going idle. Each of those points is a step. Steps happen at the same places
in every run, so a clock that moves only at steps is the same in every run.

**Exit time** moves the clock 5 microseconds per step. When nothing is
runnable, it jumps straight to the next timer, so `sleep 10` costs nothing.
Computation between steps takes no time at all: a program that computes for
a second of real time sees a few microseconds pass.

**Counter time** also adds the work done since the last step. Your CPU's
performance counter counts the conditional branches the VM's programs run,
and each one adds a nanosecond. Rewind reads the count only at steps, so it
is the same in every run, as long as the counter counts exactly.

Building GNU hello in the VM shows the difference:

| Clock        | Time inside the VM | Time on your machine |
| ------------ | ------------------ | -------------------- |
| Exit time    | 2.1 s              | 15.7 s               |
| Counter time | 11.7 s             | 14.2 s               |

With exit time, timing in code that computes is off. A check that some
computation finishes within a second always passes, and a timeout that
should fire during a long computation never does. Counter time gives such
code a clock close to the real one.

With either clock, a thread gives up the CPU only at a step. A loop that
spins waiting for another thread, without making a system call, keeps the
CPU and the run hangs. Locks, sleeps and I/O all make system calls, so they
are fine.

## Why AMD needs a setting

Counter time needs the branch count to be the same on every run. On AMD Zen
CPUs it is not, out of the box: a speculation feature around locked
instructions makes the counter count a few extra branches, a few in tens of
millions. A second of work is billions of branches, so two runs disagree
almost at once and a recording would not replay. rr, the record and replay
debugger, has the same problem and documents the same fix: set bit 54 of the
model-specific register `0xc0011020` on every CPU, which turns that
speculation off.

`sudo rewind pmu enable` makes that change:

```console
$ sudo rewind pmu enable
set the branch counter workaround on 16 CPUs, until reboot
```

It needs root, because it writes the register through `/dev/cpu/*/msr`, and
it lasts until you reboot. It changes how the CPU speculates for every
program on the machine; Rewind has not measured what that costs them. It
also leaves a note in `/run/rewind/amd-branch-workaround`. Reading the
register back needs root too, and the note is how Rewind, running as you,
knows the setting is on.

## Checking

```console
$ rewind pmu status
cpu: AMD Zen (family 25)
amd workaround (MSR 0xc0011020 bit 54): set by `rewind pmu enable` this boot
perf_event_paranoid: 2
self-test: 40045844 and 40045844 branches, exact at every exit
runs will use counter time
```

The self-test runs a small program twice in the VM, threads adding to one
shared number, which is the pattern the miscount comes from, and compares the
branch count at every step. Rewind uses counter time when the self-test
passes and, on AMD, the setting is on. On AMD the self-test alone is not
enough: without the setting, its two runs sometimes agree by chance. Rewind
remembers the verdict until the next boot, and `rewind pmu status` runs the
test again.

## Every boot on NixOS

Use the `programs.rewind` module from the release tarball's flake:

```nix
inputs.rewind.url = "https://rewindvm.dev/download/rewind-0.1.0-x86_64-linux.tar.gz";

# in your configuration, with inputs.rewind.nixosModules.default imported
programs.rewind.enable = true;
programs.rewind.amdBranchCounterWorkaround = true;
```

The module loads the `msr` kernel module and runs `rewind pmu enable` at
boot.

## Choosing yourself

`--clock exits` or `--clock branches` on `rewind run`, `nix` or `check`
overrides Rewind's choice. A run remembers its clock and replays with it. A
run recorded with counter time replays exactly only on a machine whose
counter is exact too; one recorded with exit time does not depend on the
counter.
