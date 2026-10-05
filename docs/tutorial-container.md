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
rewind: run bc2402a8d8e5a1fe exited:0 after 1323 steps, 0.019s virtual, 1.038s wall (poweroff)
```

It passes, in the same 1323 steps every time. Nothing the command
writes reaches your disk. A run's id is the hash of its inputs, so your image
makes ids other than these.

## Find a failing interleaving

`rewind check` runs it again under perturbed schedules, which reschedule the
VM's threads at different points, and stops at the first batch in which a run
ends differently:

```console
$ rewind check --root mylib.tar --cwd /src -- make check
schedule   0: exited:0             1323 steps    run bc2402a8d8e5a1fe
schedule   1: exited:0             1492 steps    run ab8478b00115bf49
schedule   2: exited:2             1619 steps    run c90fbc995ca1eb21
schedule   3: exited:0             1494 steps    run de040b96cd9a954b
schedule   4: exited:2             1548 steps    run 8a0a736c34787703
schedule   5: exited:0             1568 steps    run 783148264e5b47b3
schedule   6: exited:0             1550 steps    run cbfb6a8f65db2218
schedule   7: exited:0             1463 steps    run 370697af0ed186f4
schedule   8: exited:0             1540 steps    run e65d3d0f056aa438
schedule   9: exited:0             1590 steps    run 2aed8e423055064e
schedule  10: exited:2             1309 steps    run e4bc5a8c4456f50f
schedule  11: exited:0             1549 steps    run 58117a6d3dba97a7
schedule  12: exited:0             1489 steps    run 6cb3181b17735049
schedule  13: exited:2             1309 steps    run d11b498b0f90ffc1
schedule  14: exited:0             1523 steps    run ed25eb3c8b25a7a9
schedule  15: exited:0             1576 steps    run 144089287f1d7825
schedule  16: exited:2              789 steps    run a1941821721a3ced

schedule 2 ends differently; narrowing the steps it perturbs
perturbing only steps 367..1485 still ends differently

passing: run bc2402a8d8e5a1fe
failing: run 801778e543ae46f1

where ./tests/test_pool_shutdown first behaves differently:
  both         528    39/40    write(1, "worker picked job 2\n")
  both         532    39/41    write(1, "job 1 done: 35269\n")
  both         533    39/41    write(1, "worker picked job 3\n")
  left         544    39/41    write(1, "job 3 done: 58758\n")
  left         545    39/41    write(1, "worker picked job 4\n")
  left         555    39/40    write(1, "job 2 done: 30213\n")
  left         556    39/40    write(1, "worker picked job 5\n")
  right        597    39/41    write(1, "job 2 done: 30213\n")
  right        598    39/41    write(1, "worker picked job 4\n")
  right        605    39/40    write(1, "job 3 done: 58758\n")
  right        606    39/40    write(1, "worker picked job 5\n")
```

`check` narrows schedule 2's perturbation to steps 367
to 1485 and keeps the passing and failing runs. The last block
shows where the test's own output first differs. The search took
48 seconds. `--all` tries every schedule:

```console
$ rewind check --all --root mylib.tar --cwd /src -- make check | grep 'ended differently'
15 of 64 perturbed schedules ended differently
```

## Look at the failure

```console
$ rewind log 801778e5 --steps | tail -4
      1474    39  worker picked job 18
      1486    39  job 17 done: 43360
      1512    35  Segmentation fault
      1517    34  make: *** [Makefile:18: check] Error 1

$ rewind events 801778e5 --from 1486 --to 1503
      1486    39/46    write(1, "job 17 done: 43360\n")
      1497    39/47    thread exit(test_pool_shutd) exited:0
      1499     0/0     console "[    0.020375] test_pool_shutd[46]: segfault at 108 ip 0000563c5bf3c412 sp 00007fa0cca96ea0 error 6 in test_pool_shutdown[1412,563c5bf3c000+1000] likely on CPU 0 (core 0, socket 0)"
      1500     0/0     console "[    0.020380] Code: 5f c3 0f b7 d3 44 89 f6 48 8d 3d 48 0c 00 00 b8 00 00 00 00 e8 6f fc ff ff 48 8b 3d 90 2c 00 00 e8 c3 fc ff ff 49 8b 44 24 58 <83> 80 08 01 00 00 01 e9 c6 fe ff ff 55 53 48 83 ec 08 be 78 00 00"
      1501    39/46    SIGSEGV code=1 addr=0x108
      1503    39/39    SIGSEGV code=0 addr=0x0
```

The kernel's report marks the faulting instruction, `<83> 80 08 01 00 00 01`:
`addl $1, 0x108(%rax)`, which is `p->queue->completed++` with `p->queue`
null. The processes alive at the crash:

```console
$ rewind ps 801778e5 1501
     1 /init
    34   make check
    35     /bin/sh -c for t in tests/test_pool_basic tests/test_pool_shutdown; do echo "running $t"; ./$t || exit 1; done
    39       ./tests/test_pool_shutdown
    46         (thread)
