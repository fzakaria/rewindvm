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
rewind: run 25bb1f778ce6c669 exited:0 after 1784 steps, 0.019s virtual, 0.658s wall (poweroff)
```

It passes, in the same 1784 steps every time. Nothing the command
writes reaches your disk. A run's id is the hash of its inputs, so your image
makes ids other than these.

## Find a failing interleaving

`rewind check` runs it again under perturbed schedules, which reschedule and
stall the VM's threads at different points, and stops at the first batch in
which a run ends differently:

```console
$ rewind check --root mylib.tar --cwd /src -- make check
schedule   0: exited:0             1784 steps    run 25bb1f778ce6c669
schedule   1: exited:2             1732 steps    run 9538642ff240dc45
schedule   2: exited:0             2071 steps    run 9bd9371e2574d17e
schedule   3: exited:2             1514 steps    run 67459977d6f68d34
schedule   4: exited:2             1844 steps    run ca39f923f0d79bbe
schedule   5: exited:0             2188 steps    run bc60c18508cd2148
schedule   6: exited:2             1493 steps    run 32132cb4975b2c0e
schedule   7: exited:2             2065 steps    run b2c69a52806c1c9e
schedule   8: exited:2             1269 steps    run ad3a7a84b6d60ed7
schedule   9: exited:0             2032 steps    run fbd4a9062523872b
schedule  10: exited:0             1997 steps    run 4dae6b771d611893
schedule  11: exited:0             1988 steps    run c58baa7b88a42e34
schedule  12: exited:0             2009 steps    run 96e5aec238b7af10
schedule  13: exited:0             2125 steps    run fdb87e13bd45e65d
schedule  14: exited:0             2071 steps    run 1a397bf42ca84f78
schedule  15: exited:0             2042 steps    run c8f04b280988c2a5
schedule  16: exited:0             1978 steps    run 6384fade2933adbf

schedule 1 ends differently from schedule 0; finding the step that decides it
schedule 1 limited to steps 720..1665 still ends differently
step 1664 decides it: a request to switch threads there makes the run fail

passing: run 55575ab6ec3ef2e5, schedule 1 over steps 720..1664
failing: run 294ed8a218e7161f, schedule 1 over steps 720..1665
the two are the same run until step 1664

where ./tests/test_pool_shutdown first behaves differently:
  both           1652    39/44    write(1, "job 14 done: 39906\n")
  both           1653    39/44    write(1, "worker picked job 17\n")
  both           1664    39/45    write(1, "job 16 done: 5986\n")
  failing        1669    39/44    write(1, "job 17 done: 43360\n")
  failing        1672    39/44    SIGSEGV code=1 addr=0x108
  failing        1675    39/39    SIGSEGV code=0 addr=0x0
  failing        1678    39/44    thread exit(test_pool_shutd) killed:SIGSEGV
  passing        1665    39/45    write(1, "worker picked job 18\n")
  passing        1671    39/44    write(1, "job 17 done: 43360\n")
  passing        1672    39/44    write(1, "worker picked job 19\n")
  passing        1685    39/45    thread exit(test_pool_shutd) exited:0

open both in the desktop app: rewind open 294ed8a218e7161f 1669 --compare 55575ab6ec3ef2e5
```

`check` narrows schedule 1's failure to one step, 1664. The
passing run perturbs the same steps less that one, and only the failing run
gets a request to switch threads there. The last block shows where the test's own output
then differs. The search took 5 seconds. `--all` tries every
schedule:

```console
$ rewind check --all --root mylib.tar --cwd /src -- make check | grep 'ended differently'
19 of 64 other schedules ended differently
```

## Look at the failure

```console
$ rewind log 294ed8a2 --steps | tail -4
      1664    39  job 16 done: 5986
      1669    39  job 17 done: 43360
      1688    35  Segmentation fault
      1692    34  make: *** [Makefile:18: check] Error 1

$ rewind events 294ed8a2 --from 1669 --to 1674
      1669    39/44    write(1, "job 17 done: 43360\n")
      1670     0/0     console "[    0.017895] test_pool_shutd[44]: segfault at 108 ip 00005579e6cd1412 sp 00007f28120c6ea0 error 6 in test_pool_shutdown[1412,5579e6cd1000+1000] likely on CPU 0 (core 0, socket 0)"
      1671     0/0     console "[    0.017900] Code: 5f c3 0f b7 d3 44 89 f6 48 8d 3d 48 0c 00 00 b8 00 00 00 00 e8 6f fc ff ff 48 8b 3d 90 2c 00 00 e8 c3 fc ff ff 49 8b 44 24 58 <83> 80 08 01 00 00 01 e9 c6 fe ff ff 55 53 48 83 ec 08 be 78 00 00"
      1672    39/44    SIGSEGV code=1 addr=0x108
```

The kernel's report marks the faulting instruction, `<83> 80 08 01 00 00 01`:
`addl $1, 0x108(%rax)`, which is `p->queue->completed++` with `p->queue`
null. The processes alive at the crash:

```console
$ rewind ps 294ed8a2 1672
     1 /init
    34   make check
    35     /bin/sh -c for t in tests/test_pool_basic tests/test_pool_shutdown; do echo "running $t"; ./$t || exit 1; done
    39       ./tests/test_pool_shutdown
    44         (thread)
    45         (thread)
```

## Look inside the VM

`rewind cat`, `rewind shell` and `rewind gdb` work on a throwaway fork of the
run at a step. At the SIGSEGV, step 1672:

```console
$ rewind cat 294ed8a2 1672 src/pool.c --pid 39 | sed -n '/^void pool_shutdown/,/^}/p'
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

