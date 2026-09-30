# Tutorial: a flaky Nix build

This tutorial takes a derivation whose test suite fails about one build in
nine, makes Rewind VM find a failing run in seconds, looks at the failure step
by step, and checks the fix across 64 thread interleavings.

The derivation is `mylib`, a small C thread pool in
[examples/mylib](../examples/mylib). Its `pool_shutdown` frees the job queue
before joining the workers, and a worker that has finished a job checks
whether the pool is stopping without holding the lock, then counts the job
through the queue. When shutdown runs between that check and the count, the
worker writes through a freed, nulled pointer.

Every transcript below is real output from `rewind` 0.1.0 on a 16 core AMD
laptop.

## Install

You need x86_64 Linux with KVM and Nix with flakes enabled.

```console
$ nix profile install github:fzakaria/rewind
$ ls -l /dev/kvm
crw-rw-rw- 1 root kvm 10, 232 Sep 28 09:40 /dev/kvm
```

If `/dev/kvm` is not readable and writable by you, add yourself to the `kvm`
group. On NixOS that is `users.users.<you>.extraGroups = [ "kvm" ];`.

Runs are kept under `~/.local/share/rewind`. Set `REWIND_HOME` to keep them
somewhere else.

## The flaky build

On the host, `nix build github:fzakaria/rewind#mylib` usually succeeds. About
one build in nine fails in `checkPhase`:

```console
running tests/test_pool_shutdown
/nix/store/...-bash-5.3p15/bin/bash: line 1:   174 Segmentation fault         ./$t
make: *** [Makefile:18: check] Error 1
```

Nix reports the derivation as failed and moves on. Running it again usually
passes, so there is nothing left to look at.

## Build it in Rewind VM

`rewind nix` builds a derivation the way the Nix sandbox would, inside a
deterministic virtual machine:

```console
$ rewind nix github:fzakaria/rewind#mylib
...
rewind: run efd74e9a5a5098f5 exited:0 after 5115 steps, 0.029s virtual, 3.032s wall (poweroff)
/nix/store/f6a9gy362szw6nxx3ikrklr8glr6rdln-mylib-0.3.0 aa30ea54dc47c30f (not built on the host)
```

This build passes, and it passes every time: the same inputs make the same
run, down to the same 5115 steps. A step is one exit from the guest to the
monitor, and the step count is the run's clock. The last line is the hash of
the output tree. When the host has built the same derivation, `rewind nix`
compares the two and says whether they are the same.

## Find a failing interleaving

Determinism means the unperturbed build will never show the bug. `rewind
check` builds the derivation again under perturbed schedules. Each schedule
asks the guest kernel to reschedule at different points and lets timers fire
a little late, as timer slack does on real hardware. It stops at the first
build that ends differently:

```console
$ rewind check github:fzakaria/rewind#mylib
schedule   0: exited:0             5115 steps  aa30ea54dc47  run efd74e9a5a5098f5
schedule   1: exited:2             4013 steps    run 2ae737e23ef6e78c

schedule 1 ends differently; narrowing the steps it perturbs
perturbing only steps 3779..3980 still ends differently

passing: run efd74e9a5a5098f5
failing: run 27586aa31b6292bf

where ./tests/test_pool_shutdown first behaves differently:
  both        3800   174/176   write(1, "worker picked job 16\n")
  both        3802   174/175   write(1, "job 14 done: 39906\n")
  both        3803   174/175   write(1, "worker picked job 17\n")
  left        3812   174/176   thread exit(test_pool_shutd) exited:0
  left        3822   174/175   thread exit(test_pool_shutd) exited:0
  left        3825   174/174   clone(CLONE_THREAD) = 177
  left        3828   174/174   clone(CLONE_THREAD) = 178
  right       3811   174/175   write(1, "job 17 done: 43360\n")
  right       3812   174/176   write(1, "job 16 done: 5986\n")
  right       3815   174/176   SIGSEGV code=1 addr=0x108
  right       3816   174/174   SIGKILL code=0 addr=0x0
```

The first perturbed schedule fails. `check` then narrows the perturbation to
the smallest window that still changes the outcome, here steps 3779 to 3980.
It keeps two runs: the unperturbed one and the failing one with the narrowed
window. The two are identical up to step 3779.

The last block compares only the failing program's own events. In the passing
run, both workers see the pool stopping and exit. In the failing run, both
workers report their last jobs after shutdown has started, and the second
report is followed by a SIGSEGV at address `0x108`.

