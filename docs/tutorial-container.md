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
rewind: run d3191482bbc8c650 exited:0 after 1317 steps, 0.019s virtual, 0.750s wall (poweroff)
```

It passes, in the same 1317 steps every time. Nothing the command
writes reaches your disk. A run's id is the hash of its inputs, so your image
makes ids other than these.

## Find a failing interleaving

`rewind check` runs it again under perturbed schedules, which reschedule the
VM's threads at different points, and stops at the first batch in which a run
ends differently:

```console
$ rewind check --root mylib.tar --cwd /src -- make check
schedule   0: exited:0             1317 steps    run d3191482bbc8c650
schedule   1: exited:0             1524 steps    run bd70a3c71d358163
schedule   2: exited:0             1587 steps    run ddbc14847a462992
schedule   3: exited:0             1473 steps    run f8fb58f9257c5148
schedule   4: exited:0             1617 steps    run 22951a1eff6c8bab
schedule   5: exited:2             1018 steps    run f6f4b3b347157461
schedule   6: exited:0             1563 steps    run f0da0d3ba4ec845f
schedule   7: exited:0             1453 steps    run a4ecf993914114c9
schedule   8: exited:0             1528 steps    run ea971585b54fd590
schedule   9: exited:0             1521 steps    run 7eeadcf0a5b2287f
schedule  10: exited:0             1686 steps    run decefab92a3320ea
schedule  11: exited:0             1586 steps    run 44fe5e786f9c06e0
schedule  12: exited:0             1631 steps    run 7e6d1da8bc073869
schedule  13: exited:0             1522 steps    run 67e7f6791fec1ed6
schedule  14: exited:0             1602 steps    run afe0cfd9d95ee8cc
schedule  15: exited:0             1543 steps    run c0325ee12494db82
schedule  16: exited:0             1516 steps    run 98d6ae2d5d78dcbc

schedule 5 ends differently; narrowing the steps it perturbs
perturbing only steps 586..690 still ends differently

passing: run d3191482bbc8c650
failing: run 1f4dbc7f616b856f

where ./tests/test_pool_shutdown first behaves differently:
  both         659    39/41    write(1, "job 14 done: 39906\n")
  both         660    39/41    write(1, "worker picked job 16\n")
  both         669    39/40    write(1, "job 15 done: 42559\n")
  left         670    39/40    write(1, "worker picked job 17\n")
  left         679    39/41    thread exit(test_pool_shutd) exited:0
  left         688    39/40    thread exit(test_pool_shutd) exited:0
  left         695    39/39    clone(CLONE_THREAD) = 42
  right        684    39/41    write(1, "job 16 done: 5986\n")
  right        685    39/41    write(1, "worker picked job 17\n")
  right        696    39/40    SIGSEGV code=1 addr=0x108
  right        698    39/39    SIGSEGV code=0 addr=0x0
```

`check` narrows schedule 5's perturbation to steps 586
to 690 and keeps the passing and failing runs. The last block
shows where the test's own output first differs. The search took
33 seconds. `--all` tries every schedule:

```console
$ rewind check --all --root mylib.tar --cwd /src -- make check | grep 'ended differently'
11 of 64 perturbed schedules ended differently
```

## Look at the failure

```console
$ rewind log 1f4dbc7f --steps | tail -4
       684    39  job 16 done: 5986
       685    39  worker picked job 17
       714    35  Segmentation fault
       720    34  make: *** [Makefile:18: check] Error 1

$ rewind events 1f4dbc7f --from 681 --to 698
       681    39/40    write(1, "job 15 done: 42559\n")
       684    39/41    write(1, "job 16 done: 5986\n")
       685    39/41    write(1, "worker picked job 17\n")
       694     0/0     console "[    0.013444] test_pool_shutd[40]: segfault at 108 ip 0000562ad6446412 sp 00007fcac0975ea0 error 6 in test_pool_shutdown[1412,562ad6446000+1000] likely on CPU 0 (core 0, socket 0)"
       695     0/0     console "[    0.013449] Code: 5f c3 0f b7 d3 44 89 f6 48 8d 3d 48 0c 00 00 b8 00 00 00 00 e8 6f fc ff ff 48 8b 3d 90 2c 00 00 e8 c3 fc ff ff 49 8b 44 24 58 <83> 80 08 01 00 00 01 e9 c6 fe ff ff 55 53 48 83 ec 08 be 78 00 00"
       696    39/40    SIGSEGV code=1 addr=0x108
       698    39/39    SIGSEGV code=0 addr=0x0
