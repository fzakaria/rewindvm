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

| Piece              | Where                            | What it does                                                                                       |
| ------------------ | -------------------------------- | -------------------------------------------------------------------------------------------------- |
| Guest kernel patch | `guest/linux/rewind-guest.patch` | A "Rewind" x86 hypervisor platform in Linux 6.12: virtual clock and timer, idle as an exit, events |
| Guest init         | `crates/rewind-init`             | PID 1: mounts the input image, runs the job, reports its exit status and output hashes             |
| Monitor            | `crates/rewind-vmm`              | One vCPU on KVM: boots the kernel, handles exits, owns time and interrupts, takes keyframes        |
| Trace              | `crates/rewind-trace`            | Decodes guest records into events and answers questions about a run at a step                      |
| Page store         | `crates/rewind-store`            | Content-addressed, compressed 4 KiB pages shared by every keyframe                                 |
| Engine             | `crates/rewind-core`             | Input images, Nix derivations as jobs, runs on disk, keyframes, seeking                            |
| Command            | `crates/rewind`                  | `rewind run`, `nix`, `check`, `fork`, `replay`, `log`, `ps`, `events`, `diff`, `ls`                |
| App                | `crates/rewind-app`              | The GPUI scrubber (proprietary; see Product)                                                       |

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

The kernel is upstream Linux 6.12 with one patch and a small config, built by
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
  reschedule is requested, or both.

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

For mylib, whose shutdown test fails about one run in nine on the host, about
half of the perturbed schedules fail. The first failure comes on the first or
second schedule, and the whole search takes under a minute.

Two earlier designs did not work, and why is worth keeping.

- **Jittering every exit's time.** This found failures, but a single shift
  moved every timer after it. Narrowing then could not localize anything,
  and the reported divergence landed in the compiler.
- **Reschedule requests without a real scheduler clock.** These changed
  nothing. With no TSC, the scheduler's clock was jiffies, which barely moved
  in a short run. Every task seemed to have run for no time, so the scheduler
  never switched. The virtual scheduler clock fixed that.

## Limits

- **Computation is free, and CPU-bound threads are not preempted.** A thread
  that computes without system calls runs until it makes one. A thread
  spinning on a flag without yielding stalls the guest; futex-based waits are
  fine. Races that need preemption in the middle of pure computation are out
  of reach.
- **One vCPU.** Threads interleave but never run at the same instant.
  Throughput comes from running many machines at once, one per core, which
  `check` could do in parallel and does not yet.
- **The CPU vendor is part of the input.** The fixed CPU model keeps the
  host's vendor, since Intel and AMD differ in ways CPUID cannot hide, so a
  run made on AMD replays on any AMD host from Zen 2 on but not on Intel,
  and the other way around.
- **User-space RDRAND and RDSEED** still work if a program executes them
  without checking CPUID. KVM does not let the monitor trap them. Hardly any
  program does this.
- **No network**, other than loopback.
- **x86_64 Linux hosts with KVM only.**

### Work-proportional time: measured, not shipped

The fix for free computation is to let virtual time follow the guest's work,
as Antithesis does with instruction counts. `crates/rewind-vmm/src/pmu.rs`
reads a guest-mode hardware counter of retired conditional branches at every
exit. That is the counter rr uses, and it is readable only at exits, which
are fixed points in the guest's instruction stream.

On this Zen 4 laptop, short runs give identical counts, and retired
instructions drift. Over a full hello build, three runs differ by up to 26
branches out of 9.5 billion. That is the overcount rr documents on AMD, which
it works around by setting a model-specific register bit that needs root.
Reading the counter at every exit also doubled the wall time. So the counter
stays an experiment until it is exact on the hardware people have.

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

## Roadmap

- **gdb at a step.** A gdb stub on the restored machine, positioned in the
  address space of a chosen process, with the job's binaries available for
  symbols.
- **A shell at a step.** A fork whose guest gets an interactive shell next to
  the paused job, with its input recorded so the fork still replays.
- **Exporting runs.** A single file holding the manifest, trace, keyframes
  and the pages they reference, to attach to an issue.
- **Parallel `check`**, one machine per core.
- **Work-proportional time** once a counter is exact, or with rr's AMD
  workaround where the user can apply it.
- **More than one vCPU**, serialized, as the Red Vice design does.

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

Payments go through Stripe. Plain Stripe Checkout leaves sales tax and VAT
with the seller. Stripe Managed Payments, Stripe's merchant of record product,
would take them on. A checkout webhook on a small server issues the key and
emails it. An exe.dev VPS is enough for that server, since it needs no KVM.
Runs happen on the user's machine.
