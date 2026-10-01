# Design

Rewind VM runs a Linux workload in a virtual machine whose every run is a
function of its inputs. The same kernel, initramfs, root filesystem, command,
seed and schedule produce the same sequence of events at the same steps, every
time. Because runs repeat, nothing needs recording. A run is its inputs, a
trace of what happened, and optional keyframes that make any step quick to
reach again.

This document explains how the machine is made deterministic, what a run is
on disk, how interleavings are explored, and where the approach stops. The
numbers were measured on a Ryzen 7 7840U laptop (Zen 4, 16 threads) running
Linux 7.1.

## At a glance

| Piece              | Where                            | What it does                                                                                      |
| ------------------ | -------------------------------- | ------------------------------------------------------------------------------------------------- |
| Guest kernel patch | `guest/linux/rewind-guest.patch` | A "Rewind" x86 hypervisor platform in Linux 7.2: virtual clock and timer, idle as an exit, events |
| Guest init         | `crates/rewind-init`             | PID 1: mounts the input image, runs the job, reports its exit status and output hashes            |
| Monitor            | `crates/rewind-vmm`              | One vCPU on KVM: boots the kernel, handles exits, owns time and interrupts, takes keyframes       |
| Trace              | `crates/rewind-trace`            | Decodes guest records into events and answers questions about a run at a step                     |
| Page store         | `crates/rewind-store`            | Content-addressed, compressed 4 KiB pages shared by every keyframe                                |
| Engine             | `crates/rewind-core`             | Input images, Nix derivations as jobs, runs on disk, keyframes, seeking                           |
| Command            | `crates/rewind`                  | `rewind run`, `nix`, `check`, `fork`, `prune`, `replay`, `cat`, `shell`, `gdb`, `export` and more |
| App                | `crates/rewind-app`              | The GPUI scrubber (proprietary; see Product)                                                      |

## Determinism

KVM runs guest code on the real CPU, which is fast and deterministic on its
own: the same instructions from the same state produce the same state. What
breaks determinism is everything that reaches the guest from outside that
stream of instructions. Each such source is removed or replaced with a value
the monitor controls.

| Source                | What a normal VM does                                               | What Rewind VM does                                                                                                                  |
| --------------------- | ------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------ |
| Interrupts            | A timer or device interrupts the guest whenever the host gets to it | Only one interrupt exists, and the monitor sends it only while the vCPU is stopped at an exit the guest made                         |
| Time                  | TSC, kvmclock, HPET and the PIT read host time                      | The monitor's virtual clock, read through a port. No PIT or HPET, TSC and kvmclock hidden                                            |
| Idle                  | `HLT` waits for the next interrupt                                  | The idle routine writes a port; the monitor jumps virtual time to the armed timer                                                    |
| `RDTSC` in the kernel | Read directly                                                       | `random_get_entropy()` and `get_cycles()` in a Rewind kernel respect the missing CPUID bit                                           |
| `RDTSC` in user space | Read directly                                                       | CR4.TSD is set for every task, and the kernel emulates the instruction with virtual time                                             |
| Randomness            | `RDRAND`, `RDSEED`, interrupt timing                                | Hidden in CPUID. The kernel's RNG is seeded from a 32-byte seed the monitor passes at boot                                           |
| KASLR                 | Picks addresses from `RDRAND` and the TSC                           | Built without it                                                                                                                     |
| Jitter entropy        | Samples the cycle counter a million times at boot                   | Gets a splitmix64 sequence, which is free and the same every run                                                                     |
| CPUID                 | The host's topology and features                                    | A fixed x86-64-v3 CPU: the same features, cache sizes, family and address widths on every host (see [The CPU model](#the-cpu-model)) |
| Scheduler clock       | TSC, or jiffies without one                                         | The virtual clock, from a shared page the monitor refreshes at every exit                                                            |
| Devices               | Timers, disks and NICs complete work on their own schedule          | No asynchronous devices. The input image is memory-mapped, and output is a port write                                                |
| Host scheduling       | Invisible to the guest                                              | Also invisible: a host interrupt or preemption causes a VM exit the guest cannot observe                                             |

