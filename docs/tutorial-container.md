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
rewind: run c181c81b4c850544 exited:0 after 1321 steps, 0.019s virtual, 1.554s wall (poweroff)
```

It passes, in the same 1321 steps every time. Nothing the command
writes reaches your disk. A run's id is the hash of its inputs, so your image
makes ids other than these.

## Find a failing interleaving

`rewind check` runs it again under perturbed schedules, which reschedule the
VM's threads at different points, and stops at the first batch in which a run
ends differently:

```console
$ rewind check --root mylib.tar --cwd /src -- make check
schedule   0: exited:0       1321 steps    run c181c81b4c850544
schedule   1: exited:2       1457 steps    run 4a8af28e1668f703
schedule   2: exited:0       1590 steps    run 14ff83961826d440
schedule   3: exited:0       1493 steps    run 0fff53867c24e7fb
schedule   4: exited:0       1548 steps    run e0fbb2e637cd9ff9
schedule   5: exited:0       1510 steps    run c041cd02693e3e83
schedule   6: exited:0       1583 steps    run 7ead65b60277022a
schedule   7: exited:0       1486 steps    run 4819db0420500375
schedule   8: exited:2        819 steps    run accca49a740cd23d
schedule   9: exited:0       1535 steps    run 2cb8cf86211c538d
schedule  10: exited:0       1656 steps    run c575aab6ed9f4457
schedule  11: exited:0       1563 steps    run c4ffd94a26fe2346
schedule  12: exited:0       1564 steps    run 5a0cfcb43437ca38
schedule  13: exited:0       1527 steps    run dd1fd7f285444afd
schedule  14: exited:0       1502 steps    run 034d8dca75007cf5
schedule  15: exited:0       1542 steps    run 6ecca89d3759a5fa
schedule  16: exited:0       1524 steps    run 3d43429e62eca273

schedule 1 ends differently; narrowing the steps it perturbs
perturbing only steps 903..1375 still ends differently
step 1374 decides it: a timer 31.5 µs late there makes the run fail

passing: run 670449436209d833, schedule 1 over steps 903..1374
failing: run 4c79019e64a96855, schedule 1 over steps 903..1375
the two are the same run until step 1374

where ./tests/test_pool_shutdown first behaves differently:
  both           1357    39/46    write(1, "job 16 done: 5986\n")
  both           1358    39/46    write(1, "worker picked job 18\n")
  both           1370    39/47    write(1, "job 17 done: 43360\n")
  failing        1387    39/46    write(1, "job 18 done: 26744\n")
  failing        1390    39/46    SIGSEGV code=1 addr=0x108
  failing        1392    39/39    SIGSEGV code=0 addr=0x0
  failing        1395    39/46    thread exit(test_pool_shutd) killed:SIGSEGV
  passing        1379    39/47    write(1, "worker picked job 19\n")
  passing        1386    39/46    write(1, "job 18 done: 26744\n")
  passing        1387    39/46    write(1, "worker picked job 20\n")
  passing        1398    39/47    thread exit(test_pool_shutd) exited:0

open both in the desktop app: rewind open 4c79019e64a96855 1387 --compare 670449436209d833
```

`check` narrows schedule 1's failure to one step, 1374. The
passing run perturbs the same steps less that one, and only the failing run
gets a timer 31.5 µs late there. The last block shows where the test's own output
then differs. The search took 31 seconds. `--all` tries every
schedule:

```console
$ rewind check --all --root mylib.tar --cwd /src -- make check | grep 'ended differently'
12 of 64 perturbed schedules ended differently
```

## Look at the failure

```console
$ rewind log 4c79019e --steps | tail -4
      1370    39  job 17 done: 43360
      1387    39  job 18 done: 26744
      1404    35  Segmentation fault
      1408    34  make: *** [Makefile:18: check] Error 1

$ rewind events 4c79019e --from 1387 --to 1392
      1387    39/46    write(1, "job 18 done: 26744\n")
      1388     0/0     console "[    0.019294] test_pool_shutd[46]: segfault at 108 ip 0000562aad4b2412 sp 00007f78de197ea0 error 6 in test_pool_shutdown[1412,562aad4b2000+1000] likely on CPU 0 (core 0, socket 0)"
      1389     0/0     console "[    0.019299] Code: 5f c3 0f b7 d3 44 89 f6 48 8d 3d 48 0c 00 00 b8 00 00 00 00 e8 6f fc ff ff 48 8b 3d 90 2c 00 00 e8 c3 fc ff ff 49 8b 44 24 58 <83> 80 08 01 00 00 01 e9 c6 fe ff ff 55 53 48 83 ec 08 be 78 00 00"
      1390    39/46    SIGSEGV code=1 addr=0x108
      1392    39/39    SIGSEGV code=0 addr=0x0
```

The kernel's report marks the faulting instruction, `<83> 80 08 01 00 00 01`:
`addl $1, 0x108(%rax)`, which is `p->queue->completed++` with `p->queue`
null. The processes alive at the crash:

```console
$ rewind ps 4c79019e 1390
     1 /init
    34   make check
    35     /bin/sh -c for t in tests/test_pool_basic tests/test_pool_shutdown; do echo "running $t"; ./$t || exit 1; done
    39       ./tests/test_pool_shutdown
    46         (thread)
    47         (thread)
