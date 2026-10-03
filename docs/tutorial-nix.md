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

Every transcript below is real output from `rewind` on a 16 core AMD
laptop, with `sudo rewind pmu enable` run since boot.

## Install

You need x86_64 Linux with KVM and Nix with flakes enabled.

```console
$ nix profile install github:fzakaria/rewindvm
$ ls -l /dev/kvm
crw-rw-rw- 1 root kvm 10, 232 Oct  1 20:16 /dev/kvm
```

The build comes from `rewindvm.cachix.org`, so nothing compiles on your
machine. Nix asks once whether to trust that cache; say yes, or pass
`--accept-flake-config`. To try `rewind` without installing it, put
`nix run github:fzakaria/rewindvm --` in front of its arguments. The desktop
app is `github:fzakaria/rewindvm#app`. On NixOS, add the flake as an input and
turn on its module instead:

```nix
inputs.rewind.url = "github:fzakaria/rewindvm";

# in your configuration, with inputs.rewind.nixosModules.default imported;
# it also adds rewindvm.cachix.org to Nix's substituters
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
Rebuilt 45 times on the laptop, it failed twice, each time in `checkPhase`:

```console
$ nix build --rebuild -L github:fzakaria/rewindvm#mylib
...
mylib> running tests/test_pool_shutdown
...
mylib> job 16 done: 5986
mylib> worker picked job 18
mylib> job 17 done: 43360
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
rewind: run 3a7a903ba51d85aa exited:0 after 6162 steps, 0.216s virtual, 1.162s wall (poweroff)
/nix/store/f6a9gy362szw6nxx3ikrklr8glr6rdln-mylib-0.3.0 aa30ea54dc47c30f  matches your store
```

The run used counter time: the VM's clock follows the work done inside it. On
AMD, until `sudo rewind pmu enable` has been run since boot, `rewind` instead
prints a warning that begins `rewind: recording with exit time: this AMD
CPU's branch counter is not exact until rr's workaround is set.` and records
with exit time. [Counter time](pmu.md) explains the difference.

This build passes, and it passes every time: the same inputs make the same
run, down to the same 6162 steps. A step is one exit from the VM to Rewind,
and the step count is the run's clock. The last line is the output's NAR
hash, the hash Nix records for a store path, and the other builds of it that
have the same contents. The host built the same derivation above, so your
store has a copy, and it matches. `rewind nix` also asks the binary caches Nix
is configured with, which have no build of mylib.

## Find a failing interleaving

Determinism means the unperturbed build will never show the bug. `rewind
check` builds the derivation again under perturbed schedules. Each schedule
asks the VM's kernel to reschedule at different points and lets timers fire a
little late, as timer slack does on real hardware. It runs one machine per CPU
at a time and stops after the first batch in which a build ends differently:

```console
$ rewind check github:fzakaria/rewindvm#mylib
schedule   0: exited:0             6162 steps  aa30ea54dc47  run 3a7a903ba51d85aa
schedule   1: exited:0             7115 steps  aa30ea54dc47  run deaf00808dc50162
schedule   2: exited:0             7339 steps  aa30ea54dc47  run 0a7c28a6d574df14
schedule   3: exited:0             7277 steps  aa30ea54dc47  run bbc132f6e733e2c1
schedule   4: exited:2             5436 steps    run d2d679b756ff023c
schedule   5: exited:2             5519 steps    run 9f2ec754feab0a87
schedule   6: exited:0             7205 steps  aa30ea54dc47  run be6035cecbaf3c11
schedule   7: exited:0             7187 steps  aa30ea54dc47  run e52a9ab9c25ac76c
schedule   8: exited:0             7185 steps  aa30ea54dc47  run 0751ccb9c9b1c4e4
schedule   9: exited:0             7164 steps  aa30ea54dc47  run 0e380ea5f697a3ed
schedule  10: exited:0             7240 steps  aa30ea54dc47  run 0676fb0048ea0c4c
schedule  11: exited:0             7119 steps  aa30ea54dc47  run ca012e6928b6ed88
schedule  12: exited:0             7391 steps  aa30ea54dc47  run fa80f70a191fe2fe
schedule  13: exited:0             7314 steps  aa30ea54dc47  run 4168c21d25d0d748
schedule  14: exited:0             7348 steps  aa30ea54dc47  run 219f7485e103332b
schedule  15: exited:2             5093 steps    run 4cdf4a37725b8486
schedule  16: exited:2             5673 steps    run 049da1c7189dffa4

