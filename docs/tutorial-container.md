# Tutorial: a flaky test in a container

This tutorial runs a test suite from a Docker image in Rewind VM, finds the
thread interleaving that breaks it, and replays the failure exactly. It needs
no Nix. [The Nix tutorial](tutorial-nix.md) covers the same bug as a Nix
derivation, and goes further into inspecting the failure.

The example is `mylib`, a small C thread pool from the
[example tarball](https://rewindvm.dev/download/mylib-example.tar.gz), whose
source is [examples/mylib](../examples/mylib) in this repository. It has a
shutdown race: `pool_shutdown` frees the job queue before joining the
workers, and a worker that has finished a job counts it through the queue
without holding the lock. Most runs of its shutdown test pass.

Every transcript below is real output from `rewind` 0.1.0 on a 16 core AMD
laptop, with `sudo rewind pmu enable` run since boot.

## Install

You need x86_64 Linux with KVM, and Docker or Podman.

```console
$ mkdir -p ~/.local/opt ~/.local/bin
$ curl -L https://rewindvm.dev/download/rewind-0.1.0-x86_64-linux.tar.gz | tar -xz -C ~/.local/opt
$ ln -s ~/.local/opt/rewind-0.1.0-x86_64-linux/bin/rewind ~/.local/bin/rewind
$ ls -l /dev/kvm
crw-rw-rw- 1 root kvm 10, 232 Sep 30 16:32 /dev/kvm
$ sudo usermod -aG kvm $USER          # if /dev/kvm is not yours to use; log in again after
```

The tarball holds a static `rewind`, the VM's kernel and initramfs, and static
`mkfs.erofs` and GNU tar for turning root filesystems into images, so the host
needs nothing else. Runs are kept under `~/.local/share/rewind`; set
`REWIND_HOME` to keep them elsewhere.

## Build the image and export its filesystem

The example tarball holds mylib's source, a Containerfile that installs a
compiler on Debian, copies the source to `/src` and builds it, and a flake for
the Nix tutorial:

```console
$ curl -L https://rewindvm.dev/download/mylib-example.tar.gz | tar -xz
$ cd mylib
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
rewind: run 79283455177422d7 exited:0 after 1305 steps, 0.019s virtual, 1.448s wall (poweroff)
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
schedule   0: exited:0             1305 steps    run 79283455177422d7
schedule   1: exited:0             1409 steps    run b9641e8d36ff0f51
schedule   2: exited:2             1452 steps    run d42556e3360a5a9e
schedule   3: exited:0             1416 steps    run e47b63db448c242a
schedule   4: exited:0             1425 steps    run f49c5fde2c94bb15
schedule   5: exited:0             1440 steps    run 6fbf8cf6d833ea44
schedule   6: exited:0             1420 steps    run f52b4ec6878c9832
schedule   7: exited:0             1435 steps    run 128275ec37380116
schedule   8: exited:2              747 steps    run c63e17bb3cd4685c
schedule   9: exited:0             1460 steps    run 97c9fd2b7b144fd2
schedule  10: exited:0             1447 steps    run 3432e7f1a4e2037f
schedule  11: exited:0             1436 steps    run 5178802e05c5904b
schedule  12: exited:0             1523 steps    run ca8cac3ece9fd6a8
schedule  13: exited:0             1469 steps    run d7bc5e7d18dfdf6e
schedule  14: exited:0             1505 steps    run a1150ba93cf84fda
schedule  15: exited:0             1462 steps    run b670f085515314f0
schedule  16: exited:0             1474 steps    run f78c66b9ff7d4c33

schedule 2 ends differently; narrowing the steps it perturbs
perturbing only steps 298..1403 still ends differently

passing: run 79283455177422d7
failing: run 789a563810bbe13c

where ./tests/test_pool_shutdown first behaves differently:
  both         507    38/39    write(1, "worker picked job 2\n")
  both         517    38/40    write(1, "job 1 done: 35269\n")
  both         518    38/40    write(1, "worker picked job 3\n")
  left         527    38/40    write(1, "job 3 done: 58758\n")
  left         528    38/40    write(1, "worker picked job 4\n")
  left         536    38/39    write(1, "job 2 done: 30213\n")
  left         537    38/39    write(1, "worker picked job 5\n")
  right        571    38/40    write(1, "job 2 done: 30213\n")
  right        572    38/40    write(1, "worker picked job 4\n")
  right        579    38/39    write(1, "job 3 done: 58758\n")
  right        580    38/39    write(1, "worker picked job 5\n")
```

`check` runs the command again under perturbed schedules, one machine per CPU
at a time: at some points the VM's kernel is asked to reschedule, and timers
fire a little late, as timer slack does on real hardware. Schedules 2 and 8
make the test crash. `check` narrows
schedule 2's perturbation to steps 298 to 1403 and keeps that run. The last
block shows where the test's own events first differ: in the failing run the
workers finish jobs 2 and 3 in the other order, and the interleaving drifts
from there until shutdown lands inside the unlocked window.

This took 6 seconds. `--all` tries every schedule and reports a failure rate:

```console
$ rewind check --all --schedules 32 --root mylib.tar --cwd /src -- make check | grep 'ended differently'
2 of 32 perturbed schedules ended differently
```

## Look at it, and keep it

The failing run's last output, the processes alive at its SIGSEGV (step 1407,
which `rewind events 789a5638` shows), and a replay:

```console
$ rewind log 789a5638 --steps | tail -3
      1402    38  job 16 done: 5986
      1426    34  Segmentation fault
      1430    33  make: *** [Makefile:18: check] Error 1

$ rewind ps 789a5638 --at 1407
     1 /init
    33   make check
    34     /bin/sh -c for t in tests/test_pool_basic tests/test_pool_shutdown; do echo "running $t"; ./$t || exit 1; done
    38       ./tests/test_pool_shutdown
    45         (thread)
    46         (thread)

$ rewind replay 789a5638
identical: 393 events over 1442 steps
```

`replay` runs the failing run's inputs again and checks every event comes out
the same, at the same step. The failure is now a directory you can keep and
bring back exactly. The VM sees a fixed x86-64-v3 CPU model, so a run replays
on other machines with the same CPU vendor: one recorded on AMD replays on AMD
from Zen 2 on, and not on Intel.

To scrub it in the desktop app:

```console
$ rewind-app ~/.local/share/rewind/runs/789a563810bbe13c --compare ~/.local/share/rewind/runs/79283455177422d7
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
