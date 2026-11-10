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
rewind: run 0a1dae14daee9820 exited:0 after 6030 steps, 0.215s virtual, 0.780s wall (poweroff)
/nix/store/f6a9gy362szw6nxx3ikrklr8glr6rdln-mylib-0.3.0 a9d703ba89774f3d  matches your store, rewindvm.cachix.org
```

It passes, in the same 6030 steps every time. `--epoch` fixes the
VM's clock at boot, one of the run's inputs, so your runs are the ones shown
here. The last line says the output matches the copy in your store.

## Find a failing interleaving

`rewind check` builds it again under perturbed schedules, which reschedule
and stall the VM's threads at different points, and stops at the first batch
in which a build ends differently:

```console
$ rewind check --epoch 1790985600 github:fzakaria/rewindvm#mylib
schedule   0: exited:0             6030 steps  a9d703ba8977  run 0a1dae14daee9820
schedule   1: exited:0             7773 steps  a9d703ba8977  run eb388bf93ef00f21
schedule   2: exited:0             7663 steps  a9d703ba8977  run 00555e83e6bab5fb
schedule   3: exited:0             7930 steps  a9d703ba8977  run 6d888ae3d6e366b3
schedule   4: exited:2             6285 steps    run c63ac15c205593ff
schedule   5: exited:0             7610 steps  a9d703ba8977  run ba01494aa81fff96
schedule   6: exited:0             7496 steps  a9d703ba8977  run 37378a3c2677e60d
schedule   7: exited:0             7487 steps  a9d703ba8977  run 2a132209bc749aa6
schedule   8: exited:0             7632 steps  a9d703ba8977  run ae812cedbe8b508a
schedule   9: exited:2             5626 steps    run b28d0974a8ba0dea
schedule  10: exited:0             7581 steps  a9d703ba8977  run 469ec339402db31e
schedule  11: exited:0             7740 steps  a9d703ba8977  run 9a68c323065d3d1a
schedule  12: exited:0             7706 steps  a9d703ba8977  run d1ba6c7da93e4c1e
schedule  13: exited:2             5849 steps    run 3b3bf41704b8e8fc
schedule  14: exited:2             5728 steps    run 19d4977c9ae7c0ca
schedule  15: exited:0             7691 steps  a9d703ba8977  run ae457c867fd7a330
schedule  16: exited:2             5926 steps    run 00b3be8ad26202c1

schedule 4 ends differently from schedule 0; finding the step that decides it
schedule 4 limited to steps 4246..4826 still ends differently
step 4825 decides it: a request to switch threads there makes the run fail

passing: run 9ceb6f28d5ffd7ca, schedule 4 over steps 4246..4825
failing: run aa6759a6362f43a2, schedule 4 over steps 4246..4826
the two are the same run until step 4825

where ./tests/test_pool_shutdown first behaves differently:
  both           4813   166/172   write(1, "job 16 done: 5986\n")
  both           4814   166/172   write(1, "worker picked job 18\n")
  both           4825   166/171   write(1, "job 17 done: 43360\n")
  failing        4829   166/172   write(1, "job 18 done: 26744\n")
  failing        4830   166/172   write(1, "worker picked job 19\n")
  failing        4839   166/171   SIGSEGV code=1 addr=0x108
  failing        4841   166/166   SIGSEGV code=0 addr=0x0
  passing        4826   166/171   write(1, "worker picked job 19\n")
  passing        4833   166/172   write(1, "job 18 done: 26744\n")
  passing        4834   166/172   write(1, "worker picked job 20\n")
  passing        4846   166/171   thread exit(test_pool_shutd) exited:0

open both in the desktop app: rewind open aa6759a6362f43a2 4829 --compare 9ceb6f28d5ffd7ca
```

Schedules 4, 9, 13, 14 and 16 fail. `check` narrows schedule 4's failure to one step, 4825. The passing run perturbs the same steps less that one, so the two
runs are the same until 4825, and only the failing one gets
a request to switch threads there. The search took 13 seconds.

The last block shows where the test's own events then differ. The first
difference is the cause, showing up before the symptom: at step 4829
the other worker, thread 172, prints job 18 done and goes
on to pick the next job while thread 171 is held at the
deciding step; in the passing run thread 171 picks that job
first. Thread 171's SIGSEGV comes 10 steps later,
at step 4839.

`--all` tries every schedule, which measures how flaky a build is:

```console
$ rewind check --all --epoch 1790985600 github:fzakaria/rewindvm#mylib | grep 'ended differently'
17 of 64 other schedules ended differently
```

## Look at the failure

The failing run keeps every event with its step:

```console
$ rewind log aa6759a6 --steps | tail -4
      4829   166  job 18 done: 26744
      4830   166  worker picked job 19
      4858   162  /nix/store/...-bash-5.3p15/bin/bash: line 1:   166 Segmentation fault         ./$t
      4862   161  make: *** [Makefile:18: check] Error 1

