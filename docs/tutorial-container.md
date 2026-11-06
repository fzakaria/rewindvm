# Tutorial: a flaky test in a container

This tutorial takes a container image whose tests fail now and then, has
Rewind VM find a failing run, looks at the crash, and checks the fix across 64
thread interleavings. It needs no Nix.

The image is built from `mylib`, a small C thread pool in
[examples/mylib](https://github.com/fzakaria/rewindvm/tree/main/examples/mylib),
with the Containerfile next to its source. Its `pool_shutdown` frees the job
queue before joining the workers, so a worker still finishing a job can write
through a freed, nulled pointer.

Every transcript below is real output from `rewind` on a 16 core AMD laptop.

## Install

You need x86_64 Linux with KVM, Docker or Podman, and gdb.

```console
$ curl -fsSL https://rewindvm.dev/install | REWIND_WITH_DEBUG=1 sh
$ ls -l /dev/kvm
crw-rw-rw- 1 root kvm 10, 232 Oct  1 20:17 /dev/kvm
```

The script unpacks the latest
[release](https://github.com/fzakaria/rewindvm/releases) under
`~/.local/share/rewind`, links `rewind` and `rewind-app` into `~/.local/bin`,
and with `REWIND_WITH_DEBUG=1` adds the VM kernel's debug symbols for `rewind
gdb`. The command is static and brings the VM's kernel, so the host needs
nothing else. If `/dev/kvm` is not yours to use, add yourself to the `kvm`
group. On AMD, run `sudo rewind pmu enable` once after each boot;
[Counter time](pmu.md) says why.

## The flaky test

Clone the repository and build the image, which compiles mylib on Debian:

```console
$ git clone https://github.com/fzakaria/rewindvm
$ cd rewindvm/examples/mylib
$ docker build -t mylib -f Containerfile .
```

On the host, `docker run --rm mylib` runs `make check` and usually passes.
Run 60 times, it failed 3 times:

```console
$ docker run --rm mylib
running tests/test_pool_basic
...
running tests/test_pool_shutdown
...
worker picked job 17
job 16 done: 5986
Segmentation fault (core dumped)
make: *** [Makefile:18: check] Error 1
```

Run it again and it usually passes, leaving nothing to look at.

## Run it in Rewind VM

`rewind` runs a command in any root filesystem, such as the tarball `docker
export` writes, inside a deterministic virtual machine:

```console
$ docker export $(docker create mylib) -o mylib.tar
$ rewind run --root mylib.tar --cwd /src -- make check
...
round 3: ok
test_pool_shutdown: ok
rewind: run 0eada656c0127acd exited:0 after 1314 steps, 0.019s virtual, 0.362s wall (poweroff)
```

It passes, in the same 1314 steps every time. Nothing the command
writes reaches your disk. A run's id is the hash of its inputs, so your image
makes ids other than these.

## Find a failing interleaving

`rewind check` runs it again under perturbed schedules, which reschedule and
stall the VM's threads at different points, and stops at the first batch in
which a run ends differently:

```console
$ rewind check --root mylib.tar --cwd /src -- make check
schedule   0: exited:0             1314 steps    run 0eada656c0127acd
schedule   1: exited:0             1500 steps    run 9356b3e100312452
schedule   2: exited:0             1650 steps    run f130916faa024479
schedule   3: exited:0             1495 steps    run b832b0e462c708cc
schedule   4: exited:0             1544 steps    run a780f3446921678c
schedule   5: exited:0             1661 steps    run 52123dd31633c4e1
schedule   6: exited:2             1154 steps    run 9fe5ad0de66e2620
schedule   7: exited:2              773 steps    run 64a80f8d32688665
schedule   8: exited:0             1464 steps    run 2990d4545d9676a0
schedule   9: exited:2              844 steps    run 07e80a1bac5dd233
schedule  10: exited:0             1553 steps    run fa9b13acc4c2d2e6
schedule  11: exited:0             1555 steps    run 64f64b513ab8fc84
schedule  12: exited:0             1662 steps    run 43e4488a57031080
schedule  13: exited:0             1564 steps    run 378d2f326b2b8a0e
schedule  14: exited:0             1538 steps    run 0e78403ab6449aa6
schedule  15: exited:2             1552 steps    run bbc7c2653e4c68f9
schedule  16: exited:2             1335 steps    run 575f96eba8c7debd

schedule 6 ends differently from schedule 0; finding the step that decides it
schedule 6 limited to steps 297..1104 still ends differently
step 1103 decides it: a request to switch threads there makes the run fail

passing: run 8da6fdb7de620b24, schedule 6 over steps 297..1103
failing: run 0f3425671abc151f, schedule 6 over steps 297..1104
the two are the same run until step 1103

where ./tests/test_pool_shutdown first behaves differently:
  both           1088    39/43    write(1, "job 16 done: 5986\n")
  both           1089    39/43    write(1, "worker picked job 18\n")
  both           1103    39/42    write(1, "job 17 done: 43360\n")
  failing        1107    39/43    write(1, "job 18 done: 26744\n")
  failing        1110    39/43    SIGSEGV code=1 addr=0x108
  failing        1113    39/39    SIGSEGV code=0 addr=0x0
  failing        1115    39/43    thread exit(test_pool_shutd) killed:SIGSEGV
  passing        1104    39/42    write(1, "worker picked job 19\n")
  passing        1110    39/43    write(1, "job 18 done: 26744\n")
  passing        1111    39/43    write(1, "worker picked job 20\n")
  passing        1122    39/42    thread exit(test_pool_shutd) exited:0

open both in the desktop app: rewind open 0f3425671abc151f 1107 --compare 8da6fdb7de620b24
```

`check` narrows schedule 6's failure to one step, 1103. The
passing run perturbs the same steps less that one, and only the failing run
gets a request to switch threads there. The last block shows where the test's own output
then differs. The search took 3 seconds. `--all` tries every
schedule:

```console
$ rewind check --all --root mylib.tar --cwd /src -- make check | grep 'ended differently'
15 of 64 other schedules ended differently
```

## Look at the failure

```console
$ rewind log 0f342567 --steps | tail -4
      1103    39  job 17 done: 43360
      1107    39  job 18 done: 26744
      1125    35  Segmentation fault
      1130    34  make: *** [Makefile:18: check] Error 1

$ rewind events 0f342567 --from 1107 --to 1112
      1107    39/43    write(1, "job 18 done: 26744\n")
      1108     0/0     console "[    0.017636] test_pool_shutd[43]: segfault at 108 ip 000055c225bfb412 sp 00007ffbdb986ea0 error 6 in test_pool_shutdown[1412,55c225bfb000+1000] likely on CPU 0 (core 0, socket 0)"
      1109     0/0     console "[    0.017641] Code: 5f c3 0f b7 d3 44 89 f6 48 8d 3d 48 0c 00 00 b8 00 00 00 00 e8 6f fc ff ff 48 8b 3d 90 2c 00 00 e8 c3 fc ff ff 49 8b 44 24 58 <83> 80 08 01 00 00 01 e9 c6 fe ff ff 55 53 48 83 ec 08 be 78 00 00"
      1110    39/43    SIGSEGV code=1 addr=0x108
```

The kernel's report marks the faulting instruction, `<83> 80 08 01 00 00 01`:
`addl $1, 0x108(%rax)`, which is `p->queue->completed++` with `p->queue`
null. The processes alive at the crash:

```console
$ rewind ps 0f342567 1110
     1 /init
    34   make check
    35     /bin/sh -c for t in tests/test_pool_basic tests/test_pool_shutdown; do echo "running $t"; ./$t || exit 1; done
    39       ./tests/test_pool_shutdown
    42         (thread)
    43         (thread)
```

## Look inside the VM

`rewind cat`, `rewind shell` and `rewind gdb` work on a throwaway fork of the
run at a step. At the SIGSEGV, step 1110:

```console
$ rewind cat 0f342567 1110 src/pool.c --pid 39 | sed -n '/^void pool_shutdown/,/^}/p'
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

$ printf 'pwd; ls; exit\n' | rewind shell 0f342567 1110 --pid 39
rewind: a shell at step 1110 of 0f3425671abc151f; exit it to leave
[rewind] /src # pwd; ls; exit
/src
Containerfile  Makefile  libmylib.a  src  tests
```

`rewind gdb` loads the symbols of the VM's kernel and of the process running
at the step. Debian strips its libc; `DEBUGINFOD_URLS` names Debian's
debuginfod server, which has its symbols. From step 1107, thread
43's last write, continue to the line that faulted:

```console
$ DEBUGINFOD_URLS=https://debuginfod.debian.net rewind gdb 0f342567 1107 -- -batch -ex 'break src/pool.c:77 if p->queue == 0' -ex continue -ex 'bt 3' -ex 'p p->queue' -ex list
rewind: step 1107 ran in process 39; loading symbols for 3 of its files
rewind: fetched 12 source files from the VM
rewind: gdb 16 cannot read thread-local variables, such as errno, at a step; gdb 17 or later can
rewind: gdb at step 1107 of 0f3425671abc151f
Downloading 3.97 M separate debug info for /home/fmzakari/.cache/rewind-record/home/gdb/2841502-0/newroot/usr/lib/x86_64-linux-gnu/libc.so.6...
Downloading 539.96 K separate debug info for /home/fmzakari/.cache/rewind-record/home/gdb/2841502-0/newroot/usr/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2...
arch_local_irq_restore (flags=514) at ./arch/x86/include/asm/irqflags.h:146
146		return !(flags & X86_EFLAGS_IF);
[Switching to thread 4 (Thread 1.43)]
Download failed: Invalid argument.  Continuing without source file ./io/../sysdeps/unix/sysv/linux/write.c.
#0  __GI___libc_write (nbytes=19, buf=0x7ffbd4000b70, fd=1) at ../sysdeps/unix/sysv/linux/write.c:26
warning: 26	../sysdeps/unix/sysv/linux/write.c: No such file or directory
Breakpoint 1 at 0x55c225bfb40d: file src/pool.c, line 77.

Thread 4 hit Breakpoint 1, worker (arg=0x55c25619b680) at src/pool.c:77
77				p->queue->completed++;
#0  worker (arg=0x55c25619b680) at src/pool.c:77
#1  0x00007ffbdba141f5 in start_thread (arg=<optimized out>) at ./nptl/pthread_create.c:442
#2  0x00007ffbdba948ec in clone3 () at ../sysdeps/unix/sysv/linux/x86_64/clone3.S:81
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

Debian's server has libc's symbols but not its sources, so libc's frames show
no source and gdb prints `Download failed: Invalid argument` for each.

Images built on Fedora, Ubuntu or Arch have debuginfod servers of their own,
listed by [elfutils](https://sourceware.org/elfutils/Debuginfod.html).

## Replay it

```console
$ rewind replay 0f342567
identical to the recording: 324 events and 13 read and write records over 1141 steps, and the whole machine at its 4 keyframes

$ rewind replay 0f342567 --from 297
identical to the recording from the snapshot at step 256 to the end, and the whole machine at its 3 keyframes (0.11s)
```

`--from` starts at the nearest keyframe before the step. A run replays on any
machine with the same CPU vendor that has the features of the run's CPU model.

## Fork it

```console
$ rewind fork 0eada656 297 --schedule 3 --quiet
rewind: run b832b0e462c708cc exited:0 after 1495 steps, 0.022s virtual, 0.061s wall (poweroff)
rewind: the fork first differs from its parent at step 318
rewind: open it beside its parent in the desktop app: rewind open b832b0e462c708cc 318 --compare 0eada656c0127acd

$ rewind fork 0eada656 297 --schedule 4 --quiet
rewind: run a780f3446921678c exited:0 after 1544 steps, 0.021s virtual, 0.061s wall (poweroff)
rewind: the fork first differs from its parent at step 316
rewind: open it beside its parent in the desktop app: rewind open a780f3446921678c 316 --compare 0eada656c0127acd

$ rewind fork 0eada656 297 --schedule 5 --quiet
rewind: run 52123dd31633c4e1 exited:0 after 1661 steps, 0.022s virtual, 0.062s wall (poweroff)
rewind: the fork first differs from its parent at step 316
rewind: open it beside its parent in the desktop app: rewind open 52123dd31633c4e1 316 --compare 0eada656c0127acd

$ rewind fork 0eada656 297 --schedule 6 --quiet
rewind: run 9fe5ad0de66e2620 exited:2 after 1154 steps, 0.018s virtual, 0.052s wall (poweroff)
rewind: the fork first differs from its parent at step 316
rewind: open it beside its parent in the desktop app: rewind open 9fe5ad0de66e2620 316 --compare 0eada656c0127acd
```

From step 297 of the schedule 0 run, some schedules crash and
some pass.

## Scrub it in the app

The desktop app shows a run on a timeline: drag the playhead to any step for
the output, processes, files and event there, jump to the failure or to where
the run left the passing one, and fork from the playhead.

```console
$ rewind-app ~/.local/share/rewind/runs/0f3425671abc151f --compare ~/.local/share/rewind/runs/8da6fdb7de620b24
```

Press f to jump to the failure at step 1110, then s to open the
source panel. After a few seconds it shows `worker` at `src/pool.c:77`, with
`p->queue->completed++;` marked: the line that read the queue after
`pool_shutdown` had set it to NULL. The t key shows which thread held the CPU
around where the two runs part.

## Fix it and check the fix

In `src/pool.c`, count the job under the lock, and free the queue after the
workers are joined:

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

Rebuild the image, export it, and check again:

```console
$ docker build -q -t mylib -f Containerfile . && docker export $(docker create mylib) -o mylib.tar
sha256:cb9278c70a0b53ffbe08e7d6ee12e362cf347b40d17f8167415014f181c1ecdf

$ rewind check --all --root mylib.tar --cwd /src -- make check
...
0 of 64 other schedules ended differently
same result under all 65 schedules
```

Before the fix, 15 of the same 64 schedules crashed.

## Limits

The VM has one vCPU, so threads interleave but never run at the same instant:
a race between plain loads and stores with no system call between them is out
of reach, while races across a system call, a lock or a sleep, like this one,
are in reach. The container runs with no network. [Design](design.md#limits)
has the full list.

## What to read next

- [The Nix tutorial](tutorial-nix.md): the same bug as a Nix derivation.
- [The advanced tutorial](tutorial-advanced.md): threads, watchpoints, the
  kernel's side of a crash, sharing a run and more.
- [Counter time](pmu.md): how the VM's clock follows its work.