Determinism is checked rather than assumed. `rewind replay` runs a run's
inputs again and compares every event and its step. A busybox workload with
background jobs, pipes, `/dev/urandom` and `sleep`, and full nixpkgs builds of
GNU hello (86712 steps) and mylib (5115 steps), all replay identically. The
work on determinism was mostly finding the sources in the table above one at a
time: each one first appeared as two replays of the same run disagreeing.

### Steps and virtual time

A step is one exit the guest makes: a port or MMIO access. Steps count from
boot, and the step number is how everything in a run is addressed.

Virtual time moves in two ways. Every exit adds a fixed quantum of 5
microseconds, about what a system call and a context switch cost on current
hardware. An idle exit jumps time to the armed timer, the way a halted CPU
wakes at its next deadline. Neither depends on how long anything took on the
host, so `sleep 1` in the guest takes one virtual second and almost no real
time.

The cost of this model is that computation between exits is free. A thread
that computes for a millisecond without a system call sees no time pass, and
nothing preempts it. See [Limits](#limits).

### The guest kernel

The kernel is upstream Linux 7.2 with one patch and a small config, built by
`nix/kernel.nix`. The patch adds a hypervisor platform to
`arch/x86/kernel/cpu/rewind.c`, in the pattern of the Jailhouse and ACRN guest
platforms. The platform is detected by the CPUID signature `RewindRewind`.

- It describes the one local APIC itself, with no MP table, ACPI or I/O APIC.
- It registers a clocksource and a one-shot clock event device backed by the
  monitor, and replaces the idle routine.
- It sets the wall clock at boot and turns power off, restart and halt into
  port writes.
- The monitor's interrupt arrives as an MSI on `HYPERVISOR_CALLBACK_VECTOR`.
  A reason word in the shared page says whether the timer is due, a
  reschedule is requested, or an inspection is.

An inspection is how `rewind cat` and `rewind shell` look inside a run at a
step. Rewind forks the run at the step and writes a request, a list of
arguments, into the shared page. In the interrupt, the kernel stops every user
process with SIGSTOP. Linux never stops the global init with a signal, so init
is instead held at its next system call, before it can start or write
anything. The kernel then starts `/init --inspect` with the request as a
usermode helper, and power off waits until the helper exits. The helper enters
the root and working directory of the process named in the request. For `cat`
it writes the file through `/dev/rewind-stdout` between two marks. For `shell`
it opens a pty, starts the shell that process's environment names (for a Nix
build, the builder's bash with stdenv's PATH) with that environment, and
relays the pty to `/dev/rewind-console`. Reads of that device return input
Rewind places in the shared page with an interrupt of its own, and window
size changes arrive in band. A recording never carries a request, so none of
this changes a recorded run. Init chroots only the job's process into an image
root, never itself: Linux starts init sharing its filesystem root with the
kernel's threads, and a chroot there would hide `/init` from the helper.

`rewind shell --with <installable>` brings more Nix packages into the shell.
Every run reserves a second persistent memory region at boot, 64 GiB at
256 GiB physical, backed by empty anonymous memory, so a recording only ever
sees zeros there. In a fork, Rewind builds or fetches the packages on the
host, packs their closure into an erofs image like an input image, and maps
it over the region's host memory; KVM follows host mappings, so the slot
itself does not change. The shell's helper drops the block device's cache,
which the boot's partition scan filled with zeros, mounts the image, and
bind-mounts each store path in it into the view's `/nix/store`, with the
packages' bin directories first on PATH.

While a person is typed into a shell, idle time passes in real time: when the
VM goes idle, Rewind waits for typing until the next timer is due, and jumps to
the timer only if none came. `sleep 2` takes two seconds, and an idle shell
costs no CPU.