$ rewind events aa6759a6 --from 4825 --to 4841
      4825   166/171   write(1, "job 17 done: 43360\n")
      4829   166/172   write(1, "job 18 done: 26744\n")
      4830   166/172   write(1, "worker picked job 19\n")
      4837     0/0     console "[    0.199159] test_pool_shutd[171]: segfault at 108 ip 00005556d620d437 sp 00007feee6e68e10 error 6 in test_pool_shutdown[1437,5556d620d000+1000] likely on CPU 0 (core 0, socket 0)"
      4838     0/0     console "[    0.199164] Code: fa 48 8d 35 2a 0c 00 00 bf 02 00 00 00 b8 00 00 00 00 e8 ac fc ff ff 48 8b 05 b5 2b 00 00 48 8b 38 e8 8d fc ff ff 49 8b 46 58 <83> 80 08 01 00 00 01 e9 b5 fe ff ff 55 48 89 e5 41 54 53 be 78 00"
      4839   166/171   SIGSEGV code=1 addr=0x108
      4841   166/166   SIGSEGV code=0 addr=0x0
```

The kernel's report marks the faulting instruction, `<83> 80 08 01 00 00 01`:
`addl $1, 0x108(%rax)`, which is `p->queue->completed++` with `p->queue`
null. The processes alive at the crash:

```console
$ rewind ps aa6759a6 4839
     1 /init
    34   bash -e /nix/store/...-source-stdenv.sh /nix/store/...-default-builder.sh
   161     make SHELL=/nix/store/...-bash-5.3p15/bin/bash PREFIX=$(out) VERBOSE=y check
   162       /nix/store/...-bash-5.3p15/bin/bash -c for t in tests/test_pool_basic tests/test_pool_shutdown; do echo "running $t"; ./$t || exit 1; done
   166         ./tests/test_pool_shutdown
   171           (thread)
   172           (thread)
```

## Look inside the VM

`rewind cat`, `rewind shell` and `rewind gdb` work on a throwaway fork of the
run at a step. At the SIGSEGV, step 4839:

```console
$ rewind cat aa6759a6 4839 src/pool.c --pid 166 | sed -n '/^void pool_shutdown/,/^}/p'
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

$ printf 'pwd; ls; exit\n' | rewind shell aa6759a6 4839 --pid 166
rewind: a shell at step 4839 of aa6759a6362f43a2; exit it to leave
[rewind] /build/mylib # pwd; ls; exit
/build/mylib
Containerfile  Makefile  libmylib.a  src  tests
```

`rewind gdb` loads the symbols of the VM's kernel and of the process running
at the step. From step 4825, thread 171's last write,
continue to the line that faulted:

```console
$ rewind gdb aa6759a6 4825 -- -batch -ex 'break src/pool.c:77 if p->queue == 0' -ex continue -ex 'bt 3' -ex 'p p->queue' -ex list
rewind: step 4825 ran in process 166; loading symbols for 4 of its files
rewind: fetched 3 source files from the VM
rewind: gdb at step 4825 of aa6759a6362f43a2
Downloading 10.70 K separate debug info for /nix/store/h4wfwic161kxrr74jlzla5lsm28hgary-glibc-2.44-25/lib/libpthread.so.0...
Downloading 4.35 M separate debug info for /nix/store/h4wfwic161kxrr74jlzla5lsm28hgary-glibc-2.44-25/lib/libc.so.6...
Downloading 658.16 K separate debug info for /nix/store/h4wfwic161kxrr74jlzla5lsm28hgary-glibc-2.44-25/lib/ld-linux-x86-64.so.2...
Downloading 3.12 K source file /build/linux-7.2.8/./arch/x86/include/asm/irqflags.h...
arch_local_irq_restore (flags=518) at ./arch/x86/include/asm/irqflags.h:146
146		return !(flags & X86_EFLAGS_IF);
[Switching to thread 3 (Thread 1.171)]
Downloading 1.79 K source file /build/glibc-2.44/nptl/../sysdeps/unix/sysv/linux/x86_64/syscall_cancel.S...
#0  __syscall_cancel_arch () at ../sysdeps/unix/sysv/linux/x86_64/syscall_cancel.S:56
56		ret
Breakpoint 1 at 0x5556d620d433: file src/pool.c, line 77.