```

## Look inside the VM

`rewind cat`, `rewind shell` and `rewind gdb` work on a throwaway fork of the
run at a step. At the SIGSEGV, step 1501:

```console
$ rewind cat 801778e5 1501 src/pool.c --pid 39 | sed -n '/^void pool_shutdown/,/^}/p'
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

$ printf 'pwd; ls; exit\n' | rewind shell 801778e5 1501 --pid 39
rewind: a shell at step 1501 of 801778e543ae46f1; exit it to leave
[rewind] /src # pwd; ls; exit
/src
Containerfile  Makefile  libmylib.a  src  tests
```

`rewind gdb` loads the symbols of the VM's kernel and of the process running
at the step. Debian strips its libc; `DEBUGINFOD_URLS` names Debian's
debuginfod server, which has its symbols. From step 1486, thread
46's last write, continue to the line that faulted:

```console
$ DEBUGINFOD_URLS=https://debuginfod.debian.net rewind gdb 801778e5 1486 -- -batch -ex 'break src/pool.c:77 if p->queue == 0' -ex continue -ex 'bt 3' -ex 'p p->queue' -ex list
rewind: step 1486 ran in process 39; loading symbols for 3 of its files
rewind: fetched 12 source files from the VM
rewind: gdb at step 1486 of 801778e543ae46f1
0xffffffff81285085 in __outl (value=<optimized out>, port=1504) at ./arch/x86/include/asm/shared/io.h:24
24	BUILDIO(l,  , u32)
Breakpoint 1 at 0x563c5bf3c40d: file src/pool.c, line 77.
[Thread 1.47 exited]

Thread 1 hit Breakpoint 1, worker (arg=0x563c67cab9c0) at src/pool.c:77
77				p->queue->completed++;
#0  worker (arg=0x563c67cab9c0) at src/pool.c:77
#1  0x00007fa0cd3251f5 in start_thread (arg=<optimized out>) at ./nptl/pthread_create.c:442
#2  0x00007fa0cd3a58ec in clone3 () at ../sysdeps/unix/sysv/linux/x86_64/clone3.S:81
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

Images built on Fedora, Ubuntu or Arch have debuginfod servers of their own,
listed by [elfutils](https://sourceware.org/elfutils/Debuginfod.html).

## Replay it

```console
$ rewind replay 801778e5
identical: 403 events over 1527 steps

$ rewind replay 801778e5 --from 367
identical from the keyframe at step 256 to the end (0.41s)
```

`--from` starts at the nearest keyframe before the step. A run replays on any
machine with the same CPU vendor.

## Fork it

```console
$ rewind fork bc2402a8 367 --schedule 1 --quiet
rewind: run 8eba05ee99a5ab04 exited:0 after 1448 steps, 0.020s virtual, 0.411s wall (poweroff)
rewind: the fork first differs from its parent at step 382

$ rewind fork bc2402a8 367 --schedule 2 --quiet
rewind: run c37cff3b49b64294 exited:2 after 1531 steps, 0.021s virtual, 0.431s wall (poweroff)
rewind: the fork first differs from its parent at step 384

$ rewind fork bc2402a8 367 --schedule 3 --quiet
rewind: run 3923ba2ee77bcc3c exited:0 after 1499 steps, 0.021s virtual, 0.418s wall (poweroff)
rewind: the fork first differs from its parent at step 384

$ rewind fork bc2402a8 367 --schedule 4 --quiet
rewind: run 696a19c6f7aae504 exited:2 after 1083 steps, 0.017s virtual, 0.413s wall (poweroff)
rewind: the fork first differs from its parent at step 385
```

From step 367 of the passing run, some schedules crash and some
pass.

## Scrub it in the app

The desktop app shows a run on a timeline: drag the playhead to any step for
the output, processes, files and event there, jump to the failure or to where
the run left the passing one, and fork from the playhead.

```console
$ rewind-app ~/.local/share/rewind/runs/801778e543ae46f1 --compare ~/.local/share/rewind/runs/bc2402a8d8e5a1fe
```

Press f to jump to the failure at step 1501, then s to open the
source panel. After a few seconds it shows `worker` at `src/pool.c:77`, with
`p->queue->completed++;` marked: the line that read the queue after
`pool_shutdown` had set it to NULL.

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
sha256:6d659ced47b9b140c8bf75a998b429b92f67d406805ba9e667091a508f9bebcf

$ rewind check --all --root mylib.tar --cwd /src -- make check
...
0 of 64 perturbed schedules ended differently
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
- [The advanced tutorial](tutorial-advanced.md): watchpoints, the kernel's
  side of a crash, tools inside the VM, more CPUs, and sharing a run.
- [Counter time](pmu.md): how the VM's clock follows its work.