The whole search took 47 seconds. `rewind check --all` tries every schedule
and says how many failed, which measures how flaky a build is:
roughly half of mylib's schedules fail.

## Look at the failure

A failing run is kept like any other. It is a directory holding its inputs and
every event with its step.

```console
$ rewind log 27586aa3 --steps | tail -4
      3811   174  job 17 done: 43360
      3812   174  job 16 done: 5986
      3829   170  /nix/store/...-bash-5.3p15/bin/bash: line 1:   174 Segmentation fault         ./$t
      3832   169  make: *** [Makefile:18: check] Error 1
```

The events around the crash show the kernel's own report, with the faulting
instruction:

```console
$ rewind events 27586aa3 --from 3809 --to 3820
      3811   174/175   write(1, "job 17 done: 43360\n")
      3812   174/176   write(1, "job 16 done: 5986\n")
      3813     0/0     console "[    0.020354] test_pool_shutd[176]: segfault at 108 ip 00005555b6ed5437 sp 00007fc82fb64e10 error 6 in test_pool_shutdown[1437,5555b6ed5000+1000] likely on CPU 0 (core 0, socket 0)"
      3814     0/0     console "[    0.020359] Code: fa 48 8d 35 2a 0c 00 00 bf 02 00 00 00 b8 00 00 00 00 e8 ac fc ff ff 48 8b 05 b5 2b 00 00 48 8b 38 e8 8d fc ff ff 49 8b 46 58 <83> 80 08 01 00 00 01 ..."
      3815   174/176   SIGSEGV code=1 addr=0x108
      3816   174/175   SIGKILL code=0 addr=0x0
      3817   174/174   SIGKILL code=0 addr=0x0
      3819   174/176   thread exit(test_pool_shutd) killed:SIGSEGV
```

The instruction marked `<83> 80 08 01 00 00 01` is `addl $1, 0x108(%rax)`:
`p->queue->completed++` with `p->queue` null, 0x108 bytes into the queue.

The processes alive at the crash:

```console
$ rewind ps 27586aa3 --at 3815
     1 /init
    41   /nix/store/...-bash-5.3p15/bin/bash -e /nix/store/...-source-stdenv.sh ...
   169     make SHELL=/nix/store/...-bash-5.3p15/bin/bash PREFIX=$(out)
   170       /nix/store/...-bash-5.3p15/bin/bash -c for t in tests/test_pool_basic tests/test_pool_shutdown; ...
   174         ./tests/test_pool_shutdown
   175           (thread)
   176           (thread)
```

## Replay it

A failing run fails the same way every time it runs:

```console
$ rewind replay 27586aa3
identical: 1608 events over 3848 steps

$ rewind replay 27586aa3 --from 3000
identical from the keyframe at step 1374 to the end (0.56s)
```

`rewind` keeps keyframes while a run executes: snapshots of the machine, with
memory stored once per distinct page. `--from` restores the keyframe at or
before a step and runs from there. A keyframe that did not reproduce the rest
of the run exactly would make that command say so.

## Fork it

A fork is a run that is its parent up to a step, then explores another
schedule from there:

```console
$ rewind fork efd74e9a 3779 --schedule 1 --quiet
rewind: run b318914bbf5b7c9a exited:2 after 3848 steps, 0.021s virtual, 1.058s wall (poweroff)
rewind: the fork first differs from its parent at step 3795

$ rewind fork efd74e9a 3779 --schedule 2 --quiet
rewind: run b0cbbec43d24c020 exited:2 after 4158 steps, 0.024s virtual, 1.014s wall (poweroff)
rewind: the fork first differs from its parent at step 3782
```

Both forks of the passing build at step 3779 crash. That says the bug is a
property of the code after step 3779, not an accident of anything earlier in
the build.

## Scrub it in the app

The desktop app shows the same run on a timeline. Drag the playhead to any
step to see the build log up to that step, the processes alive, the files
written, and the event at the step. Jump to the failure, jump to where the
failing run left the passing one, and fork from the playhead.

```console
$ rewind-app ~/.local/share/rewind/runs/27586aa31b6292bf --compare ~/.local/share/rewind/runs/efd74e9a5a5098f5
```

## Fix it and check the fix

Count the finished job under the lock, and free the queue only after the
workers are joined:

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

Before the fix, about half of the same 64 schedules crashed.

## What to read next

- [The container tutorial](tutorial-container.md) does the same with a
  Docker image instead of a derivation.
- [Design](design.md) explains how the machine is made deterministic, and
  where that stops.