schedule 4 ends differently; narrowing the steps it perturbs
perturbing only steps 2713..4571 still ends differently

passing: run 3a7a903ba51d85aa
failing: run a0f799f19c9f85ed

where ./tests/test_pool_shutdown first behaves differently:
  both        4133   166/167   write(1, "worker picked job 0\n")
  both        4140   166/168   write(1, "worker picked job 1\n")
  both        4150   166/167   write(1, "job 0 done: 12727\n")
  left        4151   166/167   write(1, "worker picked job 2\n")
  left        4161   166/168   write(1, "job 1 done: 35269\n")
  left        4162   166/168   write(1, "worker picked job 3\n")
  left        4171   166/168   write(1, "job 3 done: 58758\n")
  right       4397   166/168   write(1, "job 1 done: 35269\n")
  right       4398   166/168   write(1, "worker picked job 2\n")
  right       4422   166/167   write(1, "worker picked job 3\n")
  right       4434   166/167   write(1, "job 3 done: 58758\n")
```

Schedules 4, 5, 15 and 16 fail. `check` then narrows schedule 4's perturbation
to the smallest window that still changes the outcome, here steps 2713 to 4571. It keeps two runs: the unperturbed one and the failing one with the
narrowed window. The two are identical up to step 2713.

The last block compares only the failing program's own events. In the failing
run the two workers take jobs 2 and 3 the other way round, and the
interleaving drifts from there until shutdown lands between a worker's check of the pool
and its count of the job.

The whole search took 12 seconds. `rewind check --all` tries every schedule
and says how many failed, which measures how flaky a build is:

```console
$ rewind check --all github:fzakaria/rewindvm#mylib | grep 'ended differently'
11 of 64 perturbed schedules ended differently
```

## Look at the failure

A failing run is kept like any other. It is a directory holding its inputs and
every event with its step.

```console
$ rewind log a0f799f1 --steps | tail -4
      4570   166  worker picked job 17
      4580   166  job 15 done: 42559
      4599   162  /nix/store/...-bash-5.3p15/bin/bash: line 1:   166 Segmentation fault         ./$t
      4603   161  make: *** [Makefile:18: check] Error 1
```

The events around the crash show the kernel's own report, with the faulting
instruction:

```console
$ rewind events a0f799f1 --from 4577 --to 4588
      4580   166/167   write(1, "job 15 done: 42559\n")
      4581     0/0     console "[    0.201759] test_pool_shutd[167]: segfault at 108 ip 0000564f0feaa437 sp 00007f2b7d41ae10 error 6 in test_pool_shutdown[1437,564f0feaa000+1000] likely on CPU 0 (core 0, socket 0)"
      4582     0/0     console "[    0.201765] Code: fa 48 8d 35 2a 0c 00 00 bf 02 00 00 00 b8 00 00 00 00 e8 ac fc ff ff 48 8b 05 b5 2b 00 00 48 8b 38 e8 8d fc ff ff 49 8b 46 58 <83> 80 08 01 00 00 01 ..."
      4583   166/167   SIGSEGV code=1 addr=0x108
      4585   166/166   SIGSEGV code=0 addr=0x0