`rewind gdb` serves the GDB remote protocol for a forked machine with the
`gdbstub` crate: registers through KVM, and memory through KVM's address
translation, so gdb reads the kernel and the running process's user space as
the VM's own page tables map them. Breakpoints are the CPU's four debug
address registers rather than `int3` in memory, and a single step holds
interrupts off. A debug trap is a VM exit the VM never sees and not a step, so
a debugged fork runs exactly as it would have.

What gdb knows about the code comes from two more inspections, each on a fork
of its own, before the fork gdb debugs is made. The kernel hands every
inspection the thread group id of the task that was running when the request
arrived, in `REWIND_RUNNING`. A `running` inspection answers with that
process's `/proc/<pid>/maps` and the bytes of every ELF file it had mapped
that is not in the input image's store, read through
`/proc/<pid>/map_files`: a store path is in the image unless the overlay's
writable layer has it, and everything else, such as a test program the build
compiled, only the VM has. Rewind writes those files to a directory for the
session and gives gdb every program and library at its load offset, the
mapping of the file's start less the address its first loadable segment was
linked at. For the files only the VM had, gdb lists the source files their
DWARF names, and a `files` inspection reads them in the process's view; the
list goes in as console input, since a request has room for few arguments.

DWARF and sources for everything in the store come by build ID from
nixseparatedebuginfod2, which Rewind starts for the session on a socket it
binds itself and hands over the way systemd's socket activation does. It serves
the local store's `debug` outputs and cache.nixos.org's, so glibc's DWARF and
source arrive as gdb asks for them. The kernel's `debug` output is nixpkgs'
separateDebugInfo layout with an overlay of the files the Rewind patch adds or
changes. Rewind fetches the output itself the first time, because Cachix keeps
no index by build ID, opens its vmlinux in gdb, and puts the overlay on gdb's
source path: nixseparatedebuginfod2 takes an overlay file only in place of one
the source tarball has, and `rewind.c` is new.

The kernel is uniprocessor (`CONFIG_SMP=n`), so spinlocks compile away and
nothing in the kernel waits on another CPU. It has no PCI, ACPI or modules.
Booting it and running `/bin/true` takes 315 exits.

The same patch makes the kernel report what the guest does. It registers
probes on the `sched_process_exec`, `fork`, `exit`, `signal_deliver` and
`sys_enter` tracepoints, and adds a console and three devices:
`/dev/rewind-stdout`, `/dev/rewind-stderr` and `/dev/rewind` for marks. Each
report is a record in a static buffer. The kernel writes the buffer's physical
address to a port, and the monitor reads the record out of guest memory and
stamps it with the step.

| Port    | Direction | Meaning                                        |
| ------- | --------- | ---------------------------------------------- |
| `0x5e0` | out       | A record is at this physical address           |
| `0x5e4` | in        | Write the virtual clock into the shared page   |
| `0x5ec` | out       | Arm the timer, nanoseconds from now; 0 disarms |
| `0x5f0` | out       | Nothing is runnable                            |
| `0x5f4` | out       | Stop: power off, restart or halt               |
| `0x5f8` | out       | The shared page is at this physical address    |

| Record               | Payload                                                         |
| -------------------- | --------------------------------------------------------------- |
| Console              | Kernel log text                                                 |
| Output               | Bytes a process wrote to standard output or error, with its pid |
| Exec                 | Filename and argv                                               |
| Fork                 | Child id, and whether it is a thread                            |
| Exit                 | Exit status and command name                                    |
| Signal               | Signal, `si_code`, and the fault address                        |
| Open, Unlink, Rename | Paths of files opened for writing, removed or renamed           |
| Mark                 | A line written to `/dev/rewind`                                 |

### The CPU model

Software picks code paths by the CPU it sees. glibc chooses its string
functions by the vector extensions and cache sizes CPUID reports, and the
kernel chooses mitigations by the CPU's known bugs. A run made on one CPU
therefore takes different steps on another. By default the guest sees a fixed
CPU instead of the host's:

