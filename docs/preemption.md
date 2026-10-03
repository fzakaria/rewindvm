# TODO: preempting a guest that computes without exits

Not built. `--experimental-preempt` exists but is hidden, slow and
unreliable, and nothing here is on the site. This page records what was
measured and the design that the measurements point to, for whoever picks
it up.

## The problem

A guest gets the CPU taken away only at a step: virtual time moves at
exits, so a thread that computes without system calls is never interrupted
(see Limits in [design.md](design.md)). A thread that spins waiting for
another hangs the run. The case that started this is Go: at `--cores` above
1, its garbage collector's `bgsweep` spins in user space on the sweep lock
and `isSweepDone`, waiting for a goroutine on another thread, and does not
yield while it sees an idle P. That thread is runnable but
never gets the one vCPU. A run stuck like this now ends with "timed out
computing without exits for 14.1s, in user space at 0x41b33e"; the
workaround is `GOMAXPROCS=1` or `--cores 1`.

Preemption would also reach races that need a thread switched out in the
middle of pure computation, which today are out of reach.

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
  about 100, maximum 438; an earlier session saw 1609. rr treats Zen's
  skid as unbounded. AMD needs a margin of at least 2048.
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

## Intel

No Intel machine with `/dev/kvm` was available. From Linux 7.2 and rr's
sources:

- KVM's single-step sets guest RFLAGS.TF in shared code on VMX and SVM,
  and both report the trap as KVM_EXIT_DEBUG; SYSCALL saves RFLAGS in R11
  on both, so the TF leak is the same.
- rr's skid allowance is 100 up to Rocket Lake and 125 from Alder Lake, and
  the counter is exact in a guest with `exclude_host`.
- Rewind counts event `0x01c4`, which rr uses only from Nehalem to Comet
  Lake. From Ice Lake on, umask 0x01 counts taken conditional branches
  only; rr uses umask 0x11 there, and 0x7e on E-cores. It touches counter
  time in general as well as preemption, and should be checked on an Intel
  host on its own.

Inferred, not verified: stepping is lost the same way on Intel, and on a
hybrid part the event counts nothing if the vCPU thread moves to an E-core.

## Design

1. **Deliver late.** Preempt only a guest still computing a fixed time
   after its timer was due, 10 ms as a start. A run that exits often then
   comes out identical to one without preemption, and preemptions drop 16
   to 170 times.
2. **Step only user code, and keep stepping across returns.** A small
   guest-kernel hook in `arch_exit_to_user_mode_prepare` sets TF on every
   return to user mode while a flag in the shared page says the monitor is
   stepping, and clears it otherwise. While the guest is in the kernel the
   monitor does not step it: it waits for the return with
   `KVM_GUESTDBG_USE_HW_BP` set. This removes both ways stepping is lost,
   and with it the reason TF reaches R11. Keep the `exc_debug_user`
   workaround as a backstop.
3. **Put the margin and the lateness in the run spec,** as plain fields,
   so a replay uses what was recorded. Default by vendor: at least 2048 on
   AMD, with rr's figures for Intel.
4. **Make each step cheaper.** Read the counter with `rdpmc` through the
   perf event's mmap page instead of a system call per step.
5. **Fail clearly.** Arm a safety overflow while stepping, and check the
   time limit before the overrun check, so a lost step is an error that
   says so instead of an impossible count.

## Before it ships

- **Speed:** at most about 2× on the Nix builds (hello, mylib, a case
  study) at the margin AMD needs.
- **Determinism:** hundreds of schedules across those workloads and the
  Go repro, with and without preemption, every one replaying identically
  from boot and from a keyframe; runs without preemption unchanged.
- **Intel:** the same on an Intel host, after the event encoding is
  settled.

Even then it should be opt-in, not the default. It changes what a run is,
so runs with and without it are different runs, and its cost falls on
exactly the CPU-bound builds people most often record.