```

The instruction marked `<83> 80 08 01 00 00 01` is `addl $1, 0x108(%rax)`:
`p->queue->completed++` with `p->queue` null, 0x108 bytes into the queue.

The processes alive at the crash:

```console
$ rewind ps a0f799f1 --at 4583
     1 /init
    34   /nix/store/...-bash-5.3p15/bin/bash -e /nix/store/...-source-stdenv.sh /nix/store/...-default-builder.sh
   161     make SHELL=/nix/store/...-bash-5.3p15/bin/bash PREFIX=$(out) VERBOSE=y check
   162       /nix/store/...-bash-5.3p15/bin/bash -c for t in tests/test_pool_basic tests/test_pool_shutdown; do echo "running $t"; ./$t || exit 1; done
   166         ./tests/test_pool_shutdown
   167           (thread)
   168           (thread)
```

## Look inside the VM

`rewind cat`, `rewind shell` and `rewind gdb` each work on a throwaway fork of
the run at a step, so nothing they do changes the run. At the SIGSEGV, step
4583, here are the source the test was built from and a shell in the test
program's working directory:

```console
$ rewind cat a0f799f1 4583 src/pool.c --pid 166 | sed -n '/^void pool_shutdown/,/^}/p'
void pool_shutdown(struct pool *p)
{
	pthread_mutex_lock(&p->lock);
	p->stopping = 1;
	pthread_cond_broadcast(&p->ready);
	pthread_mutex_unlock(&p->lock);

	/* The bug: the queue goes before the workers are joined. */
	free(p->queue);
	p->queue = NULL;

	for (int i = 0; i < POOL_WORKERS; i++)
		pthread_join(p->workers[i], NULL);
	pthread_cond_destroy(&p->ready);
	pthread_mutex_destroy(&p->lock);
	free(p);
}

$ printf 'pwd; ls; exit\n' | rewind shell a0f799f1 4583 --pid 166
rewind: a shell at step 4583 of a0f799f19c9f85ed; exit it to leave
[rewind] /build/mylib # pwd; ls; exit
/build/mylib
Containerfile  Makefile  libmylib.a  src  tests
```

`--pid 166` resolves the path, and starts the shell, in the test program's
root and working directory.

`rewind gdb` attaches gdb to a fork at a step, with symbols for the VM's kernel
and for the process that was running there: its program and libraries, loaded
where the process had them. The test program exists only inside the VM, so
Rewind copies it out, with the source files it was built from. Arguments after
`--` go to gdb. Step 4580 is thread 167 printing `job 15 done`, three lines
before the line that faulted. Continuing the fork to that line, once the queue
is gone, shows the crash in the test's own code:

```console
$ rewind gdb a0f799f1 4580 -- -batch -ex 'break src/pool.c:77 if p->queue == 0' -ex continue -ex 'bt 3' -ex 'p p->queue' -ex list
rewind: step 4580 ran in process 166; loading symbols for 4 of its files
rewind: fetched 3 source files from the VM
rewind: gdb at step 4580 of a0f799f19c9f85ed
0xffffffff81285085 in __outl (value=<optimized out>, port=1504) at ./arch/x86/include/asm/shared/io.h:24
24	BUILDIO(l,  , u32)
Breakpoint 1 at 0x564f0feaa433: file src/pool.c, line 77.

Breakpoint 1, worker (arg=0x564f3946e010) at src/pool.c:77
77				p->queue->completed++;
#0  worker (arg=0x564f3946e010) at src/pool.c:77
#1  0x00007f2b7d4be7d1 in start_thread (arg=<optimized out>) at pthread_create.c:454
#2  0x00007f2b7d54ab1c in __GI___clone3 () at ../sysdeps/unix/sysv/linux/x86_64/clone3.S:78
$1 = (struct queue *) 0x0
72			 * the bug. */
73			long result = run_job(job);
74			if (!p->stopping) {
75				printf("job %d done: %ld\n", job, result & 0xffff);
76				fflush(stdout);
77				p->queue->completed++;
78			}
79		}
80	}
81
[Inferior 1 (process 1) detached]
```

gdb starts where the step left the VM: in the kernel, reporting the thread's
write to Rewind. The breakpoint is one of the CPU's debug registers, so the
fork runs on unchanged until thread 167 reaches line 77 with `p->queue` null,
the access the kernel reported at 0x108. glibc's symbols and sources, and the
kernel's, come from a debuginfod server `rewind gdb` starts for the session.
The kernel's DWARF is in its package's `debug` output, which `rewind gdb`
fetches from `rewindvm.cachix.org` the first time; that needs the cache in
Nix's settings, which the NixOS module adds. Without `--`, gdb stays open for
you to type into.

## Replay it

A failing run fails the same way every time it runs:

```console
$ rewind replay a0f799f1
identical: 1595 events over 4623 steps

