# Tutorial: a flaky Nix build

This tutorial takes a derivation whose test suite fails now and then, makes
Rewind VM find a failing run in under a minute, looks at the failure step by
step, and checks the fix across 64 thread interleavings.

The derivation is `mylib`, a small C thread pool in
[examples/mylib](https://github.com/fzakaria/rewindvm/tree/main/examples/mylib),
which the Rewind VM flake builds as `github:fzakaria/rewindvm#mylib`. Its
`pool_shutdown` frees the job queue before joining the workers, and a worker
that has finished a job checks whether the pool is stopping without holding
the lock, then counts the job through the queue. When shutdown runs between
that check and the count, the worker writes through a freed, nulled pointer.

Every transcript below is real output from `rewind` 0.1.0 on a 16 core AMD
laptop, with `sudo rewind pmu enable` run since boot.

## Install

You need x86_64 Linux with KVM and Nix with flakes enabled.

```console
$ nix profile install github:fzakaria/rewindvm
$ ls -l /dev/kvm
crw-rw-rw- 1 root kvm 10, 232 Sep 30 21:28 /dev/kvm
```

The build comes from `rewindvm.cachix.org`, so nothing compiles on your
machine. Nix asks once whether to trust that cache; say yes, or pass
`--accept-flake-config`. To try `rewind` without installing it, put
`nix run github:fzakaria/rewindvm --` in front of its arguments. The desktop
app is `github:fzakaria/rewindvm#app`. On NixOS, add the flake as an input and
turn on its module instead:

```nix
inputs.rewind.url = "github:fzakaria/rewindvm";

# in your configuration, with inputs.rewind.nixosModules.default imported
programs.rewind.enable = true;
programs.rewind.app.enable = true;
# AMD only: make the branch counter exact at every boot
programs.rewind.amdBranchCounterWorkaround = true;
```

If `/dev/kvm` is not readable and writable by you, add yourself to the `kvm`
group. On NixOS that is `users.users.<you>.extraGroups = [ "kvm" ];`.

Runs are kept under `~/.local/share/rewind`. Set `REWIND_HOME` to keep them
somewhere else.

## The flaky build

On the host, `nix build github:fzakaria/rewindvm#mylib` usually succeeds.
Rebuilt 45 times on the laptop, it failed 6 times, each in `checkPhase`:

```console
$ nix build --rebuild -L github:fzakaria/rewindvm#mylib
...
mylib> running tests/test_pool_shutdown
...
mylib> job 15 done: 42559
mylib> worker picked job 17
mylib> job 16 done: 5986
mylib> /nix/store/...-bash-5.3p15/bin/bash: line 1:   133 Segmentation fault         (core dumped) ./$t
mylib> make: *** [Makefile:18: check] Error 1
error: Cannot build '/nix/store/hvp2d0h9l97d19d3xp5k3vf6xwhg4axr-mylib-0.3.0.drv'.
```

Nix reports the derivation as failed and moves on. Running it again usually
passes, so there is nothing left to look at.

## Build it in Rewind VM

`rewind nix` builds a derivation the way the Nix sandbox would, inside a
deterministic virtual machine:

```console
$ rewind nix github:fzakaria/rewindvm#mylib
rewind: packing 62 store paths for mylib-0.3.0
...
rewind: run e0fe171d17e37832 exited:0 after 6171 steps, 0.216s virtual, 1.242s wall (poweroff)
/nix/store/f6a9gy362szw6nxx3ikrklr8glr6rdln-mylib-0.3.0 aa30ea54dc47c30f (same as the host's build)
```

The run used counter time: the VM's clock follows the work done inside it. On
AMD, until `sudo rewind pmu enable` has been run since boot, `rewind` instead
prints a warning that begins `rewind: recording with exit time: this AMD
CPU's branch counter is not exact until rr's workaround is set.` and records
with exit time. [Counter time](pmu.md) explains the difference.

This build passes, and it passes every time: the same inputs make the same
run, down to the same 6171 steps. A step is one exit from the VM to Rewind,
and the step count is the run's clock. The last line is the hash of the output
tree. The host built the same derivation above, so `rewind nix` compares the
two outputs, and they are the same.

## Find a failing interleaving

Determinism means the unperturbed build will never show the bug. `rewind
check` builds the derivation again under perturbed schedules. Each schedule
asks the VM's kernel to reschedule at different points and lets timers fire a
little late, as timer slack does on real hardware. It runs one machine per CPU
at a time and stops after the first batch in which a build ends differently:

```console
$ rewind check github:fzakaria/rewindvm#mylib
schedule   0: exited:0             6171 steps  aa30ea54dc47  run e0fe171d17e37832
schedule   1: exited:0             6660 steps  aa30ea54dc47  run 04d6775bc33b0c16
schedule   2: exited:0             6792 steps  aa30ea54dc47  run 4f3d53334802b728
schedule   3: exited:0             6737 steps  aa30ea54dc47  run 01287eafc2d4dad1
schedule   4: exited:0             6739 steps  aa30ea54dc47  run 358ba0fa8044b39a
schedule   5: exited:0             6758 steps  aa30ea54dc47  run 13d96f57e112ddc7
schedule   6: exited:0             6669 steps  aa30ea54dc47  run 75c7c19c946a9352
schedule   7: exited:0             6733 steps  aa30ea54dc47  run 5f175a376da9a875
schedule   8: exited:0             6790 steps  aa30ea54dc47  run a987e6f90e4c07ba
schedule   9: exited:0             6706 steps  aa30ea54dc47  run 9fc763e31b2fdee1
schedule  10: exited:0             6689 steps  aa30ea54dc47  run dc04a4ce2782a96d
schedule  11: exited:0             6641 steps  aa30ea54dc47  run 2329ade32a538645
schedule  12: exited:0             6675 steps  aa30ea54dc47  run 5991b7aeb25ff4ae
schedule  13: exited:0             6663 steps  aa30ea54dc47  run 8677ec45b8e57305
schedule  14: exited:0             6779 steps  aa30ea54dc47  run 5a59b50726dbe515
schedule  15: exited:0             6721 steps  aa30ea54dc47  run ee860b0308908483
schedule  16: exited:0             6718 steps  aa30ea54dc47  run ab50ec4ea5601d2a
schedule  17: exited:2             4669 steps    run a05817fbc3d5d1ff
schedule  18: exited:0             6675 steps  aa30ea54dc47  run 1fc0402de8844cd3
schedule  19: exited:0             6672 steps  aa30ea54dc47  run 29110d60375d4f2f
schedule  20: exited:0             6723 steps  aa30ea54dc47  run 9a07de301c872fcd
schedule  21: exited:0             6684 steps  aa30ea54dc47  run 7d625480abebeb2e
schedule  22: exited:0             6642 steps  aa30ea54dc47  run e3e5a80292d65d6e
schedule  23: exited:0             6787 steps  aa30ea54dc47  run c0ccc61383ad266a
schedule  24: exited:0             6774 steps  aa30ea54dc47  run ce446ba7cf26788f
schedule  25: exited:2             4889 steps    run 7526c71234fb6990
schedule  26: exited:0             6756 steps  aa30ea54dc47  run d5c628e6643761ba
schedule  27: exited:0             6720 steps  aa30ea54dc47  run 8e8b3e53cff0baf5
schedule  28: exited:2             5098 steps    run 9413768a8b9b4640
schedule  29: exited:0             6690 steps  aa30ea54dc47  run e261e589c37b0a5c
schedule  30: exited:0             6805 steps  aa30ea54dc47  run 23d8e678771391b7
schedule  31: exited:0             6623 steps  aa30ea54dc47  run 5610d2ec737b380d
schedule  32: exited:0             6755 steps  aa30ea54dc47  run 86869377e46af002

schedule 17 ends differently; narrowing the steps it perturbs
perturbing only steps 3378..4616 still ends differently

passing: run e0fe171d17e37832
failing: run 26b639442266d487

where ./tests/test_pool_shutdown first behaves differently:
  both        4176   165/167   write(1, "worker picked job 6\n")
  both        4186   165/166   write(1, "job 5 done: 45034\n")
  both        4187   165/166   write(1, "worker picked job 7\n")
  left        4194   165/167   write(1, "job 6 done: 13750\n")
  left        4195   165/167   write(1, "worker picked job 8\n")
  left        4205   165/166   write(1, "job 7 done: 5785\n")
  left        4206   165/166   write(1, "worker picked job 9\n")
  right       4256   165/166   write(1, "job 7 done: 5785\n")
  right       4257   165/166   write(1, "worker picked job 8\n")
  right       4261   165/167   write(1, "job 6 done: 13750\n")
  right       4268   165/167   write(1, "worker picked job 9\n")
```

The first 16 schedules all pass, so `check` runs a second batch, where
schedules 17, 25 and 28 fail. It then narrows schedule 17's perturbation to
the smallest window that still changes the outcome, here steps 3378 to 4616.
It keeps two runs: the unperturbed one and the failing one with the narrowed
window. The two are identical up to step 3378.

The last block compares only the failing program's own events. In the failing
run the two workers finish jobs 6 and 7 in the other order, and the
interleaving drifts from there until shutdown lands between a worker's check
of the pool and its count of the job.

The whole search took 15 seconds. `rewind check --all` tries every schedule
and says how many failed, which measures how flaky a build is:

```console
$ rewind check --all github:fzakaria/rewindvm#mylib | grep 'ended differently'
9 of 64 perturbed schedules ended differently
```

## Look at the failure

A failing run is kept like any other. It is a directory holding its inputs and
every event with its step.

```console
$ rewind log 26b63944 --steps | tail -4
      4372   165  worker picked job 17
      4383   165  job 17 done: 43360
      4402   161  /nix/store/...-bash-5.3p15/bin/bash: line 1:   165 Segmentation fault         ./$t
      4407   160  make: *** [Makefile:18: check] Error 1
```

The events around the crash show the kernel's own report, with the faulting
instruction:

```console
$ rewind events 26b63944 --from 4386 --to 4397
      4387   165/166   thread exit(test_pool_shutd) exited:0
      4390     0/0     console "[    0.195621] test_pool_shutd[167]: segfault at 108 ip 000055678ff30437 sp 00007f28446ebe10 error 6 in test_pool_shutdown[1437,55678ff30000+1000] likely on CPU 0 (core 0, socket 0)"
      4391     0/0     console "[    0.195626] Code: fa 48 8d 35 2a 0c 00 00 bf 02 00 00 00 b8 00 00 00 00 e8 ac fc ff ff 48 8b 05 b5 2b 00 00 48 8b 38 e8 8d fc ff ff 49 8b 46 58 <83> 80 08 01 00 00 01 ..."
      4392   165/167   SIGSEGV code=1 addr=0x108
      4394   165/165   SIGKILL code=0 addr=0x0
      4396   165/167   thread exit(test_pool_shutd) killed:SIGSEGV
```

The instruction marked `<83> 80 08 01 00 00 01` is `addl $1, 0x108(%rax)`:
`p->queue->completed++` with `p->queue` null, 0x108 bytes into the queue.

The processes alive at the crash:

```console
$ rewind ps 26b63944 --at 4392
     1 /init
    33   /nix/store/...-bash-5.3p15/bin/bash -e /nix/store/...-source-stdenv.sh /nix/store/...-default-builder.sh
   160     make SHELL=/nix/store/...-bash-5.3p15/bin/bash PREFIX=$(out) VERBOSE=y check
   161       /nix/store/...-bash-5.3p15/bin/bash -c for t in tests/test_pool_basic tests/test_pool_shutdown; do echo "running $t"; ./$t || exit 1; done
   165         ./tests/test_pool_shutdown
   167           (thread)
```

## Replay it

A failing run fails the same way every time it runs:

```console
$ rewind replay 26b63944
identical: 1591 events over 4430 steps

$ rewind replay 26b63944 --from 3200
identical from the keyframe at step 1536 to the end (0.48s)
```

`rewind` keeps keyframes while a run executes: snapshots of the machine, with
memory stored once per distinct page. `--from` restores the keyframe at or
before a step and runs from there. A keyframe that did not reproduce the rest
of the run exactly would make that command say so.

## Fork it

A fork is a run that is its parent up to a step, then explores another
schedule from there:

```console
$ rewind fork e0fe171d 3378 --schedule 1 --quiet
rewind: run ae009572d5329d89 exited:0 after 6525 steps, 0.219s virtual, 1.054s wall (poweroff)
rewind: the fork first differs from its parent at step 3400

$ rewind fork e0fe171d 3378 --schedule 2 --quiet
rewind: run 32deda14c04df897 exited:0 after 6532 steps, 0.219s virtual, 1.022s wall (poweroff)
rewind: the fork first differs from its parent at step 3399

$ rewind fork e0fe171d 3378 --schedule 3 --quiet
rewind: run 6e29fc52ed321b75 exited:0 after 6568 steps, 0.219s virtual, 0.987s wall (poweroff)
rewind: the fork first differs from its parent at step 3380

$ rewind fork e0fe171d 3378 --schedule 4 --quiet
rewind: run a2755a9c391d9cde exited:2 after 4897 steps, 0.200s virtual, 0.943s wall (poweroff)
rewind: the fork first differs from its parent at step 3380
```

Forked from the passing build at step 3378, where `check`'s window starts,
schedules 1 to 3 pass and schedule 4 crashes. So the bug can be reached from
that step by more than the one interleaving `check` found.

## Scrub it in the app

The desktop app, `github:fzakaria/rewindvm#app`, shows the same run on a
timeline. Drag the playhead to any
step to see the build log up to that step, the processes alive, the files
written, and the event at the step. Jump to the failure, jump to where the
failing run left the passing one, and fork from the playhead.

```console
$ rewind-app ~/.local/share/rewind/runs/26b639442266d487 --compare ~/.local/share/rewind/runs/e0fe171d17e37832
```

## Fix it and check the fix

Clone the repository, so `rewind` can check your edit of the flake's
`mylib`:

```console
$ git clone https://github.com/fzakaria/rewindvm
$ cd rewindvm
```

In `examples/mylib/src/pool.c`, count the finished job under the lock, and free the queue
only after the workers are joined:

```diff
 		long result = run_job(job);
+		pthread_mutex_lock(&p->lock);
 		if (!p->stopping) {
 			printf("job %d done: %ld\n", job, result & 0xffff);
 			fflush(stdout);
 			p->queue->completed++;
 		}
+		pthread_mutex_unlock(&p->lock);
 	}
 }
@@
-	/* The bug: the queue goes before the workers are joined. */
-	free(p->queue);
-	p->queue = NULL;
-
 	for (int i = 0; i < POOL_WORKERS; i++)
 		pthread_join(p->workers[i], NULL);
+	free(p->queue);
+	p->queue = NULL;
```

```console
$ rewind check --all .#mylib
...
0 of 64 perturbed schedules ended differently
same result under all 65 schedules
```

Before the fix, 9 of the same 64 schedules crashed.

## What to read next

- [The container tutorial](tutorial-container.md) does the same with a
  Docker image instead of a derivation.
- [Counter time](pmu.md) explains how the VM's clock follows its work, and
  the exit time warning on AMD.
- [Design](design.md) explains how the machine is made deterministic, and
  where that stops.
