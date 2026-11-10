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
| Performance counters  | `perf_event_open` and `RDPMC` count the host's cycles               | Turned off with `KVM_CAP_PMU_CAPABILITY` before the vCPU exists, whatever CPUID says. The guest's perf has software events only      |
| Jitter entropy        | Samples the cycle counter a million times at boot                   | Gets a splitmix64 sequence, which is free and the same every run                                                                     |
| CPUID                 | The host's topology and features                                    | A fixed x86-64-v3 CPU: the same features, cache sizes, family and address widths on every host (see [The CPU model](#the-cpu-model)) |
| Scheduler clock       | TSC, or jiffies without one                                         | The virtual clock, from a shared page the monitor refreshes at every exit                                                            |
| Devices               | Timers, disks and NICs complete work on their own schedule          | No asynchronous devices. The input image is memory-mapped, and output is a port write                                                |
| Host scheduling       | Invisible to the guest                                              | Also invisible: a host interrupt or preemption causes a VM exit the guest cannot observe                                             |

Determinism is checked rather than assumed. `rewind replay` runs a run's
inputs again and compares every event and its step, and at each of the run's
keyframes the whole machine (see [Checking the whole
machine](#checking-the-whole-machine)). A busybox workload with background
jobs, pipes, `/dev/urandom` and `sleep`, and full nixpkgs builds of GNU hello
(141,035 steps) and mylib (5115 steps), all replay identically, machine
included. The work on determinism was mostly finding the sources in the table
above one at a time: each one first appeared as two replays of the same run
disagreeing. The performance counters were found that way on 2026-10-24, from
a line in every run's boot log: a performance test that read cycles through
`perf_event_open` read the host's, and `rewind check` blamed a schedule for
what the host decided.

`checks.determinism-sources` records and replays a probe for each source a
guest might read past the monitor: the counters, XINUSE through `xgetbv(1)`,
results x86 leaves to the CPU (the flags `MUL` leaves undefined, `BSF` and
`BSR` of zero, `RCPPS` and `RSQRTPS`), and `/proc/cpuinfo`. Those probes and a
GNU hello build, recorded with `--cpu v3-compat` on a Ryzen 7 7840U (Zen 4,
Linux 7.1) and replayed on an EPYC 7763 (Zen 3, Linux 6.18, other microcode),
replay with the machine identical at every keyframe. Intel hosts, APICv or
AVIC, and hybrid Intel parts have not been tried this way.

### Steps and virtual time

A step is one exit the guest makes: a port or MMIO access. Steps count from
boot, and the step number is how everything in a run is addressed. The
guest exits for its events, for a clock read, when it goes idle, when it
arms its timer, and at every context switch (see [Exploring
interleavings](#exploring-interleavings)).

Virtual time moves in two ways. Every exit adds a fixed quantum of 5
microseconds, about what a system call and a context switch cost on current
hardware. An idle exit jumps time to the armed timer, the way a halted CPU
wakes at its next deadline. Neither depends on how long anything took on the
host, so `sleep 1` in the guest takes one virtual second and almost no real
time.

The cost of this model is that computation between exits is free. A thread
that computes for a millisecond without a system call sees no time pass, and
nothing preempts it. See [Limits](#limits).

### Checking the whole machine

An event can only differ at an exit, so a replay that makes the recording's
events can still be another machine: a value in memory or a register that
nothing has printed yet, or ever will. A program that draws a number with
user-space `RDRAND` and keeps it in a global replays with every event the
same, and `rewind gdb` before the next keyframe shows a different number each
time, since the look replays the draw.

So at each step the run has a keyframe at, `rewind replay` captures its
machine the way recording does, hashing the pages written since the last
capture without storing them, and compares the page hashes, registers, MSRs,
local APIC, pending events, interrupt controllers and the monitor's devices
with the keyframe's, by name. The first keyframe that differs and the last
that matched bracket where the machine changed. Each differing page is named
where the replayed machine maps it: a process and an address, with the
variable or function gdb finds there, or the kernel's image and its symbol,
with the bytes on each side. The run is marked, and `rewind gdb`, `shell`,
`cat` and `where` at a step in the bracket warn that they replay the stretch
where it changed.

A few pieces of state change for reasons of the host's that the guest cannot
see, and are left out, each with its reason in `crates/rewind-vmm/src/state.rs`:
the TSC MSR, which CPUID and CR4.TSD keep from the guest; three Hyper-V MSRs of
host time and the host's TSC frequency, which a guest shown no Hyper-V leaves
never reads; the local APIC timer's current count, which KVM computes from host
time and the guest never arms; and the performance counters' MSRs, which the
VM has none of, and which KVM lists by the host's CPU. KVM leaves the vector
and error code of the last exception it delivered in the pending event fields,
so those are compared only while an event is injected or pending. Each entry
was found as a replay whose machine differed while its events did not.

`--machine every` is the default; `last` compares only at the last keyframe
and takes no capture before it, which shows that taking keyframes changes
nothing the guest sees; `none` compares events and records only. On a GNU
hello build of 140,525 steps and 82 keyframes, a replay took 17.1 s comparing
events only, 19.4 s comparing at every keyframe and 17.6 s at the last.
`checks.state-replay` replays a busybox run from boot, from each keyframe, at
the last keyframe alone and as a fork, and has a keyframe name other contents
for a page, which the replay must find, or a comparison of nothing would pass.

### Finding the instruction

`rewind replay --bisect` narrows a difference from the bracket to the
instruction that made it. The recording holds the machine only at its
keyframes, so from the last keyframe that matched, two fresh replays run side
by side and compare with each other, captured every 256 steps and then step by
step through the stride where they part. From the step before the first that
differs, both are single-stepped and their registers compared after each
instruction. KVM single-steps by setting TF, and an `iretq`, `sysretq` or
`popf` loads flags saved before stepping began, which would let the vCPU run
unstepped from the kernel's return to user space to the next exit; both
copies are given TF in the flags such an instruction loads.

The first instruction whose result differs is named with its process and its
function and line, or its kernel symbol. When it is one of the instructions
that read the host, `RDRAND`, `RDSEED`, `RDTSC`, `RDTSCP`, `RDPMC`, `RDMSR`,
`CPUID`, `XGETBV` or `RDPID`, the report says why its result can differ and
who can act: the program, for `RDRAND` in its own code, which `getrandom()`
replaces with numbers the run's seed decides, or Rewind, for anything in the
guest kernel or that Rewind should have kept from the guest. A step whose
instructions all agree and whose machines differ after its exit names what
the monitor or KVM wrote there. On the `RDRAND` probe the search takes about
5 s and finds the instruction 4,072 instructions into step 302.

The search needs the source to give two replays on this host different
answers. A run recorded on another host whose replays all agree here gets
the bracket and the pages, not the instruction. A source that differs only
sometimes takes `--tries N`. Single-stepping costs about 6 µs an instruction,
so a step past five million instructions stops the search and says so.

`rewind bugreport` gathers what a report of such a gap needs: this build, the
host's CPU, microcode, kernel and the KVM parameters that change what a VM
sees, and what `rewind doctor` and `rewind replay` find, with rewind's home and
the user's home directory left out of every path, into a directory beside the
run's trace-only `.rwd`, and prints a link to a new GitHub issue filled in with
it. The trace-only export is a few kilobytes and carries the run's command,
environment and output; the replayable one, with `--replayable`, carries the
guest's kernel and memory and is tens of megabytes, past the 25 MB an issue
takes, for a maintainer who asks. A link cannot attach a file and rewind
uploads nothing, so the person attaches what they choose to share. A replay
that differs under the build that recorded the run finds the run not
deterministic on this host and points at `--bisect` and `rewind bugreport`.
Under another build, which the manifest's `recorded_by` tells, either the run
or the builds may be the cause, and `--bisect` cannot tell which, since its
two replays are both by this build; rewind names both builds and points at the
one that recorded the run. A `rewind check` that finds a run ending
differently from schedule 0 replays both before it says what decides it, and
stops when either does not replay exactly, since then no schedule does.

### The guest kernel

The kernel is upstream Linux 7.2 with two patches and a small config, built
by `nix/kernel.nix`. The second patch changes the order directories list in
when the command line asks for it (see [Directory order](#directory-order)).
The first adds a hypervisor platform to
`arch/x86/kernel/cpu/rewind.c`, in the pattern of the Jailhouse and ACRN guest
platforms. The platform is detected by the CPUID signature `RewindRewind`.

- It describes the one local APIC itself, with no MP table, ACPI or I/O APIC.
- It registers a clocksource and a one-shot clock event device backed by the
  monitor, and replaces the idle routine.
- It sets the wall clock at boot and turns power off, restart and halt into
  port writes.
- The monitor's interrupt arrives as an MSI on `HYPERVISOR_CALLBACK_VECTOR`.
  A reason word in the shared page says whether the timer is due, a
  reschedule, a stall or an inspection is requested, or the RNG is to take
  in new bytes the shared page holds.
- Besides exec, fork, exit, signals and opens for writing, it reports the
  close that drops a written file's last reference, and an open for reading
  of a regular file whose inode counts a writer. Opens of files nobody is
  writing, nearly all of them, cost a descriptor lookup and no record.
- It keeps read and write records: one for each write to a regular file,
  with where in the file the bytes went, and one for each open of a regular
  file, for reading or for writing. These cost no exit. See
  [Read and write records](#read-and-write-records).
- `sched_getaffinity` reports the CPU count given as `rewind.cpus=` on the
  command line, and `sched_setaffinity` takes any of those CPUs to mean the
  one there is (see [Exploring interleavings](#exploring-interleavings)).
- `sched_yield` makes an exit, so a thread that yields in a loop moves
  virtual time (see [Limits](#limits)). These three hooks in
  `kernel/sched/syscalls.c`, and `rewind_rng_reseed` in
  `drivers/char/random.c`, which mixes bytes into the input pool and rekeys
  the crng at once, are the patch's only changes outside `arch/x86`.

An inspection is how `rewind cat`, `rewind files` and `rewind shell` look
inside a run at a step. Rewind forks the run at the step and writes a request, a list of
arguments, into the shared page. In the interrupt, the kernel stops every user
process with SIGSTOP. Linux never stops the global init with a signal, so init
is instead held at its next system call, before it can start or write
anything. The kernel then starts `/init --inspect` with the request as a
usermode helper, and power off waits until the helper exits. The helper enters
the root and working directory of the process named in the request. For `cat`
it writes the file through `/dev/rewind-stdout` between two marks. For `files`
it walks the directory, nearest entries first and at most 10,000 of them, and
writes a line of JSON for each; it lists a directory of `/proc` or `/sys`
below the one asked for without reading it. For `cat --archive` it walks the
path whole, each directory's entries by name and all of one entry's before the
next, staying out of `/proc` and `/sys` as a listing does, and writes each
entry with its permissions, owner and time, then a file's bytes; `rewind`
writes the tar from those as they arrive, so the archive holds
nothing of the machine that asked. For `shell`
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
address registers first, which change nothing in the VM. A debug trap is a VM
exit the VM never sees and not a step, so a debugged fork runs exactly as it
would have. Breakpoints past the registers, and those a watchpoint takes the
registers from, are `int3` written into the VM's memory, and KVM hands every
`#BP` to Rewind rather than the guest while one is there. An `int3` is in a
physical page, which other processes may map too, as they do a shared library's
code: a `#BP` in another process's address space is passed by putting the byte
back for that one instruction, stepped with interrupts held, and writing `int3`
again. An `int3` of the guest's own goes back to the guest. gdb and Rewind's
own reads of memory see the byte `int3` replaced. A process that reads the
file a page of code belongs to sees the `int3` in it, though, and the fork then
goes its own way, which Rewind reports as it reports any. Rewind's own single
steps toward a preemption point, for counter time, keep every one of the
debugger's traps.

gdb's first thread is the CPU, named for the task on it, with the same id at
every stop. Every thread of the process gdb debugs, the one running at the step
or the one `--pid` names, is a thread too, by its own id. A breakpoint or
watchpoint hit in user space stops the process's thread that hit it, one hit
in the kernel stops the CPU's thread, and a step stops the thread gdb
stepped. At setup the kernel writes into the shared page where its
tasks are: `init_task`, the variable holding the running task, the direct
map's base, and the offsets in `task_struct`, `signal_struct` and `mm_struct`
needed to walk from the list of processes to a process's threads and its page
table. At each stop Rewind walks that list in the VM's memory. A thread off the
CPU, or in the kernel on it, saved its user registers in the `struct pt_regs`
at the top of its kernel stack when it entered the kernel, and gdb gets those
for it, read-only; one running user space on the CPU has the vCPU's.
gdb reads its memory through the process's page table, walked in software,
since the CPU may be in another process or idle. So at a deadlock, with every
thread asleep and the CPU idle, `thread apply all bt` shows where each one
waits. The threads move only when the CPU runs them: a step steps the CPU,
whichever thread is on it.

gdb finds a thread's thread-local variables, errno among them, through its FS
base, so the stub describes its registers to gdb with gdb's own x86-64 Linux
feature set, `orig_rax` and the FS and GS bases included. A thread on the CPU
has its FS base in the vCPU, since the kernel leaves it as it is on the way in;
one off the CPU saved it in its `task_struct`, at an offset the kernel also
writes into the shared page, after the interface number so that never moves.

gdb can run the fork backwards. A run is deterministic, so going back is going
forward again from earlier and stopping sooner. Rewind keeps where the fork is
as a position: the start of a step, then what the fork did since, as single
steps, runs to the next hit of a trap in the debugged process, and runs to a
branch count. Replaying those from the step's start reaches the same
instruction exactly, with no guessing from registers. `reverse-stepi` goes to
the instruction boundary before the position: replayed from where the
position's last move began, run to one branch short of the position under
counter time, with the counter's interrupt armed a margin short of that and
the rest single-stepped, then stepped until the step that reaches it. Under
exit time, which counts no branches, the whole way is single-stepped, which is
slow in a long step. `reverse-continue` replays the run with gdb's traps set
from the latest keyframe or checkpoint at or before the position, then from
the one before that, newest first, and stops at the last hit. Past the first
window, the windows from keyframe to keyframe are replayed several at once, on
machines of their own forked from the run, and the newest window's last hit is
the one taken, as one window at a time would find it. It stops: at a
breakpoint as it was reached, and before the instruction that wrote a watched
value, so gdb sees the value change. A hit counts when the CPU had the
process's page table, as in `rewind blame`. That includes the kernel writing
into the process's memory, as read() does filling a buffer, where the
instruction before the write is the kernel's, and other processes may have run
since the call. Such a write in a system call of one of the process's threads
stops at the thread's syscall instruction instead, before it runs: gdb sees the
thread's own frames and the value as it was before the call, and the kernel's
writes in the call, byte by byte as it copies, are one stop. The thread saved
the address after the instruction on its way into the kernel, and a run back to
the last time that thread reached the instruction finds the call. A write by
the kernel outside a system call, or by a thread that never made the call, as
the id the kernel writes for a new thread before its first return, stops at the
instruction before it, as any other write does. Both go back no further than
the process's start or last exec as of the fork's step, where gdb is told it
reached the end of the recorded history. Before then the memory was another
program's, and the exec itself writes the new program's data and zeroes its
bss with the new page table already loaded, so a watch on a global would
otherwise stop in the exec. gdb builds `reverse-next`, `reverse-finish` and the
rest on those two.

Forwards, a session goes no further than the debugged process's next exec.
Past it is another program, whose files and symbols the session never
extracted, in a page table the session does not watch, so breakpoints there
would never fire and memory reads would fail. The kernel makes a record for an
exec once the new program's memory and page table are in place, and the fork
looks through its own records as it runs, so a fork that left its recording
sees the exec too. Breakpoints and watches in the old program still stop the
fork on the way. Between the kernel's switch to the new page table and the
record, a kernel breakpoint or a single step would stop the fork in the new
program's memory, so the fork passes them and runs on to the record. At the exec's record, the fork goes back by replaying the run
to the latest step start at which the process still had its old page table,
which is the start of the step that runs the exec, or to where the run forwards
began when that is later. A Ctrl-C on the way back waits for that stop, which
answers it: stopped short, the fork could be past the switch to the new page
table, or before where the run forwards began. Rewind prints which program the process runs and at
which step, and that `rewind gdb RUN STEP` at that step debugs the new program,
and gdb sees a SIGTRAP. A run forwards from there is refused with the same
words. A step gets the end of the replay log instead, at the stop and at a
refusal: gdb takes a SIGTRAP in the thread it steps for a step done, so `stepi
100` would be refused a hundred times, and the log's end ends the command. Going back works as before, since everything before the stop is the old
program's. A fork that cannot go back, because gdb changed it or it left its
recording, stops at the exec's record, after the exec, and says why.

Run forwards, `continue` to the kernel's write in a system call of one of the
process's threads stops once, at the first instruction that thread runs in user
space after the write: the one after its syscall instruction when the call
returns, or a signal handler's first when the kernel delivers a signal on the
way out. gdb sees the value as the call left it, as with rr. No address is
matched, since a handler can make a call from the same instruction, and a
handler that never returns, by `_exit` or `siglongjmp`, still starts in user
space. From the write on, the fork runs with gdb's traps set and two watches on
the thread: a read-write watch on the pc it saved, which the kernel reads to
return it to user space, by `sysretq` or `iretq`, and a write watch on its
task's pointer to its memory, which the kernel clears as the thread exits.
Neither is hit by the copy, so a 1 MiB `read()` runs at full speed. At the read
on the thread's way out, the fork single-steps the rest of the way, about 50
instructions after a system call and 330 after the kernel sets up a signal's
frame, and the trap the VMM sets at `sysretq` ends the step that returns to user
space; after `iretq` the read's trap is already in user space. Each hit and
single step is a move of the fork's position, so a replay comes to the same
stop. A thread killed in the call, which never returns, has its write reported
as it exits, while its memory is still there to read.

gdb reads a watch's value only at a stop it is told is that watch's, so every
event past a kernel write is reported after the write, and an event that is
several stops is reported one stop at a time. A breakpoint, a Ctrl-C, the end
of the run, or another thread's watched write on the way stops with the
waiting write reported first, in its thread, which gdb shows at the instruction
after its syscall instruction. The rest are queued at the fork's position: the
next `continue`, `stepi` or reverse command from there reports the next one
without running, and a fork that moved drops them. A call that writes two
watches is two stops, one for each watch. A run backwards to a watched write
queues a stop for each other watch whose value it changed, such as both of a
call's. A kernel write in a second thread's call while one waits, a write
outside a system call or in an exit or an exec, or one with gdb's watches and
hardware breakpoints holding more than two debug registers, stops at the write,
in the CPU's thread.

A step's start comes from a checkpoint the fork keeps in memory: the vCPU,
the interrupt controllers and the monitor's state, and the pages written
since the checkpoint before it. Rewinding to one writes back only the pages
that can differ, which the dirty log and the checkpoints name, and the shared
page, which the monitor writes itself, so it takes milliseconds where a fork
from keyframes takes a good part of a second. A step before any checkpoint
comes from a fork of the run at its keyframes.

Only user space counts branches, and a step's start is in the kernel, right
after its exit. Single steps begun there go through the kernel's return to
user space, `sysretq` or `iretq`, which loads flags saved before the steps
began and so drops the trap flag. At such an instruction a debug register
traps the user address it returns to, and the steps go on from there; gdb's
own `stepi` in the kernel does the same. A `syscall` from user space drops the
flag the other way: it enters the kernel with the flags FMASK leaves, which
Linux sets to clear it, so the kernel would run on unstepped past the exits it
makes. At a `syscall` a debug register traps the kernel's entry, the address in
LSTAR, so one instruction back from a step's start is the `out` or `in` that
made the step's exit, even when the step before entered the kernel from user
space. Exceptions and interrupts clear the flag on their way in too, and their
handlers can make exits, as a fault's does when it records the signal or prints
a segfault to the console. Every IDT stub but those of #DB, NMI, #DF and #MC
calls one entry, Linux's error_entry, after a few instructions; the VMM finds it
by decoding the page fault's stub, read from the IDT, and every single step
traps it, so a fault or an interrupt, in user space or the kernel, stops the
step there. The trap takes a free debug register, else one borrowed from gdb
for the step: the last that is neither a read-write watch nor a breakpoint at
the stepped instruction. A step then stops at the entry whether or not a
register was free, as a replay that goes back over the step, with every
register free, does; before the borrowing, a step with all four registers
gdb's ran on through the fault's handler, and a step back landed in the wrong
place. A write watch borrowed misses a write the stepped instruction makes,
so its bytes are read before the step and compared after, and a change is
reported as the watch's hit. With four read-write watches nothing can be
borrowed, and the step runs on through the handler as before. The stub's few instructions run in that
step unstepped; the VMM finds them from the return address the stub's call left
and says which they were. One instruction back from the entry is the stub's
call, reached from the boundary before the step by a run to a breakpoint there,
and back from each stub instruction is the one before it, then the instruction
that faulted, so reverse-stepi visits each once. A step forwards that ran a stub
unstepped goes back, by the same kind of run, to the stub's first instruction,
so stepi visits them too, in the other order; a fork that cannot replay its run
stays at the entry. An external interrupt runs its vector's stub first, which
pushes the vector and jumps to the common interrupt stub; the VMM finds that
stub from the vector, through its gate, and counts its instructions among the
stub's. The `iretq` back from a fault
loads the resume flag, which would let the faulting instruction run past the
trap at its address, so the flag is cleared in the frame for the step and put
back after it. A fork whose registers or memory
gdb changed, or that left its recording, cannot go back, since the run replayed
is not what the fork did.

A process's thread on the CPU shows its user registers from its struct pt_regs
while the CPU runs the kernel, but for a few instructions on the way in and out
the struct is not complete. Before the syscall entry's switch to the task's
stack, or after the exit's switch back, the CPU's rsp is the user one, and the
user pc and flags are in rcx and r11; with rsp inside the struct, the words below
it are not yet pushed, or already popped, and are the CPU's own, the user rsp in
the TSS word where the entry keeps it. An exception or interrupt from user space
pushes the registers on the CPU's entry stack, at the TSS's sp0, and error_entry
builds the struct there before copying it to the task's stack; while rsp is on
the entry stack, the frame at its top holds them. Until error_entry stores rdi
over the return address the stub's call pushed in rdi's word, rdi is the CPU's.

Under exit time a step can be millions of instructions, and going back across
one single-steps all of them, each a VM exit, which can take minutes; a run
back to a watch on a value a loop writes stops at every write. Every 64 VM
exits, single steps and trap hits alike, counted across the command's whole
replay, and between chunks of whole steps, the fork peeks at gdb's connection.
The count does not start over at each move, so a command that replays the hits
an earlier search found listens as often as the search did. A Ctrl-C stops the
fork at the next position it comes to, a position like any other, so gdb reads
registers there, steps forward and goes back again. A hit in another process is
between positions and waits for the next. A closed connection ends the session
at once. On a loop of 10 million rounds the stop comes 10 to 60 ms after the
Ctrl-C, in a first search or in a replay of one. A position's moves are a list each position
shares with the one it was made from, so the position after each of many hits
in one step costs the same to make however many came before.

Forwards, a Ctrl-C has to reach a guest that makes no exits, such as a loop
spinning on a flag, where the run loop alone would never look. A thread of
the session watches gdb's connection while the fork runs forwards and, at the
first byte, interrupts the machine: it sets the `immediate_exit` flag in the
vCPU's kvm_run page, which makes a KVM_RUN entered after it return at once,
and signals the vCPU thread, which pulls one already in the guest out. The
interruption is no exit of the guest's: the vCPU stops between two
instructions, nothing reaches the trace, and the next run goes on from
exactly there, so a debugged fork runs as it would have. The stop comes
within a few milliseconds. The fork is then between two steps, at no position
the run replays to, so going back is refused, saying so, until the fork
reaches the next step's start. The desktop app's Cancel takes the same path:
it sends the engine SIGTERM, which stops every machine the process has the
same way instead of ending the process, and the command ends as interrupted.
A process with no machine yet, or still alive two seconds later, is ended as
SIGTERM would have ended it.

`rewind blame RUN --at STEP EXPR` asks the same question as a watchpoint
run backwards, who last wrote this value, without a debugger session, and the
desktop app asks it when a value in the source panel is clicked. gdb, on a fork
at the step, evaluates the expression in the thread's frame and says where it
is: in memory at an address, in a register, or optimized out. A value in memory
and at most 32 bytes, the four debug address registers' 8 bytes each, is
watched while the run is replayed a keyframe window at a time, newest first,
from the latest keyframe before the step up to it, then the window before, back
to the process's start or last exec. The windows are replayed several at once,
one thread and one machine each, as many as there are cores, and their accesses
are taken newest window first, each once every newer window's are in, so the
search keeps what one window at a time would keep and stops the rest once it
has enough. On a run whose write is 300,000 steps and 24 windows back, the
search took 4.4 s on 16 cores where one window at a time took 8.4 s; a write
in the first window costs the same either way. A hit counts when the CPU had the
process's page table: one of its threads, or the kernel copying into the value
for one of them. The trap comes after the store, so its line is the line of the
address just before the trap's pc. One store can cover several values, such as
a 64-bit store over two 32-bit fields, and DR6 sets a bit for each register it
matched, so the store is a write of each. x86 cannot trap reads alone, so to
find reads each piece of a value takes two registers, one trapping writes and
one trapping reads and writes: a stop that set both bits wrote the value, one
that set only the second read it. A search for reads therefore watches at most
16 bytes, and one replay orders its reads and writes exactly, even a REP MOVSB
that reads a byte in one iteration and writes it in the next at the same
instruction. A search keeps each value's last writes, or with `--reads` the
latest `--max` accesses, as it replays, so a loop of many writes costs replay
time and not memory; a capped search says how many it left out and goes no
further back. A second gdb
session, on a fork run into the last write's step to just after the write,
names each access's line and where the process's other threads were. A step
runs from one VM exit to the next and can cover many lines, so the start of the
write's step can be well before the write; `rewind where --after-write` looks
just after a write the same way. On the mylib shutdown race, the last write of
`p->queue` before the crash is found 14 steps back, `pool_shutdown`'s
`p->queue = NULL`, in about 2 s, most of it the two gdb sessions.

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
linked at. A dynamically linked process's main program instead becomes gdb's
symbol file, with the kernel added beside it, and its libraries reach gdb as
gdb's SVR4 library list, which the stub builds the way gdbserver does: the
main program's `DT_DEBUG` points at the loader's `r_debug`, whose link maps
name every loaded object with its load offset, matched to the files Rewind
has by their dynamic sections. gdb then knows each library's link map, which
it finds a library's thread-local variables through; gdb 17 does that itself,
and the Nix package ships it. An older gdb gains nothing from the list, so
`rewind gdb` says what it cannot do and loads every file one by one for it.
The interpreters' and compilers' own gdb support comes in too when it is here:
CPython's `libpython.py` beside a store libpython, and the pretty printers of
the rustc that built a Rust program, found among its derivation's inputs or
else from the rustc on PATH, as rust-gdb finds them. For the files only the
VM had, gdb lists the source files their
DWARF names, and a `files` inspection reads them in the process's view; the
list goes in as console input, since a request has room for few arguments.
Rewind keeps what the inspection read in `cache/sources/<run>`, by the step of
the last event that wrote, renamed or unlinked each path, or as original when
none did, and keeps the paths the VM did not have as absent. A later lookup
whose step has the same last event for a path reads it from there, and makes
no `files` fork when every path is there. An open for writing counts once
every process holding it, the one that opened the file and any forked from
a holder since, has closed it or exited, since the open marks only the
start of its writes; until then the file is read from the VM each time.

DWARF and sources for everything in the store come by build ID from
nixseparatedebuginfod2, which Rewind starts for the session on a socket it
binds itself and hands over the way systemd's socket activation does. It serves
the local store's `debug` outputs and cache.nixos.org's, so glibc's DWARF and
source arrive as gdb asks for them. The first time, the server fetches a
library's `debug` output or its source from the cache before it answers,
which can take a minute, and gdb says nothing about a download until the
answer starts. `rewind where` runs gdb with `DEBUGINFOD_VERBOSE` set, so
libdebuginfod logs each URL it asks on standard error before it waits, and
says `rewind: downloading debug info for libc.so.6; first time only` as each
download starts. A library's sources are said once per command, as
`downloading source files of libc.so.6; each one only once`: the client
keeps each source file on its own, so a later command that reads other
files of the library downloads again. `rewind gdb`
shows gdb's own lines about downloads. The server keeps what it knows of its
cache in memory, so two servers on one directory break each other's entries:
each session's server takes a numbered directory under `cache/debuginfod`
that no other holds, marked by a lock on the file beside it that the server
inherits. Taking one moves in, by renaming each entry, what the directories
no session holds have and it lacks, so later sessions find what earlier ones
fetched; it deletes nothing, so it never waits on a large tree. `rewind gc`
removes the directories no session holds. The kernel's `debug` output is nixpkgs'
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
`/dev/rewind-stdout`, `/dev/rewind-stderr` and `/dev/rewind` for marks. It
also adds a terminal of two lines, `/dev/rewind-tty0` and `/dev/rewind-tty1`,
raw as Nix leaves a builder's pseudoterminal: a Nix build's output goes there,
as does a command's run with `--tty`, so programs that ask see a terminal and
write a line at a time, as they do under nix-daemon. Each write to either is a
record with the pid that made it. Each report is a record in a static buffer. The kernel writes the buffer's physical
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

#### Read and write records

The kernel also keeps a record of each write to a regular file and each
open of one. Sending each of these as a record above would cost an exit,
and an exit is a step, so it would change the run. Instead the kernel
appends them to a 1 MiB buffer in its memory, and the shared page says
where the buffer is and how full it is. At every step the monitor takes
what the buffer holds and sets it back to empty. A guest with records and
the same guest without them make the same steps, because no record adds an
exit.

| Record     | What it holds                                              |
| ---------- | ---------------------------------------------------------- |
| Write      | The thread, the file, where in the file, how many bytes    |
| Read-open  | The thread and the path of a regular file opened to read   |
| Write-open | The thread, the file and its path, opened to write         |
| Lost       | How many records did not fit in the buffer since last step |

`write`, `writev`, `pwrite64`, `pwritev`, `pwritev2`, `copy_file_range`
and `sendfile` make write records. Writes through a memory mapping, `splice`
and io_uring make none. The kernel adds the open records in the same hold of
its path lock as the open's own event. Taking the lock a second time there
let another thread run first in a sagittarius build, which changed the run.

If the buffer fills before the next step, the records that do not fit are
lost, and the monitor stores a Lost record with their count at that step.
With the 64 KiB buffer of a first version, a sagittarius build lost 29,678
records at 11 steps, in stretches of up to 19,000 writes with no exit
between them. With 1 MiB it lost none.

A guest kernel names the version of its interface with the monitor in the
shared page, and the monitor runs a kernel of its own version only. Version
4 added these records, version 5 the record at every context switch (see
[Exploring interleavings](#exploring-interleavings)), and version 6
changed what the records say: a file event names the path the kernel
resolved, a crash is reported once, and RDTSC traps in every task. A run
recorded with an older kernel is recorded again.

### The CPU model

Software picks code paths by the CPU it sees. glibc chooses its string
functions by the vector extensions and cache sizes CPUID reports, and the
kernel chooses mitigations by the CPU's known bugs. A run made on one CPU
therefore takes different steps on another. By default the guest sees a fixed
CPU instead of the host's:

- x86-64-v3 features (AVX2, BMI, FMA, MOVBE), plus AES, PCLMULQDQ, ERMS,
  FSGSBASE and INVPCID;
- no AVX-512, SHA, protection keys or speculation control bits;
- an XSAVE area of exactly x87, SSE and AVX state;
- a fixed family, model and brand string per vendor, fixed cache sizes, and
  39 physical and 48 virtual address bits.

That is `--cpu v3`. Not every x86-64-v3 host shows ERMS, the fast `rep movsb`
flag: an EPYC 7763 (Zen 3) host was found with every other bit but not ERMS.
`--cpu v3-compat` is the same CPU with ERMS hidden and a brand string of its
own. glibc and the kernel copy memory differently without ERMS, so the same
inputs make a different run under each model. A run that names neither picks
`v3` where the host has the whole baseline, and `v3-compat`, with a line saying
so, where the host lacks only ERMS. A `v3-compat` run replays on hosts with or
without ERMS, and a `v3` run only on hosts with it. A host that lacks any other
bit refuses both models and names the missing bits. A replay, fork, shell or
gdb session of a recorded run on a host without its model's features is refused
with the model, the missing bits and where the run replays instead, since its
model cannot change. `rewind doctor` says which model a run without `--cpu`
gets on the machine it runs on. The model is part of a run's inputs, so replays
and forks use the recorded one, and `--cpu host` shows the host's features
instead, for hosts older than the baseline. The extras slot `rewind shell
--with` fills ends at 320 GiB, which needs the model's 39 address bits, and KVM
refuses a memory slot past what the host's CPU reaches. On a CPU with 36 bits,
such as an Ivy Bridge, `--cpu host` records runs without the slot, so they
cannot take `--with`. The guest boots with `mitigations=off`. Its processes
need no protection from each other, and KVM still guards the host. With the
fixed CPU, the mitigations the kernel had chosen more than doubled a build's
time.

### The monitor

`rewind-vmm` is a few hundred lines on top of rust-vmm's `kvm-ioctls`. It
creates a VM with the in-kernel local APIC and no PIT. It maps guest RAM with
dirty logging and the input image as a read-only memory slot. A run that takes
no keyframes, as most of `rewind check`'s runs do, maps guest RAM without dirty
logging: KVM then backs the RAM with 2 MiB pages, where logging splits the RAM
into 4 KiB pages to note each write. The monitor then loads vmlinux's segments
where they were linked, enters the kernel with the 64-bit boot protocol and
runs one vCPU. Its run loop handles each exit, counts it, moves time, and sends
the interrupt if the timer
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
  writable tmpfs overlay and chroots into it. A root whose `/etc/hosts` is
  missing or empty, as `docker export` leaves it, gets `localhost` in the
  overlay, as a Nix build has.
- **A Nix derivation** (`rewind nix`, the Nix angle). `rewind` realises the
  derivation's inputs on the host and packs the input closure and the sandbox
  shell into an image. The guest mounts it at `/nix/store` under a writable
  overlay. The builder runs with the environment nix-daemon's `initEnv` would
  give it, `passAsFile` files included, as uid 1000 in `/build`. After a
  successful build, init reports each output's NAR hash, the hash Nix
  records as a store path's narHash. `rewind nix` compares it with the
  host's copy of the output, if the host has one, and with the build each
  HTTP binary cache Nix substitutes from publishes. It fetches only a
  cache's `.narinfo` for the path, never the NAR, logs in with the netrc
  file Nix is configured with, and skips substituters that are not HTTP
  caches. A store path names a build's inputs, not its contents, so a build
  that differs mostly says the package is not bit-reproducible; one that
  matches says the VM built the same bytes. GNU hello and pkgconf built in
  the guest match cache.nixos.org's builds.
- **A build Nix describes** (`rewind build`). Nix's `external-builders`
  setting hands a program a JSON description of one build: the builder, its
  arguments, the environment nix-daemon has already worked out, the input
  closure, and the files it wrote into the build directory. The command
  makes the same job of it as of a derivation, with the description's
  closure as the image and its files as the job's, and calls no `nix`
  itself, so it also runs inside Nix's sandbox, where a derivation's
  builder can write the description of its own build (`inRewind`, in the
  flake's `lib`). The derivation pins the machine in `rewindFlags` and the
  Rewind package in `rewindRuntime`. The running package must match; the
  kernel and initrd come from that package. When
  the builder exits 0, each output is read out of a fork of the run at its
  last step, as `rewind cat --archive` reads a tree, and written to the
  store. The [derivations tutorial](tutorial-derivations.md) uses `inRewind`;
  the [Nix build reference](nix-builds.md) covers both routes.

The guest's wall clock starts at midnight UTC of the day the run was made.
Configure scripts compare the clock with the timestamps in source tarballs, so
1970 does not work, and the value is an input like any other, so a replay uses
it again.

### Directory order

`readdir` returns a directory's entries in the order its filesystem keeps
them, and in the guest that order is the same in every run. tmpfs, which
`/build` is on, lists the newest entry first. erofs, the input image, lists
entries by name. A merged overlayfs directory lists the lower layer's
entries, then the upper's. On a real machine, ext4 lists a directory in the
order of a hash of each name, keyed by a seed chosen when the filesystem was
made, so two machines can list one directory in two orders. A build that
writes out a directory's entries in the order it reads them, unsorted, then
differs from machine to machine, and the guest never shows it.

The guest kernel's second patch reads `rewind.dirents=N` from the command
line. With N other than 0, every directory on tmpfs, ramfs, overlayfs and
erofs lists `.` and `..` first and the rest in the order of a SipHash of
each name keyed by N, as an ext4 made with another seed would. Without the
argument, or with 0, each filesystem keeps its own order. The order depends
only on the names and N, so a run made with a key replays, and the command
line is part of the run's spec, so the key is one of its inputs and part of
its id.

A `getdents` call at position 0 reads the whole directory, sorts it, and
serves that call and the ones after it from the sorted copy; the position
is an index into the copy. An entry created after that call shows up only
after the program reads from position 0 again, which POSIX allows.

setuptools 80.9.0 in nixpkgs is a build this finds. Under key 1,
`/build/source` lists `setuptools.egg-info` before `bootstrap.egg-info`, and
the wheel's `WHEEL` file says `Generator: setuptools (80.9.0.post0)`. Under
key 2 the two directories list the other way round, the build takes its
version from `bootstrap.egg-info`, whose `PKG-INFO` says `Version: 1.0`, and
`WHEEL` says `Generator: setuptools (1.0)`. The guest's own tmpfs order
gives the first of the two.

`--kernel-args rewind.dirents=N` sets one key for `rewind run`, `rewind nix`
and `rewind check`. `rewind check --dirents N` tries keys 1 to N as well:
it runs the first seed's unperturbed schedule again from boot under each
key and judges each run against schedule 0, the stock order, by the same
properties. Each key has a line of its own, and the count with `--all`
includes the key runs. A key's run is never perturbed. One that ends
differently is reported as a seed's is: against schedule 0, with no step
to narrow to, since the key is in the run from boot, and both runs get
keyframes. With `--seeds`, only the first seed runs under the keys.
`--dirents` is refused with `--run`, whose inputs stand, and with a
`--kernel-args` that sets the key.

## Runs on disk

```
~/.local/share/rewind/
  runs/<id>/manifest.json     inputs, what they describe, parent, outcome, verdict
  runs/<id>/trace.bin         every record, each prefixed with its step
  runs/<id>/keyframes/*.kf    keyframes, by step
  images/2/*.erofs            input images, by content
  store/                      the page store
  cache/sources/<id>/         source files read out of a run's VM
  cache/runs.json             each run's name and age, for finding one
  lock                        held while a process adds images or runs
  runs.lock                   held while a run is registered or runs are removed
```

Images live in a directory named for how they are built, so an image an
earlier way of building made is not taken for one this way makes. Runs that
booted an older image keep naming it where it is, in `images/` itself.

A command names a run by its id, a prefix of it, its name, or `@N` for the
Nth newest. A prefix is matched against the directory names in `runs/`. A
name or `@N` needs every run's name and creation time, which only its
manifest holds, and reading every manifest took 78 ms in a home of 6,233
runs, for every command. So `cache/runs.json` holds, for each directory in
`runs/`, what its manifest said when last read. The paths that make,
replace and remove runs (record, fork, check, import, remove, prune) bring
it up to date, under a lock of its own, since every process executing a run
holds the home's lock shared. A reader that finds it missing, another
build's, or naming other directories than `runs/` holds rebuilds it from the
manifests, and a lookup by name opens the run it names and checks the name,
so a writer that does not know the index costs one rebuild. In a synthetic
home of 6,000 runs, `rewind show <name>` took 379 ms before and 33 ms
after, and `rewind show @5` 292 ms and 39 ms, on a host running other
builds at the time.

Besides the inputs, a manifest records `outcome`, how the machine stopped
and the job's wait status, and `verdict`, whether the run passed and, when
it did not, every reason: a timeout, an exit status other than 0 or a
signal, a machine that stopped before init reported the job's end, the
outputs an exit of 0 did not create, and the SDK assertions the job broke.
The engine judges the run once, as it ends, in `rewind_trace::verdict`;
`rewind ls`, `check` and `show`, the desktop app and the browser viewer
read the field and judge nothing themselves, so a job that breaks an
`always` assertion and exits 0 is failed in each of them. `rewind
summary` says the verdict in plain text, with the step the run failed
on, the one the app's failure marker sits at (`rewind_trace::failing`
finds both), and the job's last lines of output. Every export carries
the same text as `summary.txt`, for a person or a program without the
app, and the browser viewer shows it on a screen too narrow for the
app. A run that has
not ended has the verdict `unfinished`, and whether a process is still
executing it is told by its executing lock. A manifest also records
`trace_hash`, the BLAKE3 hash of
`trace.bin` once the run has finished, so runs that did the same thing are
found without reading their traces. A fork records `parent`, the run and step
it was forked from, `first_difference`, the step where its trace first differs
from its parent's (absent when the two are identical), and `shared_keyframes`,
described under [Forks](#forks).

`rewind export --with-answers` replays the run and packs in, at the
failing step, at each broken assertion and at each bookmark, where each
live thread of the job was, as `rewind where` says it, and the files
the job had written by then: `answers.json` and a member under
`answers/` for each file's bytes, ahead of anything only a replay needs.
A file is kept to its first MiB and the files to 32 MiB between them,
and the index says what was cut or left out. A source file gdb fetched
through debuginfod is named as the program's DWARF names it, not by its
place in the client's cache under the exporter's home directory. The
browser viewer, which has no engine, reads files and source at those steps through
`packed::Answering`, an engine that answers from the export and hands
every other question on; at other steps it says the desktop app does
it. Source and file contents say more of a job than a trace does, so a
plain export carries none.

`trace.bin` is a sequence of events. Each is its step as eight little-endian
bytes, followed by the record exactly as the guest wrote it. `rewind-trace`
reads it and answers the scrubber's questions: which processes were alive at a
step, what they had printed, which files they had written, which build phase
it was, and where two runs first differ.

The guest gives a pid out again once its process has exited, so a pid alone
does not name a process for the whole run. A life of a pid, `Life` in
`rewind_trace`, is the pid and the step the trace first saw it: the fork
that made the process, or 0 for a process first seen as a parent. Every fork
is a step of its own, so two lives of one pid never share a start. A reader
resolves an event's pid to a life at the event's step and holds the life
from then on; each process records the life that forked it. The trace format
carries no life of its own: the fork's step is in every trace already.

The read and write records of a step are stored in the same file, as one or
more records of kind 13 at that step, before the record the step itself
makes, if any. The guest never sends kind 13. Their payload is a compact encoding, in
`rewind_trace::io_records`. Each file gets a number the first time a
record names it. Each path is stored once, as the bytes it shares with the
path stored before it and the rest. A write's position is stored as the
distance from where the file's last write ended, which is 0 for a file
written front to back. A thread is stored only when it changes. Numbers are
varints. Decoding a record needs the records before it, so a machine
restored from a keyframe first reads the trace up to the keyframe to go on
encoding the same way. The records are not events: the scrubber and the app
never see them, and traces without them, from older runs, read as before.
A trace's format version stays 1, since the framing did not change.

The records cost 190 KB on a neatvnc build, whose trace was 424 KB before
(+45%), and 2.0 MB on a sagittarius build, whose trace was 7.08 MB
(+28%).

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

Runs take a keyframe about every quarter second of the guest's work. The
interval reads only how far the guest got: its steps, and the conditional
branches it retired in user mode, weighed at 22 µs a step and 0.75 ns a branch,
a fit of each stretch's wall time in GNU hello's nixpkgs build on a Zen 4
laptop. A Zen 3 server took about twice each. So a run keyframes at the same
steps on every host, and a slower host seeks for longer instead of taking more
keyframes. The recording stops about eight times an interval to add up the work
and take a keyframe once a quarter second of it has passed, so where steps start
doing more, as when a build turns from starting processes to compiling, a
keyframe comes at most an eighth of an interval of the old steps late. Spaced by
wall time, as they were before, the same build took about 50 keyframes on the
laptop and 280 on the server, where 200 ms of each 250 ms stretch was the
keyframe. A run also takes one where init reports the job's exit, so a job too
short for a second keyframe by its work, as most one-line commands are, still
has the whole machine compared after it ran rather than only after boot.
Seeking to a step restores the latest keyframe at or before it and
runs forward, so a seek costs at most about one interval. `rewind replay --from`
restores a keyframe, runs to the end and compares the rest of the trace with the
original, events and read and write records both. Every keyframe checked this
way reproduces the rest of its run exactly. The record buffer is empty at every
step, so a keyframe needs nothing of it beyond the shared page.

A fork that looks inside a run, for `rewind where`, `gdb`, `cat` or `shell`,
also keeps a keyframe at its step when the latest one before it is more than
512 steps back. `rewind where` forks two or three times at one step, and the
app asks again wherever the playhead rests, so the later forks restore that
keyframe instead of replaying to it. In a mylib build of 5102 steps, a lookup
at step 5060 replayed 3621 steps from the keyframe at 1439 in 0.28 s on each fork, and
keeping the keyframe took 0.1 s. The keyframe holds the pages written since
the one restored and goes where a fork's shared keyframe goes (see
[Forks](#forks)), written under that run's executing lock with the page store
open, so `rewind gc` waits. A run recorded without keyframes, as most of the
runs `rewind check` makes are, counts boot as its keyframe at step 0, so the
first look inside it more than 512 steps in keeps a full keyframe, every page
that is not zero. In a run of 5947 steps that rewrites 64 MB of files, the
first `rewind cat` at step 5000 took 3.6 s instead of 3.0 s, and each later
one there took 0.5 s instead of 3.0 s.

A long look keeps keyframes on the way too. It replays toward its step in
stretches of about a quarter second of the guest's work, weighed as a
recording weighs it, and after ten seconds of that work since the keyframe it
started from or last kept, it keeps another, written the same way, so a later
look anywhere on the way replays at most about ten seconds of work, and looks
on any host keep them at the same steps. Each holds the pages written since the
last, which costs disk, so the interval is forty times recording's. A check run
of a sagittarius build, 296,484 steps recorded without keyframes, was read at
step 290,000 and then at steps 100,000, 175,000, 250,000 and 289,000, in a
page store holding its schedule 0's pages:

| Kept on the way    | First read | Keyframes | .kf files | Pages added | Later reads      |
| ------------------ | ---------- | --------- | --------- | ----------- | ---------------- |
| none, only at step | 191 s      | 1         | 6.2 MB    | 183 MB      | 10, 75, 75, 38 s |
| every 2 s          | 218 s      | 48        | 47 MB     | 829 MB      | 3.7 to 5.3 s     |
| every 5 s          | 210 s      | 27        | 35 MB     | 645 MB      | 4.8 to 8.0 s     |
| every 10 s         | 203 s      | 15        | 24 MB     | 470 MB      | 3.1 to 10.5 s    |

The first read's own keyframe, every page of the machine, is most of the
first row; each keyframe on the way added 14 to 21 MB of pages, however long
its stretch. A keyframe whose step a fork's parent owns is kept there only
when the parent reads the keyframe it is a delta from.

A replay toward a step says how far it got: as it starts, every half second
and at its step. On a terminal the command draws it as a status line;
`--progress json`, or `REWIND_PROGRESS=json`, prints a line of JSON each
time instead, with the run, the step it started from, the step reached, the
step it goes to and the milliseconds taken. The desktop app asks for JSON,
and while it waits on a read it shows the replay with a bar, the time left
and a Cancel button once the time left passes three seconds. The time left
goes by the speed over the last 20 seconds of replay, after the first five:
a replay restored from a keyframe starts slow while the guest's memory is
read back in, and the guest's own work changes the speed. The sagittarius
run above replayed from boot to step 100,000 in 10 s and then about 1,000
steps a second, so at step 150,000 the speed so far would say about 56 s
were left of a replay to step 290,000 that takes 140 s more.

A command that fails says why after `rewind:`. With `--errors json`, or
`REWIND_ERRORS=json`, it prints one line of JSON on standard error instead,
`{"error":{"kind":…,"message":…}}`, whose `kind` names the failure with what a
program needs to act on it and whose `message` holds the words. The desktop app
asks for it, so it tells a replay that went another way than its recording
(`went_another_way`, with the run and step) from other refusals by its kind,
not by matching the words, and stops asking to bring that run to a step. Kinds
the app does not know read as `other`.

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
with zstd at its fastest level. The all-zero page is never stored. A keyframe
hashes its pages, and compresses the new ones, on up to eight threads, then
writes them in order on the monitor's thread, so the store's files hold what
one thread would have written. Pages go into append-only pack files with an
append-only index. A crash can leave at most a torn last index entry, which is
dropped when the store opens. Pages no keyframe names any more stay until
`rewind gc` removes them (see [Removing runs](#removing-runs)).

The index can also name a page that is not in its pack. Each writer appends
pages to a pack of its own and entries to the shared index, and syncing the
index makes every writer's entries durable, including ones whose pages are
still in the page cache; a power loss then leaves entries without pages. A
disk that fills while an entry is written leaves part of one, and every entry
any writer appends after it is shifted and reads as nothing. Recording the
same run again does not mend either: the run is deterministic, it puts the
same pages, and the store skips them as already there. So an opener with the
store to itself, or `rewind gc`, verifies the entries written since the index
was last marked verified: an entry whose page lies past its pack's end, or
whose bytes do not decompress and hash to its name, is dropped, with every
entry after a shifted one, and the segments covering the dropped entries go.
The dropped pages are absent, and the next keyframe that has them puts them
again. The last store to close marks the whole index verified once every
pack is synced, so a session that ends well leaves nothing to verify; a
writer whose index write came up short raises a flag that keeps every close
from marking. Verifying the whole index at every open would be too slow: on
a store of 20.7 million pages, 18.5 GB of packs, reading every page took 55
s on eight threads, and checking only that each page lies within its pack,
which reads the 1 GB index alone, 1.8 s.

A store with many runs has millions of index entries, too many to read into
memory every time it opens. The index's entries are also kept in a few sorted
segments: files of entries sorted by hash, mapped into memory and searched with
an interpolation search, each covering a stretch of the index from where the
one before ends. An opener reads into memory only the entries after the last
segment. Once a million have gathered there, it sorts them into a new segment,
and the new segment takes in the segments before it, merged as it is written,
while the newest of them covers at most twice what the new one does. So each
segment covers more than twice the one after it, a store has about log2 of its
millions of entries' segments, and an entry is rewritten about that many times.
Before, a single sorted copy of the whole index was read into memory, sorted
and rewritten for every million entries added. Growing an index to 16.8 million
entries a million at a time, opening the store after each million (the
`index_growth` example of `rewind-store`, on a 4-core cloud VM):

| Index files              | Opening, all 16 | Slowest open | Written | Most memory | Lookup, found | Lookup, missing |
| ------------------------ | --------------- | ------------ | ------- | ----------- | ------------- | --------------- |
| One sorted copy (before) | 22.0 s          | 2.4 s        | 6.4 GiB | 864 MiB     | 710 ns        | 2.37 µs         |
| Sorted segments          | 6.9 s           | 1.4 s        | 2.3 GiB | 72 MiB      | 860 ns        | 2.98 µs         |

A lookup searches the segments oldest first, the oldest holding most pages, so
it costs another search for each segment past the one that has the page, and a
page not stored costs one in every segment; at 16.8 million entries there were
one to three. A missing page also costs rereading the index past the segments,
most of that column. A store that rewind 0.12 sorted is read with its sorted
copy as the first segment, until a merge takes it in.

The segments also keep a hard link to the index they describe, named
`index.segments`. A collector replaces the index by rename, so an opener
rejects segments whose link names another file, even after a 0.12 collector
that does not know about segments. Comparing only the entries at a segment's
ends misses a collection that moves an interior page. The link prevents the
old index's inode number from being reused; an exclusive opener removes a
stale link, releasing the old index. Rebinding the cache removes stale
segments durably before linking the new index. A copied store that loses the
hard-link relationship rebuilds its segments from the index.

Keyframe memory is a table of page hashes bounded by the VM's allocated RAM.
Full keyframes and deltas reject out-of-range page numbers before allocating
or changing the table, including zero pages that delta trimming would omit.
The table allocates only through the highest nonzero page and reports an
allocation failure as an error.

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

A fork can be given another seed for the guest's randomness, as `rewind
fork --seed` and `rewind check --run --seeds` do. Its spec lists the reseed
with its step, and at that step's interrupt the monitor writes 32 bytes
derived from the seed and the step into the shared page and the kernel mixes
them in and rekeys its RNG, so `getrandom` returns other numbers from there
on. A reseed parts the fork from its parent at its step, as a schedule does,
and a fork of the fork keeps the reseeds before its own step.

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

### Removing runs

`rewind remove <run>...` removes the runs and every run that descends from
them through `parent`, with any inputs an import placed for them, the
deepest first, so a removal cut short never leaves a fork whose parent is
gone. It removes nothing while a process is executing one of those runs, or
while a run outside them reads keyframes from one of them, and names that
run. A run whose execution was killed is interrupted, and goes like any other.
Such a reader is rare, since a run reads keyframes only from its parent, but
running a fork's inputs again as a plain run makes one: the run keeps the
fork's keyframes and loses its parent.

Each call reads every manifest in the home once, so many runs go in one call
rather than one call each: on a home of 8,000 runs, removing 6,000 with one
call per run took 35 minutes, and the refusals are checked once for the
whole set, so a run and the run that reads its keyframes can go together.
The runs are held, by an exclusive lock on `runs.lock`, from the listing to
the last deletion, and an execution holds the same lock shared while it
registers its run: its directory, its executing lock and its manifest. A
fork `rewind check --run` makes while runs are removed is therefore listed,
and refuses the removal as executing, or is registered after and refused
when its parent is gone, and never left with a parent the removal took.

`rewind prune <run> --identical` removes forks in a run's family, its forks
and their forks, whose `trace_hash` equals an older member's. The run itself
always stays, and so does any run another run here names as its parent or
reads keyframes from.

Both take the source files cached for the runs they remove. Neither command
removes images or pages, which other runs may share. `rewind gc` removes the
images in `images/` and its subdirectories that no run's manifest names, the
pages that no keyframe in any run's directory names, the source file
caches of runs no longer in `runs/`, and the debuginfod caches under
`cache/debuginfod` that no `rewind gdb` session holds. It also removes what
a killed process left half made: temporary images in `images/` and
`importing-<pid>` directories, each once its process is gone.
A fork reads keyframes from the directories of the runs it shares them with,
so counting every directory's keyframes counts every keyframe a run reads.
`--dry-run` reports the same figures and removes nothing. On a machine with
7,155 runs it found 23 of 81 images (22.8 GB) and 15 million of 18.6 million
stored pages (16.5 GB) unused.

`rewind gc` refuses, removing nothing, while another process may be about to
use something it would remove:

- **A process holds the home in use.** `rewind run`, `nix`, `check`, `fork`,
  `shell` and `import` hold a shared lock on the home's `lock` file until they
  exit, and `rewind gc` takes it exclusively. An image is packed before the
  manifest of the run that boots it is written, so between the two no run
  names it; the lock covers that window. An extras image for `rewind shell
--with` is named by no run at all, and goes like any unused image once no
  shell holds the home in use; the next shell with the same packages packs it
  again.
- **A run is executing**, by the same test `rewind remove` uses, for a
  process that takes no home lock.
- **Another process has the page store open**, such as one seeking in a run.
  Every store holds a shared lock on `store/lock`, and `rewind gc` takes it
  exclusively, so a store opened while it runs waits for it to finish.

Removing pages rewrites the store. The pages that stay are copied, as stored,
out of every pack that holds anything else into new packs, and a new index of
only those pages replaces the old one with a rename. The old packs are deleted
after the rename. A crash before it leaves the old index naming every page
where it was, and one after leaves the new index naming every page where it
is; either way the next `rewind gc` deletes the packs no entry names. The
copy needs as much free space as the pages that stay in the packs it
rewrites.

## Exploring interleavings

A deterministic machine runs one interleaving of a multithreaded program, the
same one every time. To find the others, a schedule seed perturbs a run in
three ways. Each is a fixed function of the seed and the step, so a perturbed
run repeats as exactly as any other.

- **Reschedule requests.** At one exit in four, the monitor asks the guest to
  reschedule. The kernel marks the current task, and the scheduler picks
  again on the way back to user space. With nothing else runnable, this
  changes nothing.
- **Stalls.** At one exit in 128, the task running then sleeps when it next
  returns to user space, for 10 microseconds to 1.28 milliseconds, each
  doubling as likely as the next. A busy machine deschedules a process for
  stretches like these, and many races need one task held up for a while
  rather than switched away from for an instant. `--stall-max` allows
  longer stalls (see [Longer stalls](#longer-stalls)).
- **Timer slack.** A timer armed at a perturbed step fires up to 50
  microseconds late, which Linux's default timer slack for user tasks allows
  on real hardware. Sleepers wake in a different order.

A schedule changes neither the inputs, nor `--seed`, nor the wall clock at
boot (`--epoch`). Values a program draws from the kernel's randomness can
still differ under another schedule, such as the ephemeral port a socket
binds: the kernel's generator starts from the same seed, but the schedule
decides which process draws from it first. Address space layout randomization
draws from the same generator, so a program started after the schedule
diverges can load at other addresses too. `--seed` moves the layout outright:
one spinning program loaded at `0x558577205154` with seed 0 and
`0x558a0708d154` with seed 1, and at the same address under schedules 0 to 7.
`--kernel-args norandmaps` turns address randomization off, which put it at
`0x555555555154` under every seed, to tell an interleaving apart from a layout
change.

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
   started, until one's verdict differs from schedule 0's. The first batch
   of them, as many as `--jobs`, starts while schedule 0 goes on: once
   schedule 0 has passed the job's start, it makes the keyframe the batch
   starts from durable and lists it, and hands the batch its trace up to
   there, the switches pending at that keyframe included. Each later seed's
   unperturbed run does the same for that seed's first batch. The search
   still reads the runs in the order it would have made them one after
   another, so what it prints is the same. A batch the search then drops,
   as when its seed's unperturbed run ends differently, is removed as
   `rewind remove` removes runs: only the runs whose directories its
   executions made, never a run of the same id that was there already.
   By default a run
   is judged by its exit status and output hashes against schedule 0's;
   `--property` judges it by what a correct run is (see
   [Properties](#properties-seeds-and-the-sdk)). A schedule can make a program loop forever, so each run gets ten
   times as long as schedule 0 took, and at least a minute, unless
   `--timeout` says otherwise. A run still going then is stopped, and ends
   as timed-out. Whether a run times out depends on how fast the host is.
   Schedule 0 has no limit unless `--timeout` is given, which then bounds
   every run, schedule 0 included: a long build is legitimate, but a test
   suite that might hang on schedule 0 should be checked with `--timeout`.
   A run that timed out says whether the guest was still making exits,
   which is a slow run, or had gone a second or more without one, which is
   a guest stuck computing; then it says for how long and at which
   instruction, in user space or at a kernel symbol. A user-space
   instruction is named with the program's symbols, as `rewind gdb` loads
   them at the run's last step, by function, offset and source line, with
   the process the VM's kernel had on the CPU: "in user space in spin+11
   (spin.c:5), process 38 (spin)". The manifest keeps the bare address.
3. Narrows the window by bisection, first its start and then its end, to
   a window that still makes that schedule end differently and stops
   doing so if its start moves one step later or its end one step
   earlier. Perturbing more steps can hide a failure as well as cause
   one, so this is not always the smallest such window in the run.
4. Names the window's last step, which decides how the run ends. Narrowing
   has already run the window one step shorter, which ends like schedule 0,
   so that run and the window's are the same run until the last step and
   differ only in what the schedule does there. `check` says what that is:
   a reschedule, a stall, a late timer, or more than one. A late timer
   changes the run only if the step's exit armed the timer, which the trace
   does not record, so `check` replays that one exit and the monitor says
   whether it did.
5. Compares the failing run with the run one step shorter, not with
   schedule 0, and shows where the failing program's own events first
   differ between them. It compares threads by the order they appear, not
   by their ids. On mylib the test's events part 2 steps after the
   deciding step, where the other worker runs ahead of the one held there,
   and the crash comes 22 steps later: the first difference is the cause,
   and the crash its symptom. Against schedule 0, which differs from the
   failing run from the window's start on, they part 305 steps before the
   crash.
6. With `--where`, says where the threads involved were, as `rewind where`
   finds them: the thread on the CPU at the deciding step, and the thread
   of the failing run's first event that differs, each by the line of the
   program's own code it was on. Each takes a fork and gdb, and the first
   for a build can wait for debug info to download, so it is asked for.

For mylib, whose shutdown test fails in about one host build in eight, 9 of 64
perturbed schedules fail with counter time and 34 of 64 with exit time.
Counter time's rate is close to the host's because computation takes time
there, as on real hardware; with exit time all of a burst of work lands at
once, which crowds the threads together and makes the race easier to hit. The
first failure has come within the first two batches of 16 schedules, and the
whole search takes under 20 seconds on 16 cores.

A program that sizes its threads by the CPU count sees one CPU by default,
so it starts one worker, and a race between its workers never happens. The
same goes for a Nix build: `NIX_BUILD_CORES=1` makes stdenv run make, ninja
and test runners one job at a time. `--cores N` tells the guest's programs
there are N CPUs and sets `NIX_BUILD_CORES` to N. The VM still has one vCPU,
so the extra workers and jobs interleave on it, and the schedules reorder
them. Programs count CPUs in two ways, and both answer N:

- **The affinity system calls.** coreutils' `nproc`, Go's runtime, Rust's
  `available_parallelism` and musl's `sysconf` count the bits
  `sched_getaffinity` returns. The kernel patch reads N from `rewind.cpus=`
  and reports CPUs 0 to N-1. `sched_setaffinity` to any of them runs the
  task on CPU 0, and to none of them fails with `EINVAL`, as on a machine
  with N CPUs.
- **Files.** glibc's `sysconf`, and so Python's `os.cpu_count` and C++'s
  `hardware_concurrency`, read `/sys/devices/system/cpu/online`, and older
  build scripts count the blocks in `/proc/cpuinfo`. Init writes versions
  of `online`, `possible`, `present` and `cpuinfo` for N CPUs and
  bind-mounts them over the real ones before it makes the job's root, so
  a Nix build and an image root both see them. Doing this in init rather
  than the kernel keeps the patch out of sysfs and procfs.

Two examples, each under 64 schedules:

| Workload                                                         | `--cores 1`       | `--cores 4`                 |
| ---------------------------------------------------------------- | ----------------- | --------------------------- |
| A Makefile whose `main.o` includes a generated `gen.h` unlisted  | 0 end differently | unperturbed builds, 58 fail |
| A pthread pool sized by `sysconf`, with an unlocked shared total | 0 end differently | unperturbed passes, 64 fail |

A job that mounts its own `/proc` or sysfs sees the kernel's files again,
which say one CPU, while the affinity calls still say N.

`check --run RUN` checks a run already recorded instead of a job. Each
schedule is a fork of the run at `--schedule-from`, made with the same
`Spec::fork` as `rewind fork`, so the run stands for schedule 0 and any
perturbation of its own before that step stays. Counting and narrowing are
the same as for a job. `--no-narrow` stops at the count, which is what the
app's Check from here asks for.

### Longer stalls

Some races need a task held off the CPU for longer than 1.28 milliseconds,
because the window they need includes a sleep, a timer or slow I/O in
another task. Go's http2 `TestTransportDisableKeepAlives_Concurrency` hangs
only when one request keeps its reservation of a connection until a
handler's 25 ms sleep has passed, and under the default stalls it did not
hang in 260 runs. `--stall-max DURATION` on `run`, `nix`, `check` and `fork`, such as
`--stall-max 50ms`, raises the longest stall. It takes s, ms, us and ns, and
values from the default up to 1 s.

The steps that stall stay the same: one exit in 128. A stall picks from
the fewest doublings of 10 microseconds that reach the maximum, each
doubling as likely as the next, and the longest is cut to the maximum. The
default's 8 doublings become 14 for 50 ms and 15 for 100 ms. The longer the
maximum, the fewer short stalls:

| Longest stall | Doublings | Mean stall | 25 ms or longer | At the maximum   |
| ------------- | --------- | ---------- | --------------- | ---------------- |
| 1.28 ms       | 8         | 0.32 ms    | never           | one exit in 1024 |
| 50 ms         | 14        | 9.4 ms     | one exit in 896 | one exit in 1792 |
| 100 ms        | 15        | 17.6 ms    | one exit in 640 | one exit in 1920 |
| 1 s           | 18        | 128 ms     | one exit in 384 | one exit in 2304 |

A stall is virtual time, so a longer maximum moves the guest's clock on
further and costs little wall time, but a program's own timeouts see the
extra time. At 100 ms a run spends about 137 microseconds stalled per exit,
against 2.5 at the default. The cap of 1 s keeps a stall short of what
programs take for a hang, so their watchdogs do not fire first. The cap is
also well inside the 4.29 s that the shared page's 32-bit count of
nanoseconds holds, which is how the monitor hands the guest a stall's
length.

The maximum is part of the schedule: the spec records it as `stall_max_ns`
for the run's own window and for each inherited one, so a replay and a fork
stall the same way. The field is left out at the default, so a run made
without the flag has the spec and the id it always had. A
fork's own window takes the default unless `fork` or `check --run` is given
`--stall-max`, as its seed takes its own value, while the windows it
inherits keep their maxima. One seed under two maxima stalls at the same
steps for other lengths, so the two runs are taken to part where the window
starts. Schedule 0 stalls nothing, and `run` and `nix` refuse
`--stall-max` without `--schedule`.

On a 128-core AMD EPYC 7763 host with counter time, `check
--schedules 64 --seeds 4 --all --no-narrow` on the test above with `-race`, as filed,
had 0 of 260 runs hang with the default, 7 with `--stall-max 50ms` and 19
with `--stall-max 100ms`. With the fix applied, 0 of 260 hung under each.
net's `TestSendfilePipe`, whose failure needs one goroutine's 50 ms sleep to
end before another notices a 1 microsecond deadline, did not fail in
1028 runs under any maximum from the default to 1 s.

The cost is in what a search finds rather than in wall time. A longer
maximum makes short stalls rarer, so a race that needs only a short hold
turns up less often. On the same host, `check --schedules 32 --all
--no-narrow` of mylib took a median of 5.4 s over three searches with the
default and 5.3 s with `--stall-max 100ms`, and found the crash in 6 of 32
schedules against 3.

### Properties, seeds and the SDK

A property says whether a finished run is correct and, when it is not, why
and at which step. `check` judges schedule 0 and every run it tries by the
properties given, and looks for a run whose verdict differs from schedule
0's: one that violates a property schedule 0 holds, or holds every
property where schedule 0 violates one. Narrowing takes the same predicate,
so it is unchanged. With no property given the property is `same-outcome`,
the comparison above. A property schedule 0 violates and every run tried
violates too is the bug itself, reported at its step with no search.

Most properties read the trace alone, from the job's start to its exit, over
the processes init's first fork after the start mark forked:
`no-crash` (a death by a signal that dumps core), `no-orphans` (a process
alive after the job's main process exited, which init then killed),
`single-writer`, `atomic-replace`, `no-read-while-written` and
`writes-within`. A write lasts from the open to the close that drops the
file's last reference, a `dup2` onto it included, or to the process's exit;
a parent and a child sharing one file count as one writer, as gcc hands
cc1 its temporary file. `/dev`, `/proc` and `/sys` are not files to any of
them. `script=PATH` runs a program on the host with the run's id, schedule
0's and the job's exit step in its environment, and `junit=GLOB` reads the
JUnit XML reports the job left matching the glob, through one `files`
inspection of the run at the job's exit, and compares results by test name
with schedule 0's. A report that does not parse, as a test runner killed
while writing one leaves, is a violation that names the file in any run
but schedule 0, where it is an error.

`check`'s runs take no keyframes, and a fork at the job's exit would replay
one from boot. So with either property each run takes a single keyframe of
every page where init reports the job's exit: the machine pauses there,
takes it and runs on, and the trace is the same as without it. `check` also
judges each batch of runs at once, a thread a run, as it executes them. In
bank-java's search under `junit=TEST-*.xml`, 135 runs of about 175000 steps,
reading one run's reports took 5.4 s from boot and 0.6 s from the keyframe,
and taking the keyframe added about 1 s to a 10 s run. The search took 333 s
with neither and 82 s with both; with the default property it takes 47 s.

A run's keyframe directory holds a file named `taken` once the run has
taken keyframes as it executed. A run with keyframes only where something
looked inside it, as a `check` run has at its exit, is executed again to
take the rest when `check` reports it.

`--seeds N` tries N seeds of the guest's randomness from `--seed` on, each
unperturbed and then under every perturbed schedule. A job's other seeds
boot from scratch, since the seed is credited at boot. A recorded run's are
forks reseeded at `--schedule-from` (see [Forks](#forks)). A seed whose
unperturbed run already differs decides the run alone and is not
perturbed further: there is no step to narrow to.

The Rewind SDK is a line a program writes to `/dev/rewind` per call, kept in
the trace as a mark with its process, thread and step: `always`, `sometimes`,
`unreachable`, `test-begin` and `test-end`, in a format `rewind_trace::sdk`
reads. Only init's marks count as init's, so a program cannot pose as it.
`check` adds the `sdk` property by itself when schedule 0 writes SDK marks:
a run violates it where an `always` failed, an `unreachable` was reached, or
a test that passed in schedule 0 failed or never finished. A `sometimes`
fails no run; after the search `check` counts the runs that made each one
true and names each one no run made true. The
libraries in `sdk/` write each pass or failure once per id and process,
since every line is an exit.

Which thread held the CPU at a step is in the trace. The guest kernel
writes a switch record at every context switch, in `finish_task_switch`,
naming the task the CPU runs from then on: its pid and tid, whether it has
memory of its own, and its name. Writing the record is an exit, so every
context switch is a step, and `rewind threads` and the app's Threads tab
read who held the CPU over any window straight from the trace
(`rewind_trace::switches`), with nothing replayed. Switch records are not
events: they say nothing a program did, so `rewind events` does not list
them and comparisons of what two runs did leave them out, while `rewind
replay` compares them record for record like the rest.

The monitor does not store the guest's 36-byte record. A run switches at
most of its steps, so those would be most of a long trace. It encodes each
switch in two or three bytes, the step as the distance from the switch
before and the task as an index into a table of the tasks named so far,
and stores many in one record of kind 15: a batch is written at the first
step of the next window of 4096 steps, when it would exceed a record's
size, or at the run's end, stamped with the step it is written at, so the
trace stays in step order and a batch holds only switches at or before its
step. A machine restored at a keyframe resumes the encoder from the whole
trace at that step: the tables as the switches up to then left them, and
pending the switches up to then that the run wrote in a batch after it,
so a fork's batches come out as the run's did and a replay from a keyframe
compares them record for record. The PGO demo's trace, with its 63,000
switches, went from 2.3 MB as guest records to 145 KB encoded. The task
table is consulted, never copied, per switch: a copy per switch cost a
hello build under four schedules at once 17 times its wall time in the
static release, where every allocation takes musl's one malloc lock.

Before interface 5, switches were not recorded, and `rewind threads`
replayed the window one step at a time, reading `current_task` from guest
memory at each exit. That saw a switch only when an exit followed it, and
most switches made an exit only by accident: Linux 7.2 turns the
scheduler's high-resolution tick on by default, and `hrtick_schedule_exit`
reads the clock at the end of every switch once the hrtimer subsystem is in
high-resolution mode, which it enters at the first periodic tick, 10 ms of
virtual time after boot at HZ=100. So a program that ran inside the first
10 ms switched threads with no exit at all, and the same program later in
a build made an exit, and 5 µs of virtual time, at every switch. The guest
now turns the hrtick off (`kernel/sched/features.h`) and makes the exit
itself, at every switch from boot on, so `rewind run` of a program and the
same program inside `rewind nix` switch under the same rules, and `rewind
check` has a point to perturb at every handoff, on purpose rather than by a
timer default.

Three earlier designs did not work, and why is worth keeping.

- **Jittering every exit's time.** This found failures, but a single shift
  moved every timer after it. Narrowing then could not localize anything,
  and the reported divergence landed in the compiler.
- **Reschedule requests without a real scheduler clock.** These changed
  nothing. With no TSC, the scheduler's clock was jiffies, which barely moved
  in a short run. Every task seemed to have run for no time, so the scheduler
  never switched. The virtual scheduler clock fixed that.
- **Keeping only the perturbations a failure needs.** After narrowing,
  leaving out stretches of the window's steps and keeping what still failed,
  until every step left was needed, took six minutes on mylib instead of ten
  seconds and kept 55 steps. A perturbation is tied to a step number, so
  leaving one out moves every later one to other work in the program, and
  nearly every step turns out to be needed. The window's last step already
  gives two runs that differ by one perturbation, at no cost.

## Limits

- **CPU-bound threads are not preempted.** A thread gives up the CPU only at
  a step, so one that computes without system calls runs until it makes one.
  A thread spinning on a flag without yielding stalls the VM; futex-based
  waits are fine, and so is a loop of `sched_yield`, since each yield is an
  exit. Races that need preemption in the middle of pure computation are
  out of reach, unless the program marks the place with the SDK's
  `point()`, a write of nothing to `/dev/rewind` that makes one exit and
  records nothing. With exit time computation also takes no time; counter
  time ([pmu.md](pmu.md)) fixes that, not the preemption.
- **Go programs can hang in garbage collection.** At `GOMAXPROCS` above 1,
  the collector's thread registers as a sweeper and restarts the world; the
  restart makes an exit, and the vCPU goes to `bgsweep`, which loops on
  `isSweepDone` in user space, without system calls, and does not yield
  while it sees an idle P. The collector's thread never runs again to end
  its registration, so the run makes no more steps. `--cores N` makes N the
  default `GOMAXPROCS`, but `--cores` does not matter beyond that:
  `--cores 1` with `GOMAXPROCS=4` hangs the same way. Set `GOGC=off` when
  the program fits in `--mem`. `GOMAXPROCS=1` avoids the hang too, but runs every
  goroutine on one thread, where only a system call inside a goroutine, such
  as the SDK's `point()`, lets another one in; schedules alone did not
  change goroutine order in any Go test tried.
- **One vCPU.** Threads interleave but never run at the same instant, even
  with `--cores` reporting more CPUs. Throughput comes from running many
  machines at once: `check` runs one per core.
- **The CPU vendor is part of the input.** The fixed CPU model keeps the
  host's vendor, since Intel and AMD differ in ways CPUID cannot hide, so a
  run made on AMD replays on any AMD host from Zen 2 on but not on Intel,
  and the other way around. A `v3` run also needs a host that shows ERMS;
  a `v3-compat` run does not.
- **User-space RDRAND and RDSEED** still work on AMD if a program executes
  them without checking CPUID. KVM does not let the monitor trap them. Hardly
  any program does this; `rewind replay` finds the machine differs where one
  does, and `--bisect` names the instruction. On Intel, KVM is reported to
  fault them when CPUID hides them, so the program dies of SIGILL;
  `checks.determinism-sources` expects that there, and has not run on Intel
  yet.
- **Read and write records miss some writes.** Writes through a memory
  mapping, `splice` and io_uring are not recorded, and records that do not
  fit in the guest's 1 MiB buffer between two steps are lost. The trace
  says how many were lost at each step.
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

Agreeing with itself shows a counter is deterministic, not that it counts
the right branches. One that counts only taken branches, as Intel's umask
0x01 does on newer cores, agrees on every run of one machine, and a run
recorded there replays differently on a machine that counts them all.
Weaver, Terpstra and Moore call this overcount, as against nondeterminism,
and find it with assembly whose counts are known by construction
([Non-Determinism and Overcount on Modern Hardware Performance Counter
Implementations](https://web.eece.maine.edu/~vweaver/projects/deterministic/deterministic_counters.pdf),
ISPASS 2013). The self-test does the same with four probes in init: branches
taken forward, taken backward and not taken; a branch after a locked add;
instructions that start with 0x0F as conditional jumps do, which Nehalem
counted; and a branch after a system call, a page fault and a VM exit. Each
loop runs N and then 2N iterations, each run between two marks, and the
monitor reads the counter at every mark. All else between a run's marks is
the same both times, so the second run's count less the first's is the
loop's N extra iterations alone, and must equal their known count. On an AMD
family 25 laptop with the workaround set, all four match; with the
taken-branch event 0xc4 in place of 0xd1, all four miss. With the workaround
off, the locked probe still matched in 20 runs, while the threaded workload's
two runs disagreed in 1 of 10: the overcount needs contention, and even then
shows only sometimes, which is why AMD also needs the workaround known to be
set.

A hybrid Intel CPU has a PMU for each kind of core: `cpu_core` for its
performance cores, `cpu_atom` for its efficient ones, and on some parts
`cpu_lowpower`. A counter on a thread counts only while the thread is on a
core of its own PMU, so the vCPU's thread has one counter per kind of core,
and a read adds them up. The sum is exact wherever the host runs the thread
only if every kind of core counts the event alike, which the self-test checks
by running on each kind in turn. On a Core Ultra X7 358H and a Core Ultra X9
388H, both Panther Lake, every kind of core counts 40046477 branches of its
workload. Each
counter runs only while the thread is on its kind of core, so the check that
a counter ran the whole time it was enabled becomes a check that the
counters' running times add up to the time they were all enabled. No one
counter can overflow at the point their sum reaches, so each kind's overflow
is armed with a share of the branches to go: the kind the thread is on with
nearly all of them, the others with a few. Each counter has counted less than
its share until it overflows, so shares adding up to the branches to go plus
one less than the number of kinds make one of them overflow by then, wherever
the thread runs, and one that fires early, when the thread moved, is armed
again.

Intel's event for every retired conditional branch, `BR_INST_RETIRED.COND`,
is event 0xc4 with a umask that changed over the years and differs between
the kinds of core of one part: 0x01 up to Skylake, 0x11 on the performance
cores from Ice Lake to Meteor Lake, 0x111 on Lunar Lake's and Arrow Lake's,
0x7e on the efficient cores until Panther Lake, and 0x07 on both kinds of
Panther Lake's. On the newer parts umask 0x01 counts only some conditional
branches: taken ones, or on Panther Lake taken backward ones, which counted
15 times fewer of a branchy loop than umask 0x07. The two kinds of core of
an Alder Lake would also count different events under 0x01, and their sum
would not be exact. So the monitor takes each microarchitecture's umask,
per kind of core, from Linux's perf tables (`tools/perf/pmu-events/arch/x86`),
keyed by CPUID family and model, and lays it out in the config as each
PMU's sysfs format says: a 9-bit umask such as 0x111 puts its top bit at
config bit 40. A model the table does not list keeps 0x01, and the
self-test still decides whether runs use counter time.

The kinds of core count alike, but they do not run every instruction alike.
Where Intel's manual leaves a flag undefined, as SF, ZF, AF and PF after `mul`
and `imul`, OF after a shift or rotate by more than one, and AF after any
shift, a Core Ultra X7 358H's performance and efficient cores leave some of
them set differently. A correct program never reads such a flag, but the
flags reach memory whenever RFLAGS is saved: in R11 at a `syscall`, which the
guest kernel keeps in `pt_regs`, in an interrupt's frame, and in a signal
frame. A run whose vCPU moved between kinds of core can then differ from its
replay in one saved RFLAGS, which the keyframe check reports. No replay of
mylib or GNU hello across kinds of core has shown it. If one does, keeping
each run on one kind of core, chosen per run and recorded with it, removes it.

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
- The first 8 days of an evaluation show only "Evaluating · Buy" in the
  header. After that, the first fork that differs from its parent, closed
  shell or gdb pane, jump to the divergence or finished export of a day
  shows a reminder that closes itself after 20 seconds; from day 30 it
  mentions the commercial license.
- A personal license is $49, and a commercial license $99 per seat; both
  include three years of updates. A version released after a license's
  updates end runs as an evaluation.

A license key is a signed text block. The app checks it offline against an
Ed25519 public key built into it, and nothing is sent anywhere.
`crates/rewind-app/LICENSING.md` covers the key format, keeping the signing
key offline, and issuing keys.