$ rewind replay a0f799f1 --from 3400
identical from the keyframe at step 1692 to the end (0.42s)
```

`rewind` keeps keyframes while a run executes: snapshots of the machine, with
memory stored once per distinct page. `--from` restores the keyframe at or
before a step and runs from there. A keyframe that did not reproduce the rest
of the run exactly would make that command say so.

The VM sees a fixed x86-64-v3 CPU model, so a run replays on other machines
with the same CPU vendor: one recorded on AMD replays on AMD from Zen 2 on,
and not on Intel.

## Fork it

A fork is a run that is its parent up to a step, then explores another
schedule from there:

```console
$ rewind fork 3a7a903b 2713 --schedule 5 --quiet
rewind: run 0723f6d984f5451d exited:0 after 6803 steps, 0.227s virtual, 0.505s wall (poweroff)
rewind: the fork first differs from its parent at step 2735

$ rewind fork 3a7a903b 2713 --schedule 6 --quiet
rewind: run aeb6f4941424124c exited:0 after 6804 steps, 0.227s virtual, 0.489s wall (poweroff)
rewind: the fork first differs from its parent at step 2717

$ rewind fork 3a7a903b 2713 --schedule 7 --quiet
rewind: run 59f65b48756d8a2d exited:2 after 4700 steps, 0.201s virtual, 0.420s wall (poweroff)
rewind: the fork first differs from its parent at step 2733

$ rewind fork 3a7a903b 2713 --schedule 8 --quiet
rewind: run bd0aeb2bae55e910 exited:2 after 4592 steps, 0.199s virtual, 0.415s wall (poweroff)
rewind: the fork first differs from its parent at step 2717
```

Forked from the passing build at step 2713, where `check`'s window starts,
schedules 7 and 8 crash and schedules 5 and 6 pass. So the bug can be reached
from that step by more than the one interleaving `check` found.

## Scrub it in the app

The desktop app, `github:fzakaria/rewindvm#app`, shows the same run on a
timeline. Drag the playhead to any
step to see the build log up to that step, the processes alive, the files
written, and the event at the step. Jump to the failure, jump to where the
failing run left the passing one, and fork from the playhead.

```console
$ rewind-app ~/.local/share/rewind/runs/a0f799f19c9f85ed --compare ~/.local/share/rewind/runs/3a7a903ba51d85aa
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

Before the fix, 11 of the same 64 schedules crashed.

## Limits worth knowing here

- The VM has one vCPU. Threads interleave, but never run at the same
  instant, so a data race between two plain loads and stores with no system
  call between them is out of reach. Races across a system call, a lock or a
  sleep, like this one, are in reach.
- With exit time, a thread that computes for a long time without a system
  call is not preempted, and a thread spinning on a flag without yielding
  stalls the VM. [Counter time](pmu.md) explains why.
- The build runs with no network, as in the Nix sandbox.

## What to read next

- [The container tutorial](tutorial-container.md) does the same with a
  Docker image instead of a derivation.
- [The advanced tutorial](tutorial-advanced.md) has short recipes for the
  rest: watchpoints, the kernel's side of the crash, tools inside the VM,
  more CPUs, and sharing a failing run.
- [Counter time](pmu.md) explains how the VM's clock follows its work, and
  the exit time warning on AMD.
- [Design](design.md) explains how the machine is made deterministic, and
  where that stops; [its list of limits](design.md#limits) is the full one.
