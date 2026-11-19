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
rewind: run 2e7b45d21f14f92a exited:0 after 6032 steps, 0.215s virtual, 0.560s wall (poweroff)
/nix/store/f6a9gy362szw6nxx3ikrklr8glr6rdln-mylib-0.3.0 a9d703ba89774f3d  matches your store, rewindvm.cachix.org
```

It passes, in the same 6032 steps every time. A step is a point
where the VM stops and hands control to Rewind, such as a system call that
reports a file or a process; steps number a run's events and are where you
look inside it. `--epoch` fixes the
VM's clock at boot, one of the run's inputs, so your runs are the ones shown
here. The last line says the output matches the copy in your store.

## Find a failing interleaving

`rewind check` builds it again under perturbed schedules, which reschedule
and stall the VM's threads at different points, and stops at the first batch
in which a build ends differently:

```console
$ rewind check --epoch 1790985600 github:fzakaria/rewindvm#mylib
schedule   0: exited:0             6032 steps  a9d703ba8977  run 2e7b45d21f14f92a
schedule   1: exited:0             7672 steps  a9d703ba8977  run f5dc521e34f746e3
schedule   2: exited:0             7750 steps  a9d703ba8977  run d8ea017d8bd5dfb7
schedule   3: exited:0             7717 steps  a9d703ba8977  run 5ff7f8d540856b29
schedule   4: exited:0             7825 steps  a9d703ba8977  run e80bf3a0d2bfc7e1
schedule   5: exited:0             7596 steps  a9d703ba8977  run e81c0935eccbb782
schedule   6: exited:0             7712 steps  a9d703ba8977  run 40756ae5bcb7786c
schedule   7: exited:2             5623 steps    run 80bd50ab329cdc81
schedule   8: exited:2             5868 steps    run f18e5729b99a4c37
schedule   9: exited:2             5377 steps    run 0c8112e693c6b6f3
schedule  10: exited:0             7604 steps  a9d703ba8977  run 4319d426f9987050
schedule  11: exited:0             7539 steps  a9d703ba8977  run 21c750ea15832690
schedule  12: exited:0             7792 steps  a9d703ba8977  run 74396e73842310df
schedule  13: exited:0             7724 steps  a9d703ba8977  run 6424cff4fe42b9e5
schedule  14: exited:0             7708 steps  a9d703ba8977  run 979e1318d22cf998
schedule  15: exited:2             6210 steps    run 30c5faffb6771e0e
schedule  16: exited:0             7660 steps  a9d703ba8977  run c54083d3beb665a2

schedule 7 ends differently from schedule 0; finding the step that decides it
schedule 7 limited to steps 2818..4978 still ends differently
step 4977 decides it: a request to switch threads there makes the run fail

passing: run 6a609b9c1faefbc8, schedule 7 over steps 2818..4977
failing: run e20758b82c63ae48, schedule 7 over steps 2818..4978
the two are the same run until step 4977

where ./tests/test_pool_shutdown first behaves differently:
  both           4963   166/169   write(1, "job 15 done: 42559\n")
  both           4964   166/169   write(1, "worker picked job 16\n")
  both           4977   166/170   write(1, "job 14 done: 39906\n")
  failing        4981   166/169   write(1, "job 16 done: 5986\n")
  failing        4982   166/169   write(1, "worker picked job 17\n")
  failing        4992   166/170   SIGSEGV code=1 addr=0x108
  failing        4999   166/170   thread exit(test_pool_shutd) killed:SIGSEGV
  passing        4978   166/170   write(1, "worker picked job 17\n")
  passing        4986   166/169   thread exit(test_pool_shutd) exited:0
  passing        4998   166/170   thread exit(test_pool_shutd) exited:0
  passing        5001   166/166   write(1, "round 1: ok\n")