- x86-64-v3 features (AVX2, BMI, FMA, MOVBE), plus AES, PCLMULQDQ, ERMS,
  FSGSBASE and INVPCID, which Haswell and Zen 2 onward all have;
- no AVX-512, SHA, protection keys or speculation control bits;
- an XSAVE area of exactly x87, SSE and AVX state;
- a fixed family, model and brand string per vendor, fixed cache sizes, and
  39 physical and 48 virtual address bits.

A host that lacks any of it refuses the run and names the missing bits. The
model is part of a run's inputs, and `--cpu host` shows the host's features
instead, for hosts older than the baseline. The guest boots with
`mitigations=off`. Its processes need no protection from each other, and KVM
still guards the host. With the fixed CPU, the mitigations the kernel had
chosen more than doubled a build's time.

### The monitor

`rewind-vmm` is a few hundred lines on top of rust-vmm's `kvm-ioctls`. It
creates a VM with the in-kernel local APIC and no PIT. It maps guest RAM with
dirty logging and the input image as a read-only memory slot. It then boots
the bzImage with the 64-bit boot protocol and runs one vCPU. Its run loop
handles each exit, counts it, moves time, and sends the interrupt if the timer
is due and the APIC will take it. If the guest goes idle with no timer armed,
nothing can ever wake it; the monitor stops the run as stalled instead of
hanging.

## Inputs

A run's inputs are the kernel, the initramfs, a read-only input image, a job,
the memory size, the RNG seed, the boot wall clock, the quantum and the
schedule. The run's id is the BLAKE3 hash of all of them, so the same inputs
land in the same run directory.

The input image is erofs, built by `mkfs.erofs` with everything pinned that
could differ between two builds of the same files: owners, timestamps, the
UUID and the order of entries. It is mapped into guest physical memory at
4 GiB and described to the kernel as legacy persistent memory, so the guest
sees `/dev/pmem0` and mounts it without any disk emulation. The image never
enters a keyframe; it is an input, named by its hash.

The job travels as a second cpio archive appended to the initramfs, which the
kernel unpacks over the first. It holds `/rewind/job.json`: argv, environment,
working directory, user, and how the image becomes the root.

- **A root filesystem** (`rewind run --root`): a directory, an erofs image,
  or a tarball such as `docker export` writes. The guest mounts it under a
  writable tmpfs overlay and chroots into it.
- **A Nix derivation** (`rewind nix`, the Nix angle). `rewind` realises the
  derivation's inputs on the host and packs the input closure and the sandbox
  shell into an image. The guest mounts it at `/nix/store` under a writable
  overlay. The builder runs with the environment nix-daemon's `initEnv` would
  give it, `passAsFile` files included, as uid 1000 in `/build`. After a
  successful build, init hashes each output tree and reports the hash.
  `rewind nix` compares it with the host's copy of the output when there is
  one. GNU hello built in the guest is identical to the host's build.

The guest's wall clock starts at midnight UTC of the day the run was made.
Configure scripts compare the clock with the timestamps in source tarballs, so
1970 does not work, and the value is an input like any other, so a replay uses
it again.

## Runs on disk

```
~/.local/share/rewind/
  runs/<id>/manifest.json     inputs, what they describe, parent, outcome
  runs/<id>/trace.bin         every record, each prefixed with its step
  runs/<id>/keyframes/*.kf    keyframes, by step
  images/*.erofs              input images, by content
  store/                      the page store
```