Thread 3 hit Breakpoint 1, worker (arg=0x5556f833c010) at src/pool.c:77
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
$ rewind replay aa6759a6
identical to the recording: 1693 events, 1148 context switches and 1476 read and write records over 4887 steps, and the whole machine at its 4 keyframes

$ rewind replay aa6759a6 --from 4246
identical to the recording from the snapshot at step 256 to the end, and the whole machine at its 3 keyframes (0.48s)
```

`--from` starts at the nearest keyframe before the step. A run replays on any
machine with the same CPU vendor that has the features of the run's CPU model.

## Fork it

A fork is its parent up to a step, then another schedule:

```console
$ rewind fork 0a1dae14 4246 --schedule 1 --quiet
rewind: run 376770718d157ce2 exited:0 after 6638 steps, 0.223s virtual, 0.119s wall (poweroff)
rewind: the fork first differs from its parent at step 4254
rewind: open it beside its parent in the desktop app: rewind open 376770718d157ce2 4254 --compare 0a1dae14daee9820

$ rewind fork 0a1dae14 4246 --schedule 2 --quiet
rewind: run 5dac25d3c22f4b95 exited:0 after 6584 steps, 0.220s virtual, 0.124s wall (poweroff)
rewind: the fork first differs from its parent at step 4257
rewind: open it beside its parent in the desktop app: rewind open 5dac25d3c22f4b95 4257 --compare 0a1dae14daee9820

$ rewind fork 0a1dae14 4246 --schedule 3 --quiet
rewind: run 90ba18ef8f535ce2 exited:0 after 6632 steps, 0.224s virtual, 0.125s wall (poweroff)
rewind: the fork first differs from its parent at step 4269
rewind: open it beside its parent in the desktop app: rewind open 90ba18ef8f535ce2 4269 --compare 0a1dae14daee9820

$ rewind fork 0a1dae14 4246 --schedule 4 --quiet
rewind: run 2eca68d10c22e3a7 exited:2 after 4893 steps, 0.200s virtual, 0.067s wall (poweroff)
rewind: the fork first differs from its parent at step 4256
rewind: open it beside its parent in the desktop app: rewind open 2eca68d10c22e3a7 4256 --compare 0a1dae14daee9820
```

From step 4246 of the schedule 0 build, schedule 4 crashes and schedules 1, 2 and 3 pass. Of schedules
1 to 32 from that step, 5 crash. `check --run` asks many
schedules at once; the
[advanced tutorial](tutorial-advanced.md#count-the-schedules-that-fail-from-a-step)
shows it.

## Scrub it in the app

The desktop app shows a run on a timeline: drag the playhead to any step for
the output, processes, files and event there, jump to the failure or to where
the run left the passing one, and fork from the playhead. Opened on `check`'s
two runs, a dashed blue mark on the timeline is step 4825, where only
the failing run gets a request to switch threads.

```console
$ rewind-app ~/.local/share/rewind/runs/aa6759a6362f43a2 --compare ~/.local/share/rewind/runs/9ceb6f28d5ffd7ca
```

Press f to jump to the failure at step 4839, then s to open the
source panel. After a few seconds it shows `worker` at `src/pool.c:77`, with
`p->queue->completed++;` marked: the line that read the queue after
`pool_shutdown` had set it to NULL. The t key shows which thread held the CPU
around where the two runs part.

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
0 of 64 other schedules ended differently
same result under all 65 schedules
```

Before the fix, 17 of the same 64 schedules crashed.

## Limits

The VM has one vCPU, so threads interleave but never run at the same instant:
a race between plain loads and stores with no system call between them is out
of reach, while races across a system call, a lock or a sleep, like this one,
are in reach. [Design](design.md#limits) has the full list.

## What to read next

- [The container tutorial](tutorial-container.md): the same bug from a Docker
  image, with no Nix.
- [The advanced tutorial](tutorial-advanced.md): threads, watchpoints, the
  kernel's side of a crash, sharing a run and more.
- [Counter time](pmu.md): how the VM's clock follows its work.