```

## Look inside the VM

`rewind cat`, `rewind shell` and `rewind gdb` work on a throwaway fork of the
run at a step. At the SIGSEGV, step 1390:

```console
$ rewind cat 4c79019e 1390 src/pool.c --pid 39 | sed -n '/^void pool_shutdown/,/^}/p'
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

$ printf 'pwd; ls; exit\n' | rewind shell 4c79019e 1390 --pid 39
rewind: a shell at step 1390 of 4c79019e64a96855; exit it to leave
[rewind] /src # pwd; ls; exit
/src
Containerfile  Makefile  libmylib.a  src  tests
```

`rewind gdb` loads the symbols of the VM's kernel and of the process running
at the step. Debian strips its libc; `DEBUGINFOD_URLS` names Debian's
debuginfod server, which has its symbols. From step 1387, thread
46's last write, continue to the line that faulted:

```console
$ DEBUGINFOD_URLS=https://debuginfod.debian.net rewind gdb 4c79019e 1387 -- -batch -ex 'break src/pool.c:77 if p->queue == 0' -ex continue -ex 'bt 3' -ex 'p p->queue' -ex list
rewind: step 1387 ran in process 39; loading symbols for 3 of its files
rewind: fetched 12 source files from the VM
rewind: gdb 16 cannot read thread-local variables, such as errno, at a step; gdb 17 or later can
rewind: gdb at step 1387 of 4c79019e64a96855
Downloading 3.97 M separate debug info for /home/fmzakari/.cache/rewind-record/home/gdb/1157178/newroot/usr/lib/x86_64-linux-gnu/libc.so.6...
Downloading 539.96 K separate debug info for /home/fmzakari/.cache/rewind-record/home/gdb/1157178/newroot/usr/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2...
arch_local_irq_restore (flags=514) at ./arch/x86/include/asm/irqflags.h:146
146		return !(flags & X86_EFLAGS_IF);
[Switching to thread 3 (Thread 1.46)]
Download failed: Invalid argument.  Continuing without source file ./io/../sysdeps/unix/sysv/linux/write.c.
#0  __GI___libc_write (nbytes=19, buf=0x7f78d8000b70, fd=1) at ../sysdeps/unix/sysv/linux/write.c:26
warning: 26	../sysdeps/unix/sysv/linux/write.c: No such file or directory
Breakpoint 1 at 0x562aad4b240d: file src/pool.c, line 77.

Thread 3 hit Breakpoint 1, worker (arg=0x562ad90e99c0) at src/pool.c:77
77				p->queue->completed++;
#0  worker (arg=0x562ad90e99c0) at src/pool.c:77
#1  0x00007f78dea261f5 in start_thread (arg=<optimized out>) at ./nptl/pthread_create.c:442
#2  0x00007f78deaa68ec in clone3 () at ../sysdeps/unix/sysv/linux/x86_64/clone3.S:81
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
$ rewind replay 4c79019e
identical: 396 events over 1417 steps

$ rewind replay 4c79019e --from 903
identical from the keyframe at step 512 to the end (0.45s)
```

`--from` starts at the nearest keyframe before the step. A run replays on any
machine with the same CPU vendor.

## Fork it

```console
$ rewind fork c181c81b 903 --schedule 1 --quiet
rewind: run 92783db3909b7765 exited:2 after 1421 steps, 0.019s virtual, 0.425s wall (poweroff)
rewind: the fork first differs from its parent at step 910
rewind: open it beside its parent in the desktop app: rewind open 92783db3909b7765 910 --compare c181c81b4c850544

$ rewind fork c181c81b 903 --schedule 2 --quiet
rewind: run 6d1f03a22d1e9f7c exited:2 after 1427 steps, 0.020s virtual, 0.417s wall (poweroff)
rewind: the fork first differs from its parent at step 930
rewind: open it beside its parent in the desktop app: rewind open 6d1f03a22d1e9f7c 930 --compare c181c81b4c850544

$ rewind fork c181c81b 903 --schedule 3 --quiet
rewind: run 71e063522bce3939 exited:0 after 1403 steps, 0.019s virtual, 0.419s wall (poweroff)
rewind: the fork first differs from its parent at step 910
rewind: open it beside its parent in the desktop app: rewind open 71e063522bce3939 910 --compare c181c81b4c850544

$ rewind fork c181c81b 903 --schedule 4 --quiet
rewind: run f5dfea0b2f2ea98f exited:0 after 1412 steps, 0.019s virtual, 0.417s wall (poweroff)
rewind: the fork first differs from its parent at step 910
rewind: open it beside its parent in the desktop app: rewind open f5dfea0b2f2ea98f 910 --compare c181c81b4c850544
```

From step 903 of the schedule 0 run, some schedules crash and
some pass.

## Scrub it in the app

The desktop app shows a run on a timeline: drag the playhead to any step for
the output, processes, files and event there, jump to the failure or to where
the run left the passing one, and fork from the playhead.

```console
$ rewind-app ~/.local/share/rewind/runs/4c79019e64a96855 --compare ~/.local/share/rewind/runs/670449436209d833
```

Press f to jump to the failure at step 1390, then s to open the
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
sha256:2fddecf334fa0f736090bbfe78b15cc2bf8ba544efc4fb8b899f9cea116473dc

$ rewind check --all --root mylib.tar --cwd /src -- make check
...
0 of 64 perturbed schedules ended differently
same result under all 65 schedules
```

Before the fix, 12 of the same 64 schedules crashed.

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