$ printf 'pwd; ls; exit\n' | rewind shell 294ed8a2 1672 --pid 39
rewind: a shell at step 1672 of 294ed8a218e7161f; exit it to leave
[rewind] /src # pwd; ls; exit
/src
Containerfile  Makefile  libmylib.a  src  tests
```

`rewind gdb` loads the symbols of the VM's kernel and of the process running
at the step. Debian strips its libc; `DEBUGINFOD_URLS` names Debian's
debuginfod server, which has its symbols. From step 1669, thread
44's last write, continue to the line that faulted:

```console
$ DEBUGINFOD_URLS=https://debuginfod.debian.net rewind gdb 294ed8a2 1669 -- -batch -ex 'break src/pool.c:77 if p->queue == 0' -ex continue -ex 'bt 3' -ex 'p p->queue' -ex list
rewind: step 1669 ran in process 39; loading symbols for 3 of its files
rewind: fetched 12 source files from the VM
rewind: gdb 16 cannot read thread-local variables, such as errno, at a step; gdb 17 or later can
rewind: gdb at step 1669 of 294ed8a218e7161f
Downloading 3.97 M separate debug info for /home/fmzakari/.cache/rewind-record/home/gdb/782209-0/newroot/usr/lib/x86_64-linux-gnu/libc.so.6...
Downloading 539.96 K separate debug info for /home/fmzakari/.cache/rewind-record/home/gdb/782209-0/newroot/usr/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2...
arch_local_irq_restore (flags=514) at ./arch/x86/include/asm/irqflags.h:146
146		return !(flags & X86_EFLAGS_IF);
[Switching to thread 3 (Thread 1.44)]
Download failed: Invalid argument.  Continuing without source file ./io/../sysdeps/unix/sysv/linux/write.c.
#0  __GI___libc_write (nbytes=19, buf=0x7f280c000b70, fd=1) at ../sysdeps/unix/sysv/linux/write.c:26
warning: 26	../sysdeps/unix/sysv/linux/write.c: No such file or directory
Breakpoint 1 at 0x5579e6cd140d: file src/pool.c, line 77.

Thread 3 hit Breakpoint 1, worker (arg=0x557a2066f820) at src/pool.c:77
77				p->queue->completed++;
#0  worker (arg=0x557a2066f820) at src/pool.c:77
#1  0x00007f28121541f5 in start_thread (arg=<optimized out>) at ./nptl/pthread_create.c:442
#2  0x00007f28121d48ec in clone3 () at ../sysdeps/unix/sysv/linux/x86_64/clone3.S:81
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
$ rewind replay 294ed8a2
identical to the recording: 349 events, 704 context switches and 13 read and write records over 1701 steps, and the whole machine at its 4 keyframes

$ rewind replay 294ed8a2 --from 720
identical to the recording from the snapshot at step 256 to the end, and the whole machine at its 3 keyframes (0.14s)
```

`--from` starts at the nearest keyframe before the step. A run replays on any
machine with the same CPU vendor that has the features of the run's CPU model.

## Fork it

```console
$ rewind fork 25bb1f77 720 --schedule 1 --quiet
rewind: run 01bbc75344d0fb53 exited:2 after 1732 steps, 0.019s virtual, 0.063s wall (poweroff)
rewind: the fork first differs from its parent at step 739
rewind: open it beside its parent in the desktop app: rewind open 01bbc75344d0fb53 739 --compare 25bb1f778ce6c669

$ rewind fork 25bb1f77 720 --schedule 2 --quiet
rewind: run 70ce67f5380decb4 exited:0 after 2031 steps, 0.023s virtual, 0.059s wall (poweroff)
rewind: the fork first differs from its parent at step 737
rewind: open it beside its parent in the desktop app: rewind open 70ce67f5380decb4 737 --compare 25bb1f778ce6c669

$ rewind fork 25bb1f77 720 --schedule 3 --quiet
rewind: run 64ed7b638c6017b3 exited:0 after 2059 steps, 0.023s virtual, 0.059s wall (poweroff)
rewind: the fork first differs from its parent at step 735
rewind: open it beside its parent in the desktop app: rewind open 64ed7b638c6017b3 735 --compare 25bb1f778ce6c669

$ rewind fork 25bb1f77 720 --schedule 4 --quiet
rewind: run a07e2cccf42a4dfe exited:0 after 2041 steps, 0.021s virtual, 0.062s wall (poweroff)
rewind: the fork first differs from its parent at step 737
rewind: open it beside its parent in the desktop app: rewind open a07e2cccf42a4dfe 737 --compare 25bb1f778ce6c669
```

From step 720 of the schedule 0 run, some schedules crash and
some pass.

## Scrub it in the app

The desktop app shows a run on a timeline: drag the playhead to any step for
the output, processes, files and event there, jump to the failure or to where
the run left the passing one, and fork from the playhead.

```console
$ rewind-app ~/.local/share/rewind/runs/294ed8a218e7161f --compare ~/.local/share/rewind/runs/55575ab6ec3ef2e5
```

Press f to jump to the failure at step 1672, then s to open the
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
sha256:226f3d486c6dfec484a99d430a9d0e6f1f830f6e325337fa657d301acfcb4b67

$ rewind check --all --root mylib.tar --cwd /src -- make check
...
0 of 64 other schedules ended differently
same result under all 65 schedules
```

Before the fix, 19 of the same 64 schedules crashed.

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
