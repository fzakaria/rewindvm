---
date: 2026-10-03
kind: design
revision: 9b340c53
---

# Preempting a guest that computes without exits

Not built. `--experimental-preempt` exists but is hidden, slow and
unreliable, and nothing here is on the site. Two designs have been tried.
The first, which is what the flag does, picks the branch count to preempt
at in advance and steps the guest to it; the measurements below show it
cannot be made both reliable and fast on AMD. The second records where
the interrupt landed on the first run and finds that place again on
replay, the way rr does. A prototype of it, 898 lines in
`crates/rewind-vmm` that were not kept, finished two programs that hang
today and cost about 1% on recording. On 2026-11-10 the decision
was to leave both unbuilt for now. This page records what was measured
and what the second design still needs, for whoever picks it up.

## The problem

A guest gets the CPU taken away only at a step: virtual time moves at
exits, so a thread that computes without system calls is never interrupted
(see Limits in [design.md](../design.md)). A thread that spins waiting for
another hangs the run. The case that started this is Go: at `GOMAXPROCS`
above 1, which `--cores` above 1 sets by default, its garbage collector's
`bgsweep` spins in user space on the sweep lock
and `isSweepDone`, waiting for a goroutine on another thread, and does not
yield while it sees an idle P. That thread is runnable but
never gets the one vCPU. A run stuck like this now ends with "timed out
computing without exits for 14.1s, in user space at 0x41b33e". The
workaround is `GOGC=off`, so `bgsweep` does not run. `GOMAXPROCS=1` avoids
the hang too, but leaves the goroutines on one thread, where schedules did
not change their order.

gdb on a hung run shows the order: the collector's thread registers as a
sweeper and restarts the world, the restart's futex wake is an exit that
hands the vCPU to `bgsweep`, and the collector's thread never runs again
to end its registration. In a study of 15 flaky Go tests, four needed
preemption to get anywhere. On `TestServerKeepAlivesEnabled/h2` with the
collector on, all 260 runs at `--cores 1` with `GOMAXPROCS=4` hung, and so
did all 65 tried at `--cores 4`.

Preemption would also reach races that need a thread switched out in the
middle of pure computation, which today are out of reach.

To fix it the monitor has to interrupt a computing thread. The
interruption has to come at the same instruction on every replay, or the
replay is a different run.

## What exists

With counter time, `Preemption::AtBranchCounts` (`crates/rewind-vmm/src/lib.rs`:
`aim_preemption`, `reached_preemption`, `start_stepping`) aims at the timer's
branch count. It arms the branch counter to overflow `PREEMPT_MARGIN` (256)
branches short of it, then single-steps the guest with KVM's guest debug
until the count is exactly the target, and delivers the timer there as a
step. Exactness matters: the point of delivery must be the same in every
replay.

## What measuring it found

On a Ryzen 7 7840U, with counter time. To time runs to the end at all, the
measurements used a mode that tolerated overruns instead of aborting.

| Workload   | No preemption | Margin 256 | 1024 | 2048 | 4096 |
| ---------- | ------------- | ---------- | ---- | ---- | ---- |
| busybox    | 0.31 s        | 1.0×       | 1.0× | 1.0× | 1.0× |
| mylib      | 0.69 s        | 3.2×       | 9.8× | 20×  | 48×  |
| hello      | 12.5 s        | 6.9×       | 23×  | 42×  | 82×  |
| gc-closure | 3.07 s        | 6.2×       | 18×  | 38×  | 68×  |

- **Stepping is per instruction, counting per branch.** A preemption took
  6 to 7 single steps per branch of margin, at 5 to 7 µs each; hello needs
  about 6,850 preemptions. busybox never computes long enough to need one.
- **Skid.** Over about 20,000 overflows: median about 30 branches, p99
  about 100, maximum 438; an earlier session saw 1609, and a later one on
  Zen 3 saw 3,177. rr treats Zen's skid as unbounded. AMD needs a margin
  of at least 2048.
- **Stepping gets lost, which is what actually breaks it.** Every Nix
  build aborted with "ran N past a preemption point", 428 to 675,924
  branches past, all while stepping and none at the overflow. A
  preemption aimed at a step already within the margin starts stepping in
  the kernel, at the exit itself, and the return to user mode (SYSRET or
  IRET) restores flags without TF. A stepped SYSCALL that blocks switches
  to another thread, which runs without TF.
- **TF leaks to user space.** Single-stepping a SYSCALL saves TF in R11,
  and SYSRET hands it back to the program, which then dies of SIGTRAP; Go
  did. A guest-kernel workaround in `exc_debug_user` clears TF and returns
  on a DR_STEP trap the task did not ask for.
- **The margin changes the run.** The same inputs at different margins
  give different step counts under the same run id, since the margin is
  not in the spec. With preemption the gc-closure build passes instead of
  failing.
- **An impossible overrun of 71,780,684,711** was stepping lost while Go
  spun with no overflow armed, until the run's time limit fired.
