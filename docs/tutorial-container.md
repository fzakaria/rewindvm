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
rewind: run 7dc4851862d646ab exited:0 after 1791 steps, 0.019s virtual, 0.190s wall (poweroff)
```

It passes, in the same 1791 steps every time. Nothing the command
writes reaches your disk. A run's id is the hash of its inputs, so your image
makes ids other than these.

## Find a failing interleaving

`rewind check` runs it again under perturbed schedules, which reschedule and
stall the VM's threads at different points, and stops at the first batch in
which a run ends differently:

```console
$ rewind check --root mylib.tar --cwd /src -- make check
schedule   0: exited:0             1791 steps    run 7dc4851862d646ab
schedule   1: exited:2             1250 steps    run 8398a702ab30b45d
schedule   2: exited:0             1997 steps    run bd3682efaae28cc8
schedule   3: exited:0             2080 steps    run 09ef41e47177387f
schedule   4: exited:2             2016 steps    run 08901814ee097fb6
schedule   5: exited:0             2036 steps    run 375a78d30b6ff3b3
schedule   6: exited:0             2153 steps    run 30e46f6fc25a9557
schedule   7: exited:2             1256 steps    run c4e5ae1daa1618cd
schedule   8: exited:0             2084 steps    run b1bfe39b0ddba6ab
schedule   9: exited:2             1268 steps    run 29751e16b2e82c2c
schedule  10: exited:0             2004 steps    run 28e01b5a60cb887a
schedule  11: exited:0             2033 steps    run be1315ae3fc9aff0
schedule  12: exited:0             2003 steps    run d2879f174da56af2
schedule  13: exited:0             2023 steps    run 36806c924ee51d83
schedule  14: exited:0             2035 steps    run c988b488536c8e5c
schedule  15: exited:0             2026 steps    run c187b24ba632eb49
schedule  16: exited:0             2008 steps    run 17b3dd400f9479fb

schedule 1 ends differently from schedule 0; finding the step that decides it
schedule 1 limited to steps 727..1204 still ends differently
step 1203 decides it: a request to switch threads there makes the run fail

passing: run dc2de70380c0f0b6, schedule 1 over steps 727..1203
failing: run 4206706523693f4c, schedule 1 over steps 727..1204
the two are the same run until step 1203

where ./tests/test_pool_shutdown first behaves differently:
  both           1186    39/40    write(1, "job 15 done: 42559\n")
  both           1187    39/40    write(1, "worker picked job 17\n")
  both           1203    39/41    write(1, "job 16 done: 5986\n")
  failing        1207    39/40    write(1, "job 17 done: 43360\n")
  failing        1208    39/40    write(1, "worker picked job 18\n")
  failing        1218    39/41    SIGSEGV code=1 addr=0x108
  failing        1225    39/41    thread exit(test_pool_shutd) killed:SIGSEGV
  passing        1204    39/41    write(1, "worker picked job 18\n")
  passing        1210    39/40    write(1, "job 17 done: 43360\n")
  passing        1211    39/40    write(1, "worker picked job 19\n")
  passing        1224    39/41    thread exit(test_pool_shutd) exited:0

open both in the desktop app: rewind open 4206706523693f4c 1207 --compare dc2de70380c0f0b6
```

`check` narrows schedule 1's failure to one step, 1203. The
passing run perturbs the same steps less that one, and only the failing run
gets a request to switch threads there. The last block shows where the test's own output
then differs. The search took 4 seconds. `--all` tries every
schedule:

```console
$ rewind check --all --root mylib.tar --cwd /src -- make check | grep 'ended differently'
16 of 64 other schedules ended differently
```

## Look at the failure

```console
$ rewind log 42067065 --steps | tail -4
      1207    39  job 17 done: 43360
      1208    39  worker picked job 18
      1236    35  Segmentation fault
      1240    34  make: *** [Makefile:18: check] Error 1

$ rewind events 42067065 --from 1203 --to 1220
      1203    39/41    write(1, "job 16 done: 5986\n")
      1207    39/40    write(1, "job 17 done: 43360\n")
      1208    39/40    write(1, "worker picked job 18\n")
      1216     0/0     console "[    0.013780] test_pool_shutd[41]: segfault at 108 ip 000056058290e412 sp 00007f4874578ea0 error 6 in test_pool_shutdown[1412,56058290e000+1000] likely on CPU 0 (core 0, socket 0)"
      1217     0/0     console "[    0.013785] Code: 5f c3 0f b7 d3 44 89 f6 48 8d 3d 48 0c 00 00 b8 00 00 00 00 e8 6f fc ff ff 48 8b 3d 90 2c 00 00 e8 c3 fc ff ff 49 8b 44 24 58 <83> 80 08 01 00 00 01 e9 c6 fe ff ff 55 53 48 83 ec 08 be 78 00 00"
      1218    39/41    SIGSEGV code=1 addr=0x108
```

The kernel's report marks the faulting instruction, `<83> 80 08 01 00 00 01`:
`addl $1, 0x108(%rax)`, which is `p->queue->completed++` with `p->queue`
null. The processes alive at the crash:

```console
$ rewind ps 42067065 1218
     1 /init
    34   make check
    35     /bin/sh -c for t in tests/test_pool_basic tests/test_pool_shutdown; do echo "running $t"; ./$t || exit 1; done
    39       ./tests/test_pool_shutdown
    40         (thread)
    41         (thread)