open both in the desktop app: rewind open e20758b82c63ae48 4981 --compare 6a609b9c1faefbc8
```

Schedules 7, 8, 9 and 15 fail. `check` narrows schedule 7's failure to one step, 4977. The passing run perturbs the same steps less that one, so the two
runs are the same until 4977, and only the failing one gets
a request to switch threads there. The search took 8 seconds.

The last block shows where the test's own events then differ. The first
difference is the cause, showing up before the symptom. At the deciding step
thread 170 has reported job 14 and has yet to count
it, which a worker does outside the lock. In the failing run it is held
there: at step 4981 the other worker, thread 169, reports
job 16 and goes on to pick job 17, the test sees half the
jobs done and shuts the pool down, and thread 170 counts its
job through the queue the shutdown freed. In the passing run thread
170 counts job 14 and picks job 17 first.
Thread 170's SIGSEGV comes 11 steps later, at step 4992.

`--all` tries every schedule, which measures how flaky a build is:

```console
$ rewind check --all --epoch 1790985600 github:fzakaria/rewindvm#mylib | grep 'ended differently'
17 of 64 other schedules ended differently
```

## Look at the failure

The failing run keeps every event with its step:

```console
$ rewind log e20758b8 --steps | tail -4
      4981   166  job 16 done: 5986
      4982   166  worker picked job 17
      5010   162  /nix/store/...-bash-5.3p15/bin/bash: line 1:   166 Segmentation fault         ./$t
      5014   161  make: *** [Makefile:18: check] Error 1

$ rewind events e20758b8 --from 4977 --to 4994
      4977   166/170   write(1, "job 14 done: 39906\n")
      4981   166/169   write(1, "job 16 done: 5986\n")
      4982   166/169   write(1, "worker picked job 17\n")
      4990     0/0     console "[    0.204005] test_pool_shutd[170]: segfault at 108 ip 000055c06fa1d437 sp 00007fda39950e10 error 6 in test_pool_shutdown[1437,55c06fa1d000+1000] likely on CPU 0 (core 0, socket 0)"
      4991     0/0     console "[    0.204010] Code: fa 48 8d 35 2a 0c 00 00 bf 02 00 00 00 b8 00 00 00 00 e8 ac fc ff ff 48 8b 05 b5 2b 00 00 48 8b 38 e8 8d fc ff ff 49 8b 46 58 <83> 80 08 01 00 00 01 e9 b5 fe ff ff 55 48 89 e5 41 54 53 be 78 00"
      4992   166/170   SIGSEGV code=1 addr=0x108
```

The kernel's report marks the faulting instruction, `<83> 80 08 01 00 00 01`:
`addl $1, 0x108(%rax)`, which is `p->queue->completed++` with `p->queue`
null. The processes alive at the crash:

```console
$ rewind ps e20758b8 4992
     1 /init
    34   bash -e /nix/store/...-source-stdenv.sh /nix/store/...-default-builder.sh
   161     make SHELL=/nix/store/...-bash-5.3p15/bin/bash PREFIX=$(out) VERBOSE=y check
   162       /nix/store/...-bash-5.3p15/bin/bash -c for t in tests/test_pool_basic tests/test_pool_shutdown; do echo "running $t"; ./$t || exit 1; done
   166         ./tests/test_pool_shutdown
   169           (thread)
   170           (thread)
```

## Look inside the VM

`rewind cat`, `rewind shell` and `rewind gdb` work on a throwaway fork of the
run at a step. At the SIGSEGV, step 4992:

```console
$ rewind cat e20758b8 4992 src/pool.c --pid 166 | sed -n '/^void pool_shutdown/,/^}/p'
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

$ printf 'pwd; ls; exit\n' | rewind shell e20758b8 4992 --pid 166
rewind: a shell at step 4992 of e20758b82c63ae48; exit it to leave
[rewind] /build/mylib # pwd; ls; exit
/build/mylib
Containerfile  Makefile  libmylib.a  src  tests
```

`rewind gdb` loads the symbols of the VM's kernel and of the process running
at the step. From step 4977, thread 170's last write,
continue to the line that faulted:

```console
$ rewind gdb e20758b8 4977 -- -batch -ex 'break src/pool.c:77 if p->queue == 0' -ex continue -ex 'bt 3' -ex 'p p->queue' -ex list
rewind: step 4977 ran in process 166; loading symbols for 4 of its files
rewind: fetched 3 source files from the VM
rewind: gdb at step 4977 of e20758b82c63ae48
Downloading 10.70 K separate debug info for /nix/store/h4wfwic161kxrr74jlzla5lsm28hgary-glibc-2.44-25/lib/libpthread.so.0...
Downloading 4.35 M separate debug info for /nix/store/h4wfwic161kxrr74jlzla5lsm28hgary-glibc-2.44-25/lib/libc.so.6...
Downloading 658.16 K separate debug info for /nix/store/h4wfwic161kxrr74jlzla5lsm28hgary-glibc-2.44-25/lib/ld-linux-x86-64.so.2...
Downloading 3.12 K source file /build/linux-7.2.8/./arch/x86/include/asm/irqflags.h...
arch_local_irq_restore (flags=518) at ./arch/x86/include/asm/irqflags.h:146
146		return !(flags & X86_EFLAGS_IF);
[Switching to thread 4 (Thread 1.170)]
Downloading 1.79 K source file /build/glibc-2.44/nptl/../sysdeps/unix/sysv/linux/x86_64/syscall_cancel.S...
#0  __syscall_cancel_arch () at ../sysdeps/unix/sysv/linux/x86_64/syscall_cancel.S:56
56		ret
Breakpoint 1 at 0x55c06fa1d433: file src/pool.c, line 77.