- **Go's own tests lose steps too.** `internal/trace`'s
  `TestTraceFutileWakeup/Stress` at Go 05dbbf91, with `GOMAXPROCS=4` and
  `GOGC=off`, passed in 1,029 steps; the same run with `-test.v` stopped at
  step 7,532 with "the branch counter ran 62995 past a preemption point
  ... its skid exceeds the margin of 256". 62,995 is over a hundred times
  the largest skid measured, so it is a lost step, which the message calls
  skid.

Delivering late cuts the count of preemptions. Preempting only a guest
still computing 10 ms (10M branches) after its timer was due, at margin
2048:

| Workload   | Time                | Preemptions |
| ---------- | ------------------- | ----------- |
| hello      | 2.4–2.9×            | 257         |
| mylib      | 1.3×                | 3           |
| gc-closure | 1.2×                | 8           |
| busybox    | 1.0×                | 0           |
| Go repro   | completes in 0.56 s | 1           |

hello needs 4116 preemptions at the deadline, 1228 at 1 ms late and 183 at
10 ms late.

## Why a count chosen in advance cannot work

The lost stepping and the TF leak above can be fixed in the guest kernel.
Two problems remain, and both come from choosing the count before the
interrupt arrives.

- **The margin has to be larger than the worst skid, and on AMD there is
  no worst.** If the interrupt arrives after the target has passed, the
  run cannot be made repeatable and has to stop. This repository has
  measured skids of 438, 1,609 and 3,177 branches on AMD. rr's table of
  CPUs gives every Zen generation a margin of 10,000 and marks it
  "essentially unbounded skid, i.e. the provided skid value is exceeded in
  rare cases". For Intel it gives 100 and 125.
- **Moving one instruction at a time is slow.** At 6 to 7 instructions
  per branch and 5 to 7 microseconds each, a margin of 10,000 branches is
  about 0.4 seconds for every preemption, and a build of GNU hello needs
  a few hundred even when delivered late.

## Intel

No Intel machine with `/dev/kvm` was available. From Linux 7.2 and rr's
sources:

- KVM's single-step sets guest RFLAGS.TF in shared code on VMX and SVM,
  and both report the trap as KVM_EXIT_DEBUG; SYSCALL saves RFLAGS in R11
  on both, so the TF leak is the same.
- rr's skid allowance is 100 up to Rocket Lake and 125 from Alder Lake, and
  the counter is exact in a guest with `exclude_host`.
- Rewind counted event `0x01c4` everywhere, which rr uses only from
  Nehalem to Comet Lake. From Ice Lake on, umask 0x01 counts taken
  conditional branches only, and on Panther Lake taken backward ones; rr
  uses umask 0x11 there, and 0x7e on E-cores. Rewind now takes the umask
  of `BR_INST_RETIRED.COND` for each microarchitecture and kind of core
  from Linux's perf tables, so it counts every conditional branch on each.

Inferred, not verified: stepping is lost the same way on Intel. Measured
on a Core Ultra: a counter on the P-cores' PMU counts nothing while the
vCPU thread is on an E-core, so the monitor opens one counter per kind of
core and adds them up (see design.md).

## Recording where the interrupt lands

Nothing requires the count to be chosen in advance. What is required is
that every replay preempts at the same place the first run did. This is
what rr does.

**In the first run,** ask for the interrupt when the timer is due and let
it arrive whenever it arrives. Preempt the guest right there. Then write
down where "there" is: the branch count, the address of the next
instruction, and the values of the registers. That place becomes a fact
about the run, as its seed is.

**In a replay,** the monitor has to stop at that recorded place. This is
the same task as before, stopping at an exact count, but the monitor now
knows the address. So it asks for the interrupt early by a margin, as
before. When the interrupt arrives it sets a hardware breakpoint at the
recorded address and lets the guest run at full speed. Each time the
guest reaches that address the breakpoint stops it and the monitor reads
the counter. If the count and the registers match what was written down,
this is the place. If they do not match, the guest carries on to the next
time it reaches the address.

What that does to each problem:

- The first run cannot overshoot, because it has no target.
- A replay can still overshoot, if the interrupt arrives later than the
  margin allowed. But the monitor can tell, because the count is already
  past the recorded one. A replay can then go back to its last keyframe
  and try again with a larger margin. A first run had no way to go back.
- The guest is never moved one instruction at a time, so the trap flag is
  never set.
- The cost of a preemption is the number of times the guest passes that
  one address inside the margin. For most code that is a handful.

The engine already has most of the parts. Reverse execution in gdb stops
at a branch count, uses hardware breakpoints, and retries with a larger
margin when it overshoots (`crates/rewind-core/src/debug/reverse.rs`).

A run is only preempted when a thread has computed for 10 milliseconds
of virtual time past a due timer without an exit, as in the late delivery
measured above. A run that never does has an empty list and is the run it
is today.

## What it changes about a run

Today a run is a function of its inputs. With this, a run that was
preempted is its inputs plus its list of recorded places. In the
prototype, two first runs of the same build agreed on 18 of 181 places
and differed on the rest.

