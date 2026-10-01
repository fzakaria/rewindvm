# Tutorial: a flaky test in a container

This tutorial runs a test suite from a Docker image in Rewind VM, finds the
thread interleaving that breaks it, and replays the failure exactly. It needs
no Nix. [The Nix tutorial](tutorial-nix.md) covers the same bug as a Nix
derivation, and goes further into inspecting the failure.

The example is `mylib`, a small C thread pool in
[examples/mylib](https://github.com/fzakaria/rewindvm/tree/main/examples/mylib).
It has a
shutdown race: `pool_shutdown` frees the job queue before joining the
workers, and a worker that has finished a job counts it through the queue
without holding the lock. Most runs of its shutdown test pass.

Every transcript below is real output from `rewind` on a 16 core AMD
laptop, with `sudo rewind pmu enable` run since boot.

## Install

You need x86_64 Linux with KVM, and Docker or Podman.

```console
$ mkdir -p ~/.local/opt ~/.local/bin
$ curl -L https://github.com/fzakaria/rewindvm/releases/latest/download/rewind-x86_64-linux.tar.gz | tar -xz -C ~/.local/opt
$ ln -s ~/.local/opt/rewind-x86_64-linux/bin/rewind ~/.local/bin/rewind
$ ls -l /dev/kvm
crw-rw-rw- 1 root kvm 10, 232 Oct  1 10:44 /dev/kvm
$ sudo usermod -aG kvm $USER          # if /dev/kvm is not yours to use; log in again after
```

The tarball, from the latest
[release](https://github.com/fzakaria/rewindvm/releases), holds a static
`rewind`, the VM's kernel and initramfs, and static `mkfs.erofs` and GNU tar
for turning root filesystems into images, so the host needs nothing else. Runs
are kept under `~/.local/share/rewind`; set `REWIND_HOME` to keep them
elsewhere.

The desktop app is a second tarball from the same release. Unpacked next to
the first, it uses that `rewind`:

```console
$ curl -L https://github.com/fzakaria/rewindvm/releases/latest/download/rewind-app-x86_64-linux.tar.gz | tar -xz -C ~/.local/opt
$ ln -s ~/.local/opt/rewind-app-x86_64-linux/bin/rewind-app ~/.local/bin/rewind-app
```

## Build the image and export its filesystem

Next to mylib's source is a Containerfile that installs a compiler on Debian,
copies the source to `/src` and builds it:

```console
$ git clone https://github.com/fzakaria/rewindvm
$ cd rewindvm/examples/mylib
$ docker build -t mylib -f Containerfile .
$ docker export $(docker create mylib) -o mylib.tar
```

`rewind` runs a command in any root filesystem: a directory, or a tarball like
the one `docker export` writes. It converts the tarball once into a read-only
erofs image and caches it by the tarball's hash. The VM mounts it under a
writable overlay, so the command can write anywhere, and nothing it writes
reaches your disk.

## Run the tests

```console
$ rewind run --root mylib.tar --cwd /src -- make check
...
round 3: ok
test_pool_shutdown: ok
rewind: run ddfd379b211e7343 exited:0 after 1317 steps, 0.019s virtual, 1.617s wall (poweroff)
```

On AMD, until `sudo rewind pmu enable` has been run since boot, `rewind` first
prints a warning that begins `rewind: recording with exit time: this AMD
CPU's branch counter is not exact until rr's workaround is set.` and records
with exit time, where the VM's clock does not follow the work done inside it.
[Counter time](pmu.md) explains the difference.

The tests pass, and they pass every time with this root and this command:
runs in Rewind VM are deterministic, and the run's id is the hash of its
inputs. Your image, and so your run ids, will differ from these.

## Find a failing interleaving

```console
$ rewind check --root mylib.tar --cwd /src -- make check
schedule   0: exited:0             1317 steps    run ddfd379b211e7343
schedule   1: exited:0             1524 steps    run d8d1e6a5490c3ae6
schedule   2: exited:0             1587 steps    run 45931e26f3075e54
schedule   3: exited:0             1473 steps    run 6082d794b2a4d1b9
schedule   4: exited:0             1617 steps    run e7df004e46e9c23d
schedule   5: exited:2             1018 steps    run 037fd4b7e6dd1997
schedule   6: exited:0             1563 steps    run 3543af096ed5f719
schedule   7: exited:0             1453 steps    run cd880a24e507c598
schedule   8: exited:0             1528 steps    run c300723a1c419324
schedule   9: exited:0             1521 steps    run fb5bddf7914a74f1
schedule  10: exited:0             1686 steps    run a82b92c436ebbf58
schedule  11: exited:0             1586 steps    run 5419f8b009a9aca9
schedule  12: exited:0             1631 steps    run 5f18b54051e031ae
schedule  13: exited:0             1522 steps    run 4010625af811f5c9
schedule  14: exited:0             1602 steps    run eccf48b0fbed4d7a
schedule  15: exited:0             1543 steps    run a3c092c0a5acbd08
schedule  16: exited:0             1516 steps    run c024f0a4ed3dcc30

schedule 5 ends differently; narrowing the steps it perturbs
perturbing only steps 586..690 still ends differently

passing: run ddfd379b211e7343
failing: run 589339f23d54ee71

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

`check` runs the command again under perturbed schedules, one machine per CPU
at a time: at some points the VM's kernel is asked to reschedule, and timers
fire a little late, as timer slack does on real hardware, and now and then a
task is stalled for a moment. Schedule 5 makes the test crash. `check` narrows
its perturbation to steps 586 to 690 and keeps that run. The last block shows
where the test's own events first differ: up to job 15 both runs agree; then
in the passing run the workers finish and exit, while in the failing run a
worker is still finishing job 16 when shutdown runs, and crashes right after.

This took 6 seconds. `--all` tries every schedule and reports a failure rate:

```console
$ rewind check --all --schedules 32 --root mylib.tar --cwd /src -- make check | grep 'ended differently'
3 of 32 perturbed schedules ended differently
```

## Look at it, and keep it

The failing run's last output, the processes alive at its SIGSEGV (step 1232,
which `rewind events 44cbffcc` shows), and a replay:

```console
$ rewind log 589339f2 --steps | tail -3
       685    39  worker picked job 17
       714    35  Segmentation fault
       720    34  make: *** [Makefile:18: check] Error 1

$ rewind ps 589339f2 --at 696
     1 /init
    34   make check
    35     /bin/sh -c for t in tests/test_pool_basic tests/test_pool_shutdown; do echo "running $t"; ./$t || exit 1; done
    39       ./tests/test_pool_shutdown
    40         (thread)
    41         (thread)

$ rewind replay 589339f2
identical: 280 events over 730 steps
```

`replay` runs the failing run's inputs again and checks every event comes out
the same, at the same step. The failure is now a directory you can keep and
bring back exactly. The VM sees a fixed x86-64-v3 CPU model, so a run replays
on other machines with the same CPU vendor: one recorded on AMD replays on AMD
from Zen 2 on, and not on Intel.

`rewind cat` and `rewind shell` look inside the VM at a step, in a throwaway
fork of the run, so nothing they do changes it. At the SIGSEGV, the source the
test was built from, and a shell in the test program's working directory:

```console
$ rewind cat 589339f2 696 src/pool.c --pid 39 | sed -n '/^void pool_shutdown/,/^}/p'
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

$ printf 'pwd; ls; exit\n' | rewind shell 589339f2 696 --pid 39
rewind: a shell at step 696 of 589339f23d54ee71; exit it to leave
[rewind] /src # pwd; ls; exit
/src
Containerfile  Makefile  libmylib.a  src  tests
```

To scrub it in the desktop app:

```console
$ rewind-app ~/.local/share/rewind/runs/44cbffccfb599fe3 --compare ~/.local/share/rewind/runs/62efc0dd26d542af
```

## Check the fix

Fix `pool_shutdown` in `src/pool.c` as
[the Nix tutorial](tutorial-nix.md#fix-it-and-check-the-fix) does, rebuild the
image, export it, and check again:

```console
$ docker build -t mylib -f Containerfile . && docker export $(docker create mylib) -o mylib.tar
$ rewind check --all --root mylib.tar --cwd /src -- make check | tail -2
0 of 64 perturbed schedules ended differently
same result under all 65 schedules
```

## Limits worth knowing here

- The VM has one vCPU. Threads interleave, but never run at the same
  instant, so a data race between two plain loads and stores with no system
  call between them is out of reach. Races across a system call, a lock or a
  sleep, like this one, are in reach.
- With exit time, a thread that computes for a long time without a system
  call is not preempted, and a thread spinning on a flag without yielding
  stalls the VM. [Counter time](pmu.md) explains why.
- The container runs with no network.

[Design](design.md#limits) has the full list of limits, and how the machine is made
deterministic.