```

The kernel's report marks the faulting instruction, `<83> 80 08 01 00 00 01`:
`addl $1, 0x108(%rax)`, which is `p->queue->completed++` with `p->queue`
null. The processes alive at the crash:

```console
$ rewind ps 1f4dbc7f --at 696
     1 /init
    34   make check
    35     /bin/sh -c for t in tests/test_pool_basic tests/test_pool_shutdown; do echo "running $t"; ./$t || exit 1; done
    39       ./tests/test_pool_shutdown
    40         (thread)
    41         (thread)
```

## Look inside the VM

`rewind cat`, `rewind shell` and `rewind gdb` work on a throwaway fork of the
run at a step. At the SIGSEGV, step 696:

```console
$ rewind cat 1f4dbc7f 696 src/pool.c --pid 39 | sed -n '/^void pool_shutdown/,/^}/p'
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

$ printf 'pwd; ls; exit\n' | rewind shell 1f4dbc7f 696 --pid 39
rewind: a shell at step 696 of 1f4dbc7f616b856f; exit it to leave
[rewind] /src # pwd; ls; exit
/src
Containerfile  Makefile  libmylib.a  src  tests
```

`rewind gdb` loads the symbols of the VM's kernel and of the process running
at the step. Debian strips its libc; `DEBUGINFOD_URLS` names Debian's
debuginfod server, which has its symbols. From step 681, thread
40's last write, continue to the line that faulted:

```console
$ DEBUGINFOD_URLS=https://debuginfod.debian.net rewind gdb 1f4dbc7f 681 -- -batch -ex 'break src/pool.c:77 if p->queue == 0' -ex continue -ex 'bt 3' -ex 'p p->queue' -ex list
rewind: step 681 ran in process 39; loading symbols for 3 of its files
rewind: fetched 12 source files from the VM
rewind: gdb at step 681 of 1f4dbc7f616b856f
0xffffffff81285085 in __outl (value=<optimized out>, port=1504) at ./arch/x86/include/asm/shared/io.h:24
24	BUILDIO(l,  , u32)
Breakpoint 1 at 0x562ad644640d: file src/pool.c, line 77.

Thread 1 hit Breakpoint 1, worker (arg=0x562b0be702a0) at src/pool.c:77
77				p->queue->completed++;
#0  worker (arg=0x562b0be702a0) at src/pool.c:77
#1  0x00007fcac0a031f5 in start_thread (arg=<optimized out>) at ./nptl/pthread_create.c:442
#2  0x00007fcac0a838ec in clone3 () at ../sysdeps/unix/sysv/linux/x86_64/clone3.S:81
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
$ rewind replay 1f4dbc7f
identical: 280 events over 730 steps

$ rewind replay 1f4dbc7f --from 586
identical from the keyframe at step 512 to the end (0.42s)
```

`--from` starts at the nearest keyframe before the step. A run replays on any
machine with the same CPU vendor.

## Fork it

```console
$ rewind fork d3191482 586 --schedule 1 --quiet
rewind: run 58cbf04a2b0e3ff4 exited:2 after 971 steps, 0.016s virtual, 0.413s wall (poweroff)
rewind: the fork first differs from its parent at step 601

$ rewind fork d3191482 586 --schedule 2 --quiet
rewind: run 47beb53ee8ce54d2 exited:0 after 1443 steps, 0.021s virtual, 0.408s wall (poweroff)
rewind: the fork first differs from its parent at step 598

$ rewind fork d3191482 586 --schedule 3 --quiet
rewind: run 63711cf002eb6a45 exited:0 after 1626 steps, 0.022s virtual, 0.398s wall (poweroff)
rewind: the fork first differs from its parent at step 599

$ rewind fork d3191482 586 --schedule 4 --quiet
rewind: run f8a0e6725ffa4344 exited:0 after 1570 steps, 0.022s virtual, 0.410s wall (poweroff)
rewind: the fork first differs from its parent at step 600
```

From step 586 of the passing run, some schedules crash and some
pass.

## Scrub it in the app

The desktop app shows a run on a timeline: drag the playhead to any step for
the output, processes, files and event there, jump to the failure or to where
the run left the passing one, and fork from the playhead.

```console
$ rewind-app ~/.local/share/rewind/runs/1f4dbc7f616b856f --compare ~/.local/share/rewind/runs/d3191482bbc8c650
```

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
sha256:928a4019d08629600730cc6b2ca794a74612983aec7899f45f46b7e724bf9408

$ rewind check --all --root mylib.tar --cwd /src -- make check
...
0 of 64 perturbed schedules ended differently
same result under all 65 schedules
```

Before the fix, 11 of the same 64 schedules crashed.

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
