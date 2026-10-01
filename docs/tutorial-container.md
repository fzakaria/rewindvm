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

Every transcript below is real output from `rewind` 0.1.0 on a 16 core AMD
laptop, with `sudo rewind pmu enable` run since boot.

## Install

You need x86_64 Linux with KVM, and Docker or Podman.

```console
$ mkdir -p ~/.local/opt ~/.local/bin
$ curl -L https://github.com/fzakaria/rewindvm/releases/latest/download/rewind-x86_64-linux.tar.gz | tar -xz -C ~/.local/opt
$ ln -s ~/.local/opt/rewind-x86_64-linux/bin/rewind ~/.local/bin/rewind
$ ls -l /dev/kvm
crw-rw-rw- 1 root kvm 10, 232 Oct  1 01:55 /dev/kvm
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
rewind: run 62efc0dd26d542af exited:0 after 1310 steps, 0.019s virtual, 1.481s wall (poweroff)
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
schedule   0: exited:0             1310 steps    run 62efc0dd26d542af
schedule   1: exited:0             1499 steps    run 8e5fdee1af22b11f
schedule   2: exited:0             1480 steps    run 98427bba94b6c4ef
schedule   3: exited:0             1498 steps    run ff4c0a97968d3f50
schedule   4: exited:0             1465 steps    run 33e43b84affd801b
schedule   5: exited:2             1340 steps    run 1c5be68b74dafe5f
schedule   6: exited:0             1558 steps    run 4cf85f01e81e0871
schedule   7: exited:0             1478 steps    run 16858876d8a8e72b
schedule   8: exited:0             1508 steps    run b0ede991dca94cf9
schedule   9: exited:0             1553 steps    run 8a0ae9acb90c1a5a
schedule  10: exited:0             1579 steps    run 3d06b30ebc57e354
schedule  11: exited:0             1517 steps    run 40df6dd6a881509d
schedule  12: exited:0             1636 steps    run 73823df940d145f0
schedule  13: exited:0             1546 steps    run 068363c62f0cf272
schedule  14: exited:0             1542 steps    run ad9a5b81701e981f
schedule  15: exited:2             1241 steps    run c64db13005d0a584
schedule  16: exited:0             1518 steps    run d26fc576bf172c06

schedule 5 ends differently; narrowing the steps it perturbs
perturbing only steps 732..1212 still ends differently

passing: run 62efc0dd26d542af
failing: run 44cbffccfb599fe3

where ./tests/test_pool_shutdown first behaves differently:
  both         744    39/42    write(1, "worker picked job 5\n")
  both         754    39/43    write(1, "job 4 done: 3480\n")
  both         755    39/43    write(1, "worker picked job 6\n")
  left         765    39/42    write(1, "job 5 done: 45034\n")
  left         766    39/42    write(1, "worker picked job 7\n")
  left         773    39/43    write(1, "job 6 done: 13750\n")
  left         774    39/43    write(1, "worker picked job 8\n")
  right        767    39/43    write(1, "job 6 done: 13750\n")
  right        768    39/43    write(1, "worker picked job 7\n")
  right        772    39/42    write(1, "job 5 done: 45034\n")
  right        773    39/42    write(1, "worker picked job 8\n")
```

`check` runs the command again under perturbed schedules, one machine per CPU
at a time: at some points the VM's kernel is asked to reschedule, and timers
fire a little late, as timer slack does on real hardware. Schedules 5 and 15
make the test crash. `check` narrows schedule 5's perturbation to steps 732 to
1212 and keeps that run. The last block shows where the test's own events
first differ: in the failing run the workers finish jobs 5 and 6 in the other
order, and the interleaving drifts
from there until shutdown lands inside the unlocked window.

This took 6 seconds. `--all` tries every schedule and reports a failure rate:

```console
$ rewind check --all --schedules 32 --root mylib.tar --cwd /src -- make check | grep 'ended differently'
5 of 32 perturbed schedules ended differently
```

## Look at it, and keep it

The failing run's last output, the processes alive at its SIGSEGV (step 1232,
which `rewind events 44cbffcc` shows), and a replay:

```console
$ rewind log 44cbffcc --steps | tail -3
      1211    39  job 17 done: 43360
      1245    35  Segmentation fault
      1249    34  make: *** [Makefile:18: check] Error 1

$ rewind ps 44cbffcc --at 1232
     1 /init
    34   make check
    35     /bin/sh -c for t in tests/test_pool_basic tests/test_pool_shutdown; do echo "running $t"; ./$t || exit 1; done
    39       ./tests/test_pool_shutdown
    45         (thread)

$ rewind replay 44cbffcc
identical: 373 events over 1259 steps
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
$ rewind cat 44cbffcc 1232 src/pool.c --pid 39 | sed -n '/^void pool_shutdown/,/^}/p'
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

$ printf 'pwd; ls; exit\n' | rewind shell 44cbffcc 1232 --pid 39
rewind: a shell at step 1232 of 44cbffccfb599fe3; exit it to leave
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
