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
Running phase: unpackPhase
...
rewind: run f9d89f1535ecd781 exited:0 after 6174 steps, 0.216s virtual, 0.665s wall (poweroff)
/nix/store/f6a9gy362szw6nxx3ikrklr8glr6rdln-mylib-0.3.0 a9d703ba89774f3d  matches your store, rewindvm.cachix.org
```

It passes, in the same 6174 steps every time. `--epoch` fixes the
VM's clock at boot, one of the run's inputs, so your runs are the ones shown
here. The last line says the output matches the copy in your store.

## Find a failing interleaving

`rewind check` builds it again under perturbed schedules, which reschedule
the VM's threads at different points, and stops at the first batch in which a
build ends differently:

```console
$ rewind check --epoch 1790985600 github:fzakaria/rewindvm#mylib
schedule   0: exited:0             6174 steps  a9d703ba8977  run f9d89f1535ecd781
schedule   1: exited:2             5080 steps    run f5d81bb9cda9d7b7
schedule   2: exited:0             7120 steps  a9d703ba8977  run a0472a667cd3bc6d
schedule   3: exited:0             7377 steps  a9d703ba8977  run ef052c2fcba3a3ba
schedule   4: exited:0             7531 steps  a9d703ba8977  run 7de05b9392ac2559
schedule   5: exited:0             7113 steps  a9d703ba8977  run 47d0b11ac8cff55d
schedule   6: exited:2             5401 steps    run b8fef19d0aec5778
schedule   7: exited:0             7199 steps  a9d703ba8977  run 06f0fb3acb9cb513
schedule   8: exited:0             7215 steps  a9d703ba8977  run b04ded01f01ae32a
schedule   9: exited:0             7144 steps  a9d703ba8977  run 73cb4de5639f9164
schedule  10: exited:0             7324 steps  a9d703ba8977  run af9a7006cf1c7638
schedule  11: exited:2             5108 steps    run 0295cc74472836c0
schedule  12: exited:2             5101 steps    run eae2df7d47838d95
schedule  13: exited:0             7207 steps  a9d703ba8977  run f9ca2c7df6c38a76
schedule  14: exited:0             7291 steps  a9d703ba8977  run 3ac070dd2f6fd2c5
schedule  15: exited:2             4997 steps    run 759641c4c3c466f6
schedule  16: exited:0             7170 steps  a9d703ba8977  run 35a0b21ef846dcf5

schedule 1 ends differently; narrowing the steps it perturbs
perturbing only steps 4396..5048 still ends differently

passing: run f9d89f1535ecd781
failing: run 77cb8e22b8ce6d9d

where ./tests/test_pool_shutdown first behaves differently:
  both        4405   166/169   write(1, "worker picked job 7\n")
  both        4412   166/170   write(1, "job 6 done: 13750\n")
  both        4413   166/170   write(1, "worker picked job 8\n")
  left        4423   166/169   write(1, "job 7 done: 5785\n")
  left        4424   166/169   write(1, "worker picked job 9\n")
  left        4431   166/170   write(1, "job 8 done: 21929\n")
  left        4432   166/170   write(1, "worker picked job 10\n")
  right       4431   166/170   write(1, "job 8 done: 21929\n")
  right       4432   166/170   write(1, "worker picked job 9\n")
  right       4440   166/169   write(1, "job 7 done: 5785\n")
  right       4441   166/169   write(1, "worker picked job 10\n")
```

Schedules 1, 6, 11, 12 and 15 fail. `check` narrows schedule 1's
perturbation to steps 4396 to 5048 and keeps two runs,
the passing one and the failing one, identical up to step 4396.
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
$ rewind log 77cb8e22 --steps | tail -4
      5047   166  worker picked job 18
      5057   166  job 16 done: 5986
      5076   162  /nix/store/...-bash-5.3p15/bin/bash: line 1:   166 Segmentation fault         ./$t
      5080   161  make: *** [Makefile:18: check] Error 1

$ rewind events 77cb8e22 --from 5057 --to 5062
      5057   166/174   write(1, "job 16 done: 5986\n")
      5058     0/0     console "[    0.202375] test_pool_shutd[174]: segfault at 108 ip 000055bfaf437437 sp 00007f61584a9e10 error 6 in test_pool_shutdown[1437,55bfaf437000+1000] likely on CPU 0 (core 0, socket 0)"
      5059     0/0     console "[    0.202380] Code: fa 48 8d 35 2a 0c 00 00 bf 02 00 00 00 b8 00 00 00 00 e8 ac fc ff ff 48 8b 05 b5 2b 00 00 48 8b 38 e8 8d fc ff ff 49 8b 46 58 <83> 80 08 01 00 00 01 e9 b5 fe ff ff 55 48 89 e5 41 54 53 be 78 00"
      5060   166/174   SIGSEGV code=1 addr=0x108
      5062   166/166   SIGSEGV code=0 addr=0x0
```