```

## Look inside the VM

`rewind cat`, `rewind shell` and `rewind gdb` work on a throwaway fork of the
run at a step. At the SIGSEGV, step 1218:

```console
$ rewind cat 42067065 1218 src/pool.c --pid 39 | sed -n '/^void pool_shutdown/,/^}/p'
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

$ printf 'pwd; ls; exit\n' | rewind shell 42067065 1218 --pid 39
rewind: a shell at step 1218 of 4206706523693f4c; exit it to leave
[rewind] /src # pwd; ls; exit
/src
Containerfile  Makefile  libmylib.a  src  tests
```

`rewind gdb` loads the symbols of the VM's kernel and of the process running
at the step. Debian strips its libc; `DEBUGINFOD_URLS` names Debian's
debuginfod server, which has its symbols. From step 1203, thread
41's last write, continue to the line that faulted:

```console
$ DEBUGINFOD_URLS=https://debuginfod.debian.net rewind gdb 42067065 1203 -- -batch -ex 'break src/pool.c:77 if p->queue == 0' -ex continue -ex 'bt 3' -ex 'p p->queue' -ex list
rewind: step 1203 ran in process 39; loading symbols for 3 of its files
rewind: fetched 12 source files from the VM
rewind: gdb 16 cannot read thread-local variables, such as errno, at a step; gdb 17 or later can
rewind: gdb at step 1203 of 4206706523693f4c
Downloading 3.97 M separate debug info for /home/fmzakari/.cache/rewind-record/home/gdb/470540-0/newroot/usr/lib/x86_64-linux-gnu/libc.so.6...
Downloading 539.96 K separate debug info for /home/fmzakari/.cache/rewind-record/home/gdb/470540-0/newroot/usr/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2...
arch_local_irq_restore (flags=514) at ./arch/x86/include/asm/irqflags.h:146
146		return !(flags & X86_EFLAGS_IF);
[Switching to thread 4 (Thread 1.41)]
Download failed: Invalid argument.  Continuing without source file ./io/../sysdeps/unix/sysv/linux/write.c.
#0  __GI___libc_write (nbytes=18, buf=0x7f486c000b70, fd=1) at ../sysdeps/unix/sysv/linux/write.c:26
warning: 26	../sysdeps/unix/sysv/linux/write.c: No such file or directory
Breakpoint 1 at 0x56058290e40d: file src/pool.c, line 77.

Thread 4 hit Breakpoint 1, worker (arg=0x5605849f22a0) at src/pool.c:77
77				p->queue->completed++;
#0  worker (arg=0x5605849f22a0) at src/pool.c:77
#1  0x00007f4874e071f5 in start_thread (arg=<optimized out>) at ./nptl/pthread_create.c:442
#2  0x00007f4874e878ec in clone3 () at ../sysdeps/unix/sysv/linux/x86_64/clone3.S:81
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
$ rewind replay 42067065
identical to the recording: 272 events, 563 context switches and 13 read and write records over 1249 steps, and the whole machine at its 4 keyframes

$ rewind replay 42067065 --from 727
identical to the recording from the keyframe at step 256 to the end, and the whole machine at its 3 keyframes (0.12s)
```

`--from` starts at the nearest keyframe before the step. A run replays on any
machine with the same CPU vendor that has the features of the run's CPU model.

## Fork it

```console
$ rewind fork 7dc48518 727 --schedule 1 --quiet
rewind: run 28a172a196bdb74d exited:2 after 1250 steps, 0.014s virtual, 0.056s wall (poweroff)
rewind: the fork first differs from its parent at step 743
rewind: open it beside its parent in the desktop app: rewind open 28a172a196bdb74d 743 --compare 7dc4851862d646ab

$ rewind fork 7dc48518 727 --schedule 2 --quiet
rewind: run 717b2a4f88b1b212 exited:0 after 1998 steps, 0.021s virtual, 0.064s wall (poweroff)
rewind: the fork first differs from its parent at step 741
rewind: open it beside its parent in the desktop app: rewind open 717b2a4f88b1b212 741 --compare 7dc4851862d646ab

$ rewind fork 7dc48518 727 --schedule 3 --quiet
rewind: run dbcd8da65a6bc28a exited:0 after 2070 steps, 0.023s virtual, 0.065s wall (poweroff)
rewind: the fork first differs from its parent at step 743
rewind: open it beside its parent in the desktop app: rewind open dbcd8da65a6bc28a 743 --compare 7dc4851862d646ab

$ rewind fork 7dc48518 727 --schedule 4 --quiet
rewind: run 4142badad3603662 exited:0 after 2019 steps, 0.022s virtual, 0.065s wall (poweroff)
rewind: the fork first differs from its parent at step 741
rewind: open it beside its parent in the desktop app: rewind open 4142badad3603662 741 --compare 7dc4851862d646ab
```

From step 727 of the schedule 0 run, some schedules crash and
some pass.

## Scrub it in the app

The desktop app shows a run on a timeline: drag the playhead to any step for
the output, processes, files and event there, jump to the failure or to where
the run left the passing one, and fork from the playhead.

```console
$ rewind-app ~/.local/share/rewind/runs/4206706523693f4c --compare ~/.local/share/rewind/runs/dc2de70380c0f0b6
```

Press f to jump to the failure at step 1218, then s to open the
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
sha256:819b4c91e732003eecf2bdfee3c280ff70a2b0d5163c767e73c264823351e59b

$ rewind check --all --root mylib.tar --cwd /src -- make check
...
0 of 64 other schedules ended differently
same result under all 65 schedules
```

Before the fix, 16 of the same 64 schedules crashed.

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