Besides the inputs, a manifest records `trace_hash`, the BLAKE3 hash of
`trace.bin` once the run has finished, so runs that did the same thing are
found without reading their traces. A fork records `parent`, the run and step
it was forked from, `first_difference`, the step where its trace first differs
from its parent's (absent when the two are identical), and `shared_keyframes`,
described under [Forks](#forks).

`trace.bin` is a sequence of events. Each is its step as eight little-endian
bytes, followed by the record exactly as the guest wrote it. `rewind-trace`
reads it and answers the scrubber's questions: which processes were alive at a
step, what they had printed, which files they had written, which build phase
it was, and where two runs first differ.

## Keyframes

A keyframe captures everything that decides what the machine does next:

- the vCPU's registers, extended state, MSRs, local APIC, pending events,
  debug registers and run state;
- the in-kernel interrupt controllers;
- the monitor's clock and devices;
- guest memory.

Memory is written as page hashes into the page store. Only the pages KVM's
dirty log says the guest wrote since the previous keyframe are included, plus
the shared page, which the monitor writes itself. A chain of keyframes
therefore costs about what the guest wrote between them.

Before capturing, the monitor calls `KVM_RUN` once with `immediate_exit` set.
KVM completes a port instruction only on the next `KVM_RUN`. A keyframe taken
right after an exit without this step captures the vCPU still on the
instruction, and the restored guest executes it a second time.

Runs take a keyframe every quarter second of wall time. Seeking to a step
restores the latest keyframe at or before it and runs forward, so a seek costs
at most about one interval. `rewind replay --from` restores a keyframe, runs
to the end and compares the rest of the trace with the original. Every
keyframe checked this way reproduces the rest of its run exactly.

A delta lists the pages KVM's dirty log names, less the ones whose contents
are what they were at the parent keyframe, often zero. About one entry in ten
of the shared keyframe a fork takes (see [Forks](#forks)) is a page written
back to the same bytes.

| Workload                      | Steps | Wall time | With keyframes | Keyframes | New pages stored |
| ----------------------------- | ----- | --------- | -------------- | --------- | ---------------- |
| GNU hello, full nixpkgs build | 86693 | 11.7 s    | 15.8 s         | 58        | 165 MB           |
| mylib, full nixpkgs build     | 5115  | 0.6 s     | 1.4 s          | 4         | 51 MB            |

For comparison, `nix build --rebuild nixpkgs#hello` takes 14.3 s on the same
laptop.

### The page store

Consecutive keyframes of one run share most of their memory. So do keyframes
of runs with the same inputs, and a fork and its parent. `rewind-store`
therefore stores each 4 KiB page once, named by its BLAKE3 hash and compressed
with zstd at its fastest level. The all-zero page is never stored. Pages go
into append-only pack files with an append-only index. A crash can leave at
most a torn last index entry, which is dropped when the store opens.

[casita](https://casita.rs) was the other candidate. It is a content-addressed
store from Cachix with BLAKE3 and FastCDC chunking. It fits disk images and
build artifacts well, and its sync would suit sharing runs between machines.
It is a poor fit for memory. Its content-defined chunks average 256 KiB, and
the pages a guest dirties between two keyframes are scattered 4 KiB pages, so
dedup at that granularity would be poor. It was also pre-release when this
was written. It is the likely choice for moving runs and images between
machines later.

### Forks

A fork is the same run as its parent up to the step before its fork step, so
it shares the parent's keyframes for those steps instead of taking copies. It
restores the parent's latest keyframe at or before that step, copies the
parent's trace up to the keyframe, and runs on from there. Its manifest's
`shared_keyframes` names the parent and the last shared step. Seeking in the
fork reads the parent's keyframes at or before that step and its own after,
and the parent may share some of its own the same way, back to a run that
shares none. Runs that share keyframes sit side by side in one runs
directory.

Rewind takes one keyframe at the last shared step and puts it in the
directory of the earliest run that has that step in common with the fork,
where every later fork at the same step finds it and restores from it. A
fork's own keyframes then hold only what it wrote after its step. How far two
runs agree comes from their schedules: a seed's choice at a step depends on
the seed and the step alone, so two runs part at the first step only one of
them perturbs, or that both perturb with different seeds.

A fork of a fork carries its parent's perturbations up to its own step (see
[Exploring interleavings](#exploring-interleavings)), so it shares its
parent's keyframes through the step before its own, like any fork. A fork of
the mylib fork made at step 3000, forked again at step 4000 with schedule 3,
first differs from its parent at step 4020. Before forks carried their
parents' perturbations it ran unperturbed from step 3000 and differed at
step 3018.

For the mylib run in the tutorial (6164 steps, keyframes at 256, 512, 1200 and
5934), forks of the passing run measured as follows.

| Fork                                  | Before  | After  |
| ------------------------------------- | ------- | ------ |
| At step 4855, the first at that step  | 862 KB  | 102 KB |
| At step 4855, another schedule        | 1454 KB | 155 KB |
| At step 3000, the first at that step  | 883 KB  | 320 KB |
| Shared keyframe at step 4854 (parent) |         | 524 KB |
| Shared keyframe at step 2999 (parent) |         | 504 KB |

The shared keyframe is a delta from the parent's keyframe at step 1200, and
this build writes most of its memory between the two, so the first fork at a
step costs a little less than it did before. Each further fork at that step costs only
its trace and what it wrote after.

A finished run's keyframes are never replaced, since other runs may read them;
running the same inputs again leaves them as they are. A replayable export
copies every keyframe a run reads into the archive as the run's own and drops
`shared_keyframes`, so it imports and replays where the parent is not. A fork
whose parent is gone cannot reach its shared keyframes, and seeking in it
fails with the id of the run it needs; `rewind replay` from boot still works.

`rewind remove <run>` removes the run and every run that descends from it
through `parent`, with any inputs an import placed for them, the deepest
first, so a removal cut short never leaves a fork whose parent is gone. It
removes nothing while one of those runs has not finished, or while a run
outside them reads keyframes from one of them, and names that run. Such a
reader is rare, since a run reads keyframes only from its parent, but
running a fork's inputs again as a plain run makes one: the run keeps the
fork's keyframes and loses its parent.

`rewind prune <run> --identical` removes forks in a run's family, its forks
and their forks, whose `trace_hash` equals an older member's. The run itself
always stays, and so does any run another run here names as its parent or
reads keyframes from. Pages the removed keyframes named stay in the page
store.

## Exploring interleavings

A deterministic machine runs one interleaving of a multithreaded program, the
same one every time. To find the others, a schedule seed perturbs a run in two
ways. Both are fixed functions of the seed and the step, so a perturbed run
repeats as exactly as any other.

- **Reschedule requests.** At one exit in four, the monitor asks the guest to
  reschedule. The kernel marks the current task, and the scheduler picks
  again on the way back to user space. With nothing else runnable, this
  changes nothing.
- **Timer slack.** A timer armed at a perturbed step fires up to 50
  microseconds late, which Linux's default timer slack for user tasks allows
  on real hardware. Sleepers wake in a different order.

A perturbation applies only inside a window of steps. Before the window, a
perturbed run is the unperturbed one, exit for exit. `rewind fork` builds on
this. A fork of a run at step N with schedule K is the run's inputs with the
perturbation starting at N, so it is its parent up to N by construction.

A fork of a perturbed run, such as a fork of a fork, also keeps the parent's
perturbations, each cut off at N, in the spec's `inherited_schedules`. Every
step is then perturbed by the one window that holds it, if any, the same way
it was in the run it came from. A fork of an unperturbed run inherits
nothing.

`rewind check` does the following:

1. Runs the unperturbed schedule.
2. Runs perturbed schedules, starting the window at the step the job
   started, until one ends differently. It compares exit statuses and output
   hashes.
3. Narrows the window, first its end and then its start, to the smallest
   window that still makes that schedule end differently. A smaller window
   perturbs a subset of the same steps, so the search is well defined.
4. Shows where the failing program's own events first differ between the two
   runs. It compares threads by the order they appear, not by their ids.

For mylib, whose shutdown test fails in about one host build in eight, 9 of 64
perturbed schedules fail with counter time and 34 of 64 with exit time.
Counter time's rate is close to the host's because computation takes time
there, as on real hardware; with exit time all of a burst of work lands at
once, which crowds the threads together and makes the race easier to hit. The
first failure has come within the first two batches of 16 schedules, and the
whole search takes under 20 seconds on 16 cores.

Two earlier designs did not work, and why is worth keeping.

- **Jittering every exit's time.** This found failures, but a single shift
  moved every timer after it. Narrowing then could not localize anything,
  and the reported divergence landed in the compiler.
- **Reschedule requests without a real scheduler clock.** These changed
  nothing. With no TSC, the scheduler's clock was jiffies, which barely moved
  in a short run. Every task seemed to have run for no time, so the scheduler
  never switched. The virtual scheduler clock fixed that.

## Limits

- **CPU-bound threads are not preempted.** A thread gives up the CPU only at
  a step, so one that computes without system calls runs until it makes one.
  A thread spinning on a flag without yielding stalls the VM; futex-based
  waits are fine. Races that need preemption in the middle of pure
  computation are out of reach. With exit time computation also takes no
  time; counter time ([pmu.md](pmu.md)) fixes that, not the preemption.
- **One vCPU.** Threads interleave but never run at the same instant.
  Throughput comes from running many machines at once: `check` runs one per
  core.
- **The CPU vendor is part of the input.** The fixed CPU model keeps the
  host's vendor, since Intel and AMD differ in ways CPUID cannot hide, so a
  run made on AMD replays on any AMD host from Zen 2 on but not on Intel,
  and the other way around.
- **User-space RDRAND and RDSEED** still work if a program executes them
  without checking CPUID. KVM does not let the monitor trap them. Hardly any
  program does this.
- **No network**, other than loopback.
- **x86_64 Linux hosts with KVM only.**

### Counter time

With exit time, computation between exits takes no virtual time. Counter time
adds the guest's work: a guest-mode hardware counter of retired conditional
branches, the counter rr uses, read at every exit and worth a nanosecond a
branch. The count is read only at exits, which are fixed points in the
guest's instruction stream, so it is the same on every run when the counter is
exact. It is on Intel. On AMD Zen it overcounts around locked instructions
unless the workaround rr documents is set, a bit in a model-specific register
that needs root; `rewind pmu enable` sets it and leaves a note in /run for
runs as the user. A self-test that provokes the overcount can pass by chance
without the workaround, so on AMD counter time also needs the workaround to be
known to be set. [pmu.md](pmu.md) is the user's side of this.

## Why not QEMU, and why not bhyve

The first prototype used QEMU's record and replay, with a small TCG plugin
that stamped every console byte with its instruction count. It worked: replay
was byte-identical, `replay-seek` landed exactly on the stamped counts, and a
new recording could fork from a snapshot. It also showed the costs.

- TCG with icount ran about 350 million guest instructions a second, 10 to
  15 times slower than hardware.
- Seeking after a snapshot hit two QEMU bugs. One is a deadlock: `loadvm` in
  replay mode runs `vapic_post_load`, which waits on the vCPU thread while it
  holds the replay mutex. The other is a livelock when a snapshot was taken
  just after the guest had been idle.

Antithesis built their deterministic hypervisor on bhyve because a small
hypervisor they own end to end let them inject interrupts at exact
instruction counts. Rewind VM gets the same property without owning the
hypervisor. The guest kernel cooperates, so interrupts only ever arrive at
exits the guest made, and stock KVM does the rest. The same idea underlies
the cooperative deterministic hypervisor described at
[redvice.org](https://redvice.org/2026/deterministic-hypervisor/).

## Product

The engine is open source under the MIT license: the kernel patch, the init,
the monitor, the trace and store crates, the engine and the command. The
kernel patch is GPL-2.0, as Linux is.

The desktop app is proprietary to Lunch Time Surf LLC and sold like Sublime
Text:

- Evaluation has no time limit and never loses features.
- While unregistered, a reminder appears after 20 engine actions or 45
  minutes of use.
- A personal license is $49 and includes three years of updates. A
  commercial license is $99 per seat.

A license key is a signed text block. The app checks it offline against an
Ed25519 public key built into it, and nothing is sent anywhere.
`crates/rewind-app/LICENSING.md` covers the key format, keeping the signing
key offline, and issuing keys.