The kernel's report marks the faulting instruction, `<83> 80 08 01 00 00 01`:
`addl $1, 0x108(%rax)`, which is `p->queue->completed++` with `p->queue`
null. The processes alive at the crash:

```console
$ rewind ps 77cb8e22 --at 5060
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
run at a step. At the SIGSEGV, step 5060:

```console
$ rewind cat 77cb8e22 5060 src/pool.c --pid 166 | sed -n '/^void pool_shutdown/,/^}/p'
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

$ printf 'pwd; ls; exit\n' | rewind shell 77cb8e22 5060 --pid 166
rewind: a shell at step 5060 of 77cb8e22b8ce6d9d; exit it to leave
[rewind] /build/mylib # pwd; ls; exit
/build/mylib
Containerfile  Makefile  libmylib.a  src  tests
```

`rewind gdb` loads the symbols of the VM's kernel and of the process running
at the step. From step 5057, thread 174's last write,
continue to the line that faulted:

```console
$ rewind gdb 77cb8e22 5057 -- -batch -ex 'break src/pool.c:77 if p->queue == 0' -ex continue -ex 'bt 3' -ex 'p p->queue' -ex list
rewind: step 5057 ran in process 166; loading symbols for 4 of its files
rewind: fetched 3 source files from the VM
rewind: gdb at step 5057 of 77cb8e22b8ce6d9d
Downloading 740.00 B source file /build/linux-7.2.8/./arch/x86/include/asm/shared/io.h...
0xffffffff81285085 in __outl (value=<optimized out>, port=1504) at ./arch/x86/include/asm/shared/io.h:24
24	BUILDIO(l,  , u32)
Breakpoint 1 at 0x55bfaf437433: file src/pool.c, line 77.

Thread 1 hit Breakpoint 1, worker (arg=0x55bfe41ff010) at src/pool.c:77
77				p->queue->completed++;
#0  worker (arg=0x55bfe41ff010) at src/pool.c:77
#1  0x00007f615854d7d1 in start_thread (arg=<optimized out>) at pthread_create.c:454
#2  0x00007f61585d9b1c in __GI___clone3 () at ../sysdeps/unix/sysv/linux/x86_64/clone3.S:78
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
$ rewind replay 77cb8e22
identical: 1715 events over 5102 steps

$ rewind replay 77cb8e22 --from 4396
identical from the keyframe at step 1294 to the end (0.42s)
```

`--from` starts at the nearest keyframe before the step. A run replays on any
machine with the same CPU vendor.

## Fork it

A fork is its parent up to a step, then another schedule:

```console
$ rewind fork f9d89f15 4396 --schedule 1 --quiet
rewind: run 12475319c317a26f exited:2 after 5105 steps, 0.203s virtual, 0.234s wall (poweroff)
rewind: the fork first differs from its parent at step 4405

$ rewind fork f9d89f15 4396 --schedule 2 --quiet
rewind: run 88ee14f47f4d1751 exited:0 after 6599 steps, 0.220s virtual, 0.284s wall (poweroff)
rewind: the fork first differs from its parent at step 4413

$ rewind fork f9d89f15 4396 --schedule 3 --quiet
rewind: run 97e60ff715e0dee2 exited:2 after 4571 steps, 0.197s virtual, 0.256s wall (poweroff)
rewind: the fork first differs from its parent at step 4413

$ rewind fork f9d89f15 4396 --schedule 4 --quiet
rewind: run 798cdeb6514f9ab1 exited:0 after 6557 steps, 0.221s virtual, 0.290s wall (poweroff)
rewind: the fork first differs from its parent at step 4405
```

From step 4396 of the passing build, schedules 1 and 3
crash and 2 and 4 pass.

## Scrub it in the app

The desktop app shows a run on a timeline: drag the playhead to any step for
the output, processes, files and event there, jump to the failure or to where
the run left the passing one, and fork from the playhead.

```console
$ rewind-app ~/.local/share/rewind/runs/77cb8e22b8ce6d9d --compare ~/.local/share/rewind/runs/f9d89f1535ecd781
```

Press f to jump to the failure at step 5060, then s to open the
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