The list is small, a few dozen bytes per preemption. There are two places
to keep it:

- **In the spec.** The spec already holds lists of this kind, such as the
  schedule windows a fork inherits. The run id is the hash of the spec,
  so it would cover the list, and "the same spec makes the same run"
  stays true.
- **Beside the trace,** as part of the recording. The prototype's author
  preferred this, on the grounds that the spec describes what was asked
  for and the list is something that happened.

Either way the id has to cover the list, and `replay`, `--bisect`,
export, import and forks all have to carry it. The spec is the better
place, because every command that makes a run again already starts from
one.

One thing changes for users. A failing schedule found in CI can no longer
be reproduced elsewhere from its schedule number alone if it was
preempted. It needs its spec or its exported file.

## What the prototype measured

The prototype is 898 lines in `crates/rewind-vmm`, mostly a new file
`landed.rs`, named for where the interrupt lands. It hangs off the hidden
`--experimental-preempt` flag and passes the list between runs in a file.

On the laptop (Zen 4), with a margin of 16,384 branches. "Replay" means
running the same inputs again with the recorded list.

| Workload                                    | Today     | First run | Replay | Preemptions |
| ------------------------------------------- | --------- | --------- | ------ | ----------- |
| A thread spinning until another sets a flag | times out | 0.30 s    | 0.45 s | 1           |
| A Go program that allocates, on 4 cores     | times out | 0.9 s     | 1.0 s  | 17 to 28    |
| Building GNU hello                          | 11.8 s    | 11.9 s    | 12.4 s | 181         |
| Building mylib                              | 0.67 s    | 0.73 s    | 0.67 s | 1           |
| Two threads that only compute               | 2.48 s    | 2.6 s     | 6.0 s  | 150         |

Does a replay reproduce the first run? For one recording of the hello
build:

- 20 of 20 replays in an empty home produced the same trace.
- 20 of 20 runs of `rewind replay` found the whole machine identical at
  all 39 keyframes.
- 39 of 39 replays that started from a keyframe were identical.

Two harder cases, with preemption made far more frequent than normal,
also matched every time: 986 preemptions in the hello build, and 3,011
in a compute loop.

The last row of the table is the expensive case. A tight loop passes the
same address on every iteration, so the breakpoint stops the guest
thousands of times per preemption, at about 7 microseconds each.

On leviathan (Zen 3, under load from other jobs):

- With the same margin of 16,384, 17 of 84 replays failed. In each one
  the interrupt arrived after the recorded place, by 17,175 to 49,660
  branches. Each failure was detected and reported with those numbers.
  Every replay that did finish was identical.
- With a margin of 131,072, 10 of 10 replays matched, and replay took
  28.8 to 32.9 seconds against 26.0 to 26.5 for the first run.

Zen 3's skid has a far longer tail than Zen 4's, and the cause is not
known. So no single margin is both safe and cheap there. That is what
rr's "unbounded" flag says too.

## What is left to build

1. **Retry after an overshoot.** Go back to the last keyframe and try
   again with a larger margin. This is what makes Zen 3 reliable. It also
   lets the first margin be small, 2,048 or 4,096, which cuts the number
   of breakpoint stops three to eight times.
2. **Storing the list,** in the spec as decided above. A replay that has
   no list must refuse. The prototype instead runs as a first run and
   reports a difference.
3. **Sharing the debug registers.** The prototype takes one of the CPU's
   four debug registers for its breakpoint. While it is set, a guest
   program that uses hardware watchpoints of its own would behave
   differently in a replay than in the first run. Reverse gdb is refused
   on these runs for now.
4. **Cheaper breakpoint stops.** Each one makes three system calls that
   can be avoided.
5. **Places a breakpoint cannot reach.** An interrupt can arrive in the
   middle of a repeated string instruction, where a breakpoint cannot
   stop. The prototype declines to preempt there and tries again a little
   later. None occurred in about 10,000 preemptions, so that path has
   only a unit test.
6. **Testing** on Intel, on hybrid Intel CPUs, with schedules and with
   forks. None of these were run.

One limit stays. A guest that computes inside its kernel without an exit
is still never preempted, because the counter counts only user code.

The prototype was built on the experimental mode's entry points in the
run loop. A separate change that removed that mode was not kept either,
so the mode is still in the tree; whoever builds this starts from
whatever the run loop looks like then.

## Before it ships

- **Speed:** at most about 2× on the Nix builds (hello, mylib, a case
  study) at the margin AMD needs, retries included.
- **Determinism:** hundreds of schedules across those workloads and the
  Go repro, with and without preemption, every one replaying identically
  from boot and from a keyframe; runs without preemption unchanged.
- **Intel:** the same on an Intel host, after the event encoding is
  settled.

Even then it should be opt-in, not the default. It changes what a run is,
so runs with and without it are different runs, and its cost falls on
exactly the CPU-bound builds people most often record.