Thread 4 hit Breakpoint 1, worker (arg=0x55c074530010) at src/pool.c:77
77				p->queue->completed++;
#0  worker (arg=0x55c074530010) at src/pool.c:77
#1  0x00007fda399f47d1 in start_thread (arg=<optimized out>) at pthread_create.c:454
#2  0x00007fda39a80b1c in __GI___clone3 () at ../sysdeps/unix/sysv/linux/x86_64/clone3.S:78
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
$ rewind replay e20758b8
identical to the recording: 1646 events, 1399 context switches and 1476 read and write records over 5037 steps, and the whole machine at its 4 keyframes

$ rewind replay e20758b8 --from 2818
identical to the recording from the keyframe at step 256 to the end, and the whole machine at its 3 keyframes (0.40s)
```

`--from` starts at the nearest keyframe before the step. A keyframe is a copy
of the VM's whole state, registers and memory, that recording saves every so
many steps, so a replay can start there instead of at boot. A run replays on any
machine with the same CPU vendor that has the features of the run's CPU model.

## Fork it

A fork is its parent up to a step, then another schedule:

```console
$ rewind fork 2e7b45d2 2818 --schedule 1 --quiet
rewind: run 14219c78cadede42 exited:2 after 4802 steps, 0.201s virtual, 0.176s wall (poweroff)
rewind: the fork first differs from its parent at step 2824
rewind: open it beside its parent in the desktop app: rewind open 14219c78cadede42 2824 --compare 2e7b45d21f14f92a

$ rewind fork 2e7b45d2 2818 --schedule 2 --quiet
rewind: run 833681b7d0b0ac7b exited:0 after 7081 steps, 0.227s virtual, 0.231s wall (poweroff)
rewind: the fork first differs from its parent at step 2866
rewind: open it beside its parent in the desktop app: rewind open 833681b7d0b0ac7b 2866 --compare 2e7b45d21f14f92a

$ rewind fork 2e7b45d2 2818 --schedule 3 --quiet
rewind: run 3e20ad3124957f71 exited:0 after 7240 steps, 0.230s virtual, 0.224s wall (poweroff)
rewind: the fork first differs from its parent at step 2827
rewind: open it beside its parent in the desktop app: rewind open 3e20ad3124957f71 2827 --compare 2e7b45d21f14f92a

$ rewind fork 2e7b45d2 2818 --schedule 4 --quiet
rewind: run 0fd7901aeeaa203f exited:0 after 7235 steps, 0.233s virtual, 0.226s wall (poweroff)
rewind: the fork first differs from its parent at step 2833
rewind: open it beside its parent in the desktop app: rewind open 0fd7901aeeaa203f 2833 --compare 2e7b45d21f14f92a
```

From step 2818 of the schedule 0 build, schedule 1 crashes and schedules 2, 3 and 4 pass. Of schedules
1 to 32 from that step, 11 crash. `check --run` asks many
schedules at once; the
[advanced tutorial](tutorial-advanced.md#count-the-schedules-that-fail-from-a-step)
shows it.

## Scrub it in the app

The desktop app shows a run on a timeline: drag the playhead to any step for
the output, processes, files and event there, jump to the failure or to where
the run left the passing one, and fork from the playhead. Opened on `check`'s
two runs, a dashed blue mark on the timeline is step 4977, where only
the failing run gets a request to switch threads.

```console
$ rewind-app ~/.local/share/rewind/runs/e20758b82c63ae48 --compare ~/.local/share/rewind/runs/6a609b9c1faefbc8
```

Press f to jump to the failure at step 4992, then s to open the
source panel. After a few seconds it shows `worker` at `src/pool.c:77`, with
`p->queue->completed++;` marked: the line that read the queue after
`pool_shutdown` had set it to NULL. The t key shows which thread held the CPU
around where the two runs part.

## Fix it and check the fix

Clone the repository to edit the flake's `mylib`:

```console
$ git clone https://github.com/fzakaria/rewindvm
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
