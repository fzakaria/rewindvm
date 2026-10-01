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
crw-rw-rw- 1 root kvm 10, 232 Oct  1 06:29 /dev/kvm
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
Rebuilt 45 times on the laptop, it failed twice, each time in `checkPhase`:

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
rewind: run 2085adea85fc0f1e exited:0 after 6188 steps, 0.216s virtual, 1.244s wall (poweroff)
/nix/store/f6a9gy362szw6nxx3ikrklr8glr6rdln-mylib-0.3.0 aa30ea54dc47c30f (same as the host's build)
```

The run used counter time: the VM's clock follows the work done inside it. On
AMD, until `sudo rewind pmu enable` has been run since boot, `rewind` instead
prints a warning that begins `rewind: recording with exit time: this AMD
CPU's branch counter is not exact until rr's workaround is set.` and records
with exit time. [Counter time](pmu.md) explains the difference.

This build passes, and it passes every time: the same inputs make the same
run, down to the same 6168 steps. A step is one exit from the VM to Rewind,
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
schedule   0: exited:0             6188 steps  aa30ea54dc47  run 2085adea85fc0f1e
schedule   1: exited:2             5766 steps    run b0e9743e983f513e
schedule   2: exited:0             7372 steps  aa30ea54dc47  run 02e6337029437d6a
schedule   3: exited:2             5598 steps    run 33f01fdb2994ad19
schedule   4: exited:0             7531 steps  aa30ea54dc47  run d5709ffb8201f5af
schedule   5: exited:2             5516 steps    run a84465fa4963ab82
schedule   6: exited:0             7333 steps  aa30ea54dc47  run b31321cd76ac526e
schedule   7: exited:0             7281 steps  aa30ea54dc47  run b5b5a1ff4250dffa
schedule   8: exited:2             5683 steps    run 22dedc7fa7a3cf92
schedule   9: exited:0             7250 steps  aa30ea54dc47  run e3cfe79a5a9317ce
schedule  10: exited:0             7324 steps  aa30ea54dc47  run e3ae6a871d83d690
schedule  11: exited:0             7437 steps  aa30ea54dc47  run 7e6cf91613cd2bb6
schedule  12: exited:0             7452 steps  aa30ea54dc47  run 0d1445124317d2de
schedule  13: exited:0             7276 steps  aa30ea54dc47  run 2a59e327039cda87
schedule  14: exited:0             7274 steps  aa30ea54dc47  run a6e551527fb191c4
schedule  15: exited:0             7332 steps  aa30ea54dc47  run 052961f7932bce8a
schedule  16: exited:0             7086 steps  aa30ea54dc47  run d4695c25b79fc9cd

schedule 1 ends differently; narrowing the steps it perturbs
perturbing only steps 2198..4570 still ends differently

passing: run 2085adea85fc0f1e
failing: run 9a96fab59759f2a5

where ./tests/test_pool_shutdown first behaves differently:
  both        4159   166/167   write(1, "worker picked job 2\n")
  both        4169   166/168   write(1, "job 1 done: 35269\n")
  both        4170   166/168   write(1, "worker picked job 3\n")
  left        4179   166/168   write(1, "job 3 done: 58758\n")
  left        4180   166/168   write(1, "worker picked job 4\n")
  left        4188   166/167   write(1, "job 2 done: 30213\n")
  left        4189   166/167   write(1, "worker picked job 5\n")
  right       4401   166/167   write(1, "job 2 done: 30213\n")
  right       4402   166/167   write(1, "worker picked job 4\n")
  right       4405   166/168   write(1, "job 3 done: 58758\n")
  right       4407   166/168   write(1, "worker picked job 5\n")
```

Schedules 1, 3, 5 and 8 fail. `check` then narrows schedule 1's perturbation
to the smallest window that still changes the outcome, here steps 2198 to 4570. It keeps two runs: the unperturbed one and the failing one with the
narrowed window. The two are identical up to step 2198.

The last block compares only the failing program's own events. In the failing
run the workers finish jobs 2 and 3 in the other order, and the interleaving
drifts from there until shutdown lands between a worker's check of the pool
and its count of the job.

The whole search took 12 seconds. `rewind check --all` tries every schedule
and says how many failed, which measures how flaky a build is:

```console
$ rewind check --all github:fzakaria/rewindvm#mylib | grep 'ended differently'
12 of 64 perturbed schedules ended differently
```

## Look at the failure

A failing run is kept like any other. It is a directory holding its inputs and
every event with its step.

```console
$ rewind log 9a96fab5 --steps | tail -4
      4569   166  job 16 done: 5986
      4573   166  job 17 done: 43360
      4590   162  /nix/store/...-bash-5.3p15/bin/bash: line 1:   166 Segmentation fault         ./$t
      4594   161  make: *** [Makefile:18: check] Error 1
```

The events around the crash show the kernel's own report, with the faulting
instruction:

```console
$ rewind events 9a96fab5 --from 4570 --to 4581
      4573   166/168   write(1, "job 17 done: 43360\n")
      4574     0/0     console "[    0.198653] test_pool_shutd[168]: segfault at 108 ip 000055a854bdd437 sp 00007f53a1b30e10 error 6 in test_pool_shutdown[1437,55a854bdd000+1000] likely on CPU 0 (core 0, socket 0)"
      4575     0/0     console "[    0.198659] Code: fa 48 8d 35 2a 0c 00 00 bf 02 00 00 00 b8 00 00 00 00 e8 ac fc ff ff 48 8b 05 b5 2b 00 00 48 8b 38 e8 8d fc ff ff 49 8b 46 58 <83> 80 08 01 00 00 01 ..."
      4576   166/168   SIGSEGV code=1 addr=0x108
      4578   166/166   SIGSEGV code=0 addr=0x0
      4581   166/168   thread exit(test_pool_shutd) killed:SIGSEGV
```

The instruction marked `<83> 80 08 01 00 00 01` is `addl $1, 0x108(%rax)`:
`p->queue->completed++` with `p->queue` null, 0x108 bytes into the queue.

The processes alive at the crash:

```console
$ rewind ps 9a96fab5 --at 4576
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
4576, here are the source the test was built from and a shell in the test
program's working directory:

```console
$ rewind cat 9a96fab5 4576 src/pool.c --pid 166 | sed -n '/^void pool_shutdown/,/^}/p'
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

$ printf 'pwd; ls; exit\n' | rewind shell 9a96fab5 4576 --pid 166
rewind: a shell at step 4576 of 9a96fab59759f2a5; exit it to leave
[rewind] /build/mylib # pwd; ls; exit
/build/mylib
Containerfile  Makefile  libmylib.a  src  tests
```

`--pid 166` resolves the path, and starts the shell, in the test program's
root and working directory. gdb sees the VM's CPU and its memory as the
running process maps it:

```console
$ rewind gdb 9a96fab5 4576 --listen 127.0.0.1:1234 &
rewind: gdb at step 4576 of 9a96fab59759f2a5; connect with: gdb -q /nix/store/...-rewind-guest-kernel-...-symbols/vmlinux -ex 'source /nix/store/...-rewind-guest-kernel-...-symbols/vmlinux-gdb.py' -ex 'target remote 127.0.0.1:1234'
$ gdb -q -batch /nix/store/...-rewind-guest-kernel-...-symbols/vmlinux -ex 'source /nix/store/...-rewind-guest-kernel-...-symbols/vmlinux-gdb.py' -ex 'target remote 127.0.0.1:1234' -ex 'bt 6' -ex 'x/i 0x55a854bdd437'
0xffffffff8128642c in __outl (value=<optimized out>, port=1504) at ./arch/x86/include/asm/shared/io.h:24
warning: 24	./arch/x86/include/asm/shared/io.h: No such file or directory
#0  0xffffffff8128642c in __outl (value=<optimized out>, port=1504) at ./arch/x86/include/asm/shared/io.h:24
#1  rewind_emit (kind=6, flags=0, pid=<optimized out>, tid=<optimized out>, aux=11, a=0xffffc900001ebe1c, alen=12, b=0x0, blen=0) at arch/x86/kernel/cpu/rewind.c:192
#2  rewind_probe_signal (data=<optimized out>, sig=11, info=<optimized out>, ka=<optimized out>) at arch/x86/kernel/cpu/rewind.c:727
#3  0xffffffff812c46db in __do_trace_signal_deliver (sig=11, info=<optimized out>, ka=<optimized out>) at ./include/trace/events/signal.h:96
#4  trace_signal_deliver (sig=11, info=<optimized out>, ka=<optimized out>) at ./include/trace/events/signal.h:96
#5  get_signal (ksig=ksig@entry=0xffffc900001ebea0) at kernel/signal.c:3018
   0x55a854bdd437:	addl   $0x1,0x108(%rax)
[Inferior 1 (process 1) detached]
```

The VM is stopped in the kernel, delivering the SIGSEGV to the test at the
point where Rewind reports it, and at the address from the kernel's segfault
report is the instruction that faulted. The kernel's symbols come from its
package's `symbols` output, which `rewind gdb` fetches from the binary cache
the first time. Without `--listen`, `rewind gdb` starts gdb itself.

## Replay it

A failing run fails the same way every time it runs:

```console
$ rewind replay 9a96fab5
identical: 1596 events over 4613 steps

$ rewind replay 9a96fab5 --from 3400
identical from the keyframe at step 1699 to the end (0.42s)
```

`rewind` keeps keyframes while a run executes: snapshots of the machine, with
memory stored once per distinct page. `--from` restores the keyframe at or
before a step and runs from there. A keyframe that did not reproduce the rest
of the run exactly would make that command say so.

## Fork it

A fork is a run that is its parent up to a step, then explores another
schedule from there:

```console
$ rewind fork 2085adea 2198 --schedule 1 --quiet
rewind: run 33ebf4cb07d2a7e7 exited:2 after 4617 steps, 0.199s virtual, 1.011s wall (poweroff)
rewind: the fork first differs from its parent at step 2218

$ rewind fork 2085adea 2198 --schedule 2 --quiet
rewind: run a617e0bbda7a89de exited:0 after 6943 steps, 0.225s virtual, 1.046s wall (poweroff)
rewind: the fork first differs from its parent at step 2205

$ rewind fork 2085adea 2198 --schedule 3 --quiet
rewind: run 9599dc1fd2091996 exited:0 after 7020 steps, 0.228s virtual, 1.076s wall (poweroff)
rewind: the fork first differs from its parent at step 2220

$ rewind fork 2085adea 2198 --schedule 4 --quiet
rewind: run bbfb4816078d0655 exited:2 after 4828 steps, 0.204s virtual, 1.019s wall (poweroff)
rewind: the fork first differs from its parent at step 2205
```

Forked from the passing build at step 2198, where `check`'s window starts,
schedules 1 and 4 crash and schedules 2 and 3 pass. So the bug can be reached
from that step by more than the one interleaving `check` found.

## Scrub it in the app

The desktop app, `github:fzakaria/rewindvm#app`, shows the same run on a
timeline. Drag the playhead to any
step to see the build log up to that step, the processes alive, the files
written, and the event at the step. Jump to the failure, jump to where the
failing run left the passing one, and fork from the playhead.

```console
$ rewind-app ~/.local/share/rewind/runs/276c5a7d91efda95 --compare ~/.local/share/rewind/runs/bf468504f5aaf10c
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

Before the fix, 12 of the same 64 schedules crashed.

## What to read next

- [The container tutorial](tutorial-container.md) does the same with a
  Docker image instead of a derivation.
- [Counter time](pmu.md) explains how the VM's clock follows its work, and
  the exit time warning on AMD.
- [Design](design.md) explains how the machine is made deterministic, and
  where that stops.
