# Tutorial: a flaky Nix build

This tutorial takes a derivation whose tests fail now and then, has Rewind VM
find a failing run, looks at the crash, and checks the fix across 64 thread
interleavings.

The derivation is `mylib`, a small C thread pool in
[examples/mylib](https://github.com/fzakaria/rewindvm/tree/main/examples/mylib).
Its `pool_shutdown` frees the job queue before joining the workers, so a
worker still finishing a job can write through a freed, nulled pointer.

Every transcript below is real output from `rewind` on a 16 core AMD laptop.

## Install

You need x86_64 Linux with KVM and Nix with flakes enabled.

```console
$ nix profile install github:fzakaria/rewindvm
$ ls -l /dev/kvm
crw-rw-rw- 1 root kvm 10, 232 Oct  1 20:16 /dev/kvm
```

The build comes from `rewindvm.cachix.org`; say yes when Nix asks to trust
it. The desktop app is `github:fzakaria/rewindvm#app`. On NixOS, use the
module instead:

```nix
inputs.rewind.url = "github:fzakaria/rewindvm";

# with inputs.rewind.nixosModules.default imported
programs.rewind.enable = true;
programs.rewind.app.enable = true;
# AMD only: make the branch counter exact at every boot
programs.rewind.amdBranchCounterWorkaround = true;
```

If `/dev/kvm` is not yours to use, add yourself to the `kvm` group. On AMD,
run `sudo rewind pmu enable` once after each boot; [Counter time](pmu.md)
says why.

## The flaky build

On the host the build usually passes. Rebuilt 45 times, it failed twice:

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

Run it again and it usually passes, leaving nothing to look at.

## Build it in Rewind VM

`rewind nix` builds a derivation as the Nix sandbox would, inside a
deterministic virtual machine:

```console
$ rewind nix --epoch 1790985600 github:fzakaria/rewindvm#mylib
rewind: packing 62 store paths for mylib-0.3.0
...
rewind: run 6380bb57fcb451da exited:0 after 6169 steps, 0.216s virtual, 1.265s wall (poweroff)
/nix/store/f6a9gy362szw6nxx3ikrklr8glr6rdln-mylib-0.3.0 a9d703ba89774f3d  matches your store, rewindvm.cachix.org
```

It passes, in the same 6169 steps every time. `--epoch` fixes the
VM's clock at boot, one of the run's inputs, so your runs are the ones shown
here. The last line says the output matches the copy in your store.

## Find a failing interleaving

`rewind check` builds it again under perturbed schedules, which reschedule
the VM's threads at different points, and stops at the first batch in which a
build ends differently:

```console
$ rewind check --epoch 1790985600 github:fzakaria/rewindvm#mylib
schedule   0: exited:0       6169 steps  a9d703ba8977  run 6380bb57fcb451da
schedule   1: exited:0       7202 steps  a9d703ba8977  run e5003a7041880029
schedule   2: exited:0       7233 steps  a9d703ba8977  run 70416cb698a5d521
schedule   3: exited:0       7376 steps  a9d703ba8977  run 4c727264ea44d07a
schedule   4: exited:0       7320 steps  a9d703ba8977  run 0fe6d5ce0ac1e084
schedule   5: exited:0       7161 steps  a9d703ba8977  run 898d327455f8a618
schedule   6: exited:2       5468 steps    run 1506c6347419c2d2
schedule   7: exited:0       7118 steps  a9d703ba8977  run 37e34441e3db0566
schedule   8: exited:0       7244 steps  a9d703ba8977  run b26e2596cc549ef8
schedule   9: exited:0       7249 steps  a9d703ba8977  run 238747dddefeed87
schedule  10: exited:0       7254 steps  a9d703ba8977  run 2750fdaeb0554cc9
schedule  11: exited:0       7413 steps  a9d703ba8977  run e92ed815f16f1fdf
schedule  12: exited:0       7340 steps  a9d703ba8977  run c908407abdb9d2ec
schedule  13: exited:0       7228 steps  a9d703ba8977  run 1f63eee938f3660a
schedule  14: exited:0       7355 steps  a9d703ba8977  run 3670bbbf0b5c0136
schedule  15: exited:0       7230 steps  a9d703ba8977  run a3959bfb556d268c
schedule  16: exited:0       7170 steps  a9d703ba8977  run 8733165044c0fe35

schedule 6 ends differently; narrowing the steps it perturbs
perturbing only steps 3629..5140 still ends differently

passing: run 6380bb57fcb451da
failing: run 5c910df9774b38f2

where ./tests/test_pool_shutdown first behaves differently:
  both           4153   166/166   clone(CLONE_THREAD) = 168
  both           4160   166/167   write(1, "worker picked job 0\n")
  both           4167   166/168   write(1, "worker picked job 1\n")
  failing        4179   166/168   write(1, "job 1 done: 35269\n")
  failing        4180   166/168   write(1, "worker picked job 2\n")
  failing        4187   166/167   write(1, "job 0 done: 12727\n")
  failing        4188   166/167   write(1, "worker picked job 3\n")
  passing        4145   166/167   write(1, "job 0 done: 12727\n")
  passing        4146   166/167   write(1, "worker picked job 2\n")
  passing        4156   166/168   write(1, "job 1 done: 35269\n")
  passing        4157   166/168   write(1, "worker picked job 3\n")

open both in the desktop app: rewind open 5c910df9774b38f2 4179 --compare 6380bb57fcb451da
```

Schedule 6 fails. `check` narrows schedule 6's
perturbation to steps 3629 to 5140 and keeps two runs,
the passing one and the failing one, identical up to step 3629.
The last block shows where the test's own output first differs. The search
took 10 seconds.

`--all` tries every schedule, which measures how flaky a build is:

```console
$ rewind check --all --epoch 1790985600 github:fzakaria/rewindvm#mylib | grep 'ended differently'
10 of 64 perturbed schedules ended differently
```

## Look at the failure

The failing run keeps every event with its step:

```console
$ rewind log 5c910df9 --steps | tail -4
      5143   166  worker picked job 17
      5150   166  job 15 done: 42559
      5170   162  /nix/store/...-bash-5.3p15/bin/bash: line 1:   166 Segmentation fault         ./$t
      5174   161  make: *** [Makefile:18: check] Error 1

$ rewind events 5c910df9 --from 5150 --to 5155
      5150   166/174   write(1, "job 15 done: 42559\n")
      5151     0/0     console "[    0.202816] test_pool_shutd[174]: segfault at 108 ip 00005556d620d437 sp 00007feee6e68e10 error 6 in test_pool_shutdown[1437,5556d620d000+1000] likely on CPU 0 (core 0, socket 0)"
      5152     0/0     console "[    0.202821] Code: fa 48 8d 35 2a 0c 00 00 bf 02 00 00 00 b8 00 00 00 00 e8 ac fc ff ff 48 8b 05 b5 2b 00 00 48 8b 38 e8 8d fc ff ff 49 8b 46 58 <83> 80 08 01 00 00 01 e9 b5 fe ff ff 55 48 89 e5 41 54 53 be 78 00"
      5153   166/174   SIGSEGV code=1 addr=0x108
      5155   166/166   SIGSEGV code=0 addr=0x0
```

The kernel's report marks the faulting instruction, `<83> 80 08 01 00 00 01`:
`addl $1, 0x108(%rax)`, which is `p->queue->completed++` with `p->queue`
null. The processes alive at the crash:

```console
$ rewind ps 5c910df9 5153
     1 /init
    34   bash -e /nix/store/...-source-stdenv.sh /nix/store/...-default-builder.sh
   161     make SHELL=/nix/store/...-bash-5.3p15/bin/bash PREFIX=$(out) VERBOSE=y check
   162       /nix/store/...-bash-5.3p15/bin/bash -c for t in tests/test_pool_basic tests/test_pool_shutdown; do echo "running $t"; ./$t || exit 1; done
   166         ./tests/test_pool_shutdown
   173           (thread)
   174           (thread)
```

## Look inside the VM

`rewind cat`, `rewind shell` and `rewind gdb` work on a throwaway fork of the
run at a step. At the SIGSEGV, step 5153:

```console
$ rewind cat 5c910df9 5153 src/pool.c --pid 166 | sed -n '/^void pool_shutdown/,/^}/p'
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

$ printf 'pwd; ls; exit\n' | rewind shell 5c910df9 5153 --pid 166
rewind: a shell at step 5153 of 5c910df9774b38f2; exit it to leave
[rewind] /build/mylib # pwd; ls; exit
/build/mylib
Containerfile  Makefile  libmylib.a  src  tests
```

`rewind gdb` loads the symbols of the VM's kernel and of the process running
at the step. From step 5150, thread 174's last write,
continue to the line that faulted:

```console
$ rewind gdb 5c910df9 5150 -- -batch -ex 'break src/pool.c:77 if p->queue == 0' -ex continue -ex 'bt 3' -ex 'p p->queue' -ex list
rewind: step 5150 ran in process 166; loading symbols for 4 of its files
rewind: fetched 3 source files from the VM
rewind: gdb at step 5150 of 5c910df9774b38f2
Downloading 4.35 M separate debug info for /nix/store/h4wfwic161kxrr74jlzla5lsm28hgary-glibc-2.44-25/lib/libc.so.6...
Downloading 10.70 K separate debug info for /nix/store/h4wfwic161kxrr74jlzla5lsm28hgary-glibc-2.44-25/lib/libpthread.so.0...
Downloading 658.16 K separate debug info for /nix/store/h4wfwic161kxrr74jlzla5lsm28hgary-glibc-2.44-25/lib/ld-linux-x86-64.so.2...
Downloading 3.12 K source file /build/linux-7.2.8/./arch/x86/include/asm/irqflags.h...
arch_local_irq_restore (flags=518) at ./arch/x86/include/asm/irqflags.h:146
146		return !(flags & X86_EFLAGS_IF);
[Switching to thread 4 (Thread 1.174)]
#0  __syscall_cancel_arch () at ../sysdeps/unix/sysv/linux/x86_64/syscall_cancel.S:56
warning: 56	../sysdeps/unix/sysv/linux/x86_64/syscall_cancel.S: No such file or directory
Breakpoint 1 at 0x5556d620d433: file src/pool.c, line 77.

Thread 4 hit Breakpoint 1, worker (arg=0x5556f833c010) at src/pool.c:77
77				p->queue->completed++;
#0  worker (arg=0x5556f833c010) at src/pool.c:77
#1  0x00007feee6f0c7d1 in start_thread (arg=<optimized out>) at pthread_create.c:454
#2  0x00007feee6f98b1c in __GI___clone3 () at ../sysdeps/unix/sysv/linux/x86_64/clone3.S:78
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

The breakpoint is one of the CPU's debug registers, so the fork runs unchanged
until line 77 with `p->queue` null. Without `--`, gdb stays open for you to
type into.

## Replay it

```console
$ rewind replay 5c910df9
identical: 1713 events over 5192 steps

$ rewind replay 5c910df9 --from 3629
identical from the keyframe at step 1659 to the end (0.46s)
```

`--from` starts at the nearest keyframe before the step. A run replays on any
machine with the same CPU vendor.

## Fork it

A fork is its parent up to a step, then another schedule:

```console
$ rewind fork 6380bb57 3629 --schedule 5 --quiet
rewind: run 405c9c6df777a799 exited:0 after 6628 steps, 0.222s virtual, 0.377s wall (poweroff)
rewind: the fork first differs from its parent at step 3638
rewind: open it beside its parent in the desktop app: rewind open 405c9c6df777a799 3638 --compare 6380bb57fcb451da

$ rewind fork 6380bb57 3629 --schedule 6 --quiet
rewind: run 8f024e4b0e651026 exited:2 after 5201 steps, 0.203s virtual, 0.334s wall (poweroff)
rewind: the fork first differs from its parent at step 3638
rewind: open it beside its parent in the desktop app: rewind open 8f024e4b0e651026 3638 --compare 6380bb57fcb451da

$ rewind fork 6380bb57 3629 --schedule 7 --quiet
rewind: run 4e5abadaa24c7bc5 exited:0 after 6634 steps, 0.222s virtual, 0.371s wall (poweroff)
rewind: the fork first differs from its parent at step 3662
rewind: open it beside its parent in the desktop app: rewind open 4e5abadaa24c7bc5 3662 --compare 6380bb57fcb451da

$ rewind fork 6380bb57 3629 --schedule 8 --quiet
rewind: run 360e80c8b6a8c065 exited:2 after 4544 steps, 0.200s virtual, 0.317s wall (poweroff)
rewind: the fork first differs from its parent at step 3638
rewind: open it beside its parent in the desktop app: rewind open 360e80c8b6a8c065 3638 --compare 6380bb57fcb451da
```

From step 3629 of the passing build, schedules 6 and 8
crash and 5 and 7 pass.

## Scrub it in the app

The desktop app shows a run on a timeline: drag the playhead to any step for
the output, processes, files and event there, jump to the failure or to where
the run left the passing one, and fork from the playhead.

```console
$ rewind-app ~/.local/share/rewind/runs/5c910df9774b38f2 --compare ~/.local/share/rewind/runs/6380bb57fcb451da
```

Press f to jump to the failure at step 5153, then s to open the
source panel. After a few seconds it shows `worker` at `src/pool.c:77`, with
`p->queue->completed++;` marked: the line that read the queue after
`pool_shutdown` had set it to NULL.

## Fix it and check the fix

Clone the repository to edit the flake's `mylib`:

```console
$ git clone https://github.com/fzakaria/rewindvm
Cloning into 'rewindvm'...

$ cd rewindvm
```

In `examples/mylib/src/pool.c`, count the job under the lock, and free the
queue after the workers are joined:

```diff
@@ -71,11 +71,13 @@
 		 * stopping. The check and the count are not under the lock:
 		 * the bug. */
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

@@ -120,12 +122,10 @@
 	pthread_cond_broadcast(&p->ready);
 	pthread_mutex_unlock(&p->lock);

-	/* The bug: the queue goes before the workers are joined. */
-	free(p->queue);
-	p->queue = NULL;
-
 	for (int i = 0; i < POOL_WORKERS; i++)
 		pthread_join(p->workers[i], NULL);
+	free(p->queue);
+	p->queue = NULL;
 	pthread_cond_destroy(&p->ready);
 	pthread_mutex_destroy(&p->lock);
 	free(p);
```

```console
$ rewind check --all --epoch 1790985600 .#mylib
...
0 of 64 perturbed schedules ended differently
same result under all 65 schedules
```

Before the fix, 10 of the same 64 schedules crashed.

## Limits

The VM has one vCPU, so threads interleave but never run at the same instant:
a race between plain loads and stores with no system call between them is out
of reach, while races across a system call, a lock or a sleep, like this one,
are in reach. [Design](design.md#limits) has the full list.

## What to read next

- [The container tutorial](tutorial-container.md): the same bug from a Docker
  image, with no Nix.
- [The advanced tutorial](tutorial-advanced.md): watchpoints, the kernel's
  side of a crash, tools inside the VM, more CPUs, and sharing a run.
- [Counter time](pmu.md): how the VM's clock follows its work.
