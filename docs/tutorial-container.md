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
laptop.

## Install

You need x86_64 Linux with KVM, and Docker or Podman.

```console
$ mkdir -p ~/.local/opt ~/.local/bin
$ curl -L https://rewindvm.dev/download/rewind-0.1.0-x86_64-linux.tar.gz | tar -xz -C ~/.local/opt
$ ln -s ~/.local/opt/rewind-0.1.0-x86_64-linux/bin/rewind ~/.local/bin/rewind
$ ls -l /dev/kvm
crw-rw-rw- 1 root kvm 10, 232 Sep 30 15:38 /dev/kvm
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
rewind: recording with exit time: this AMD CPU's branch counter is not exact until rr's workaround is set. Computation will not move the VM's clock, and a thread that computes without system calls is not preempted. Fix it with `sudo rewind pmu enable` (until reboot), then `rewind pmu status`. Why: https://rewindvm.dev/counter-time.html
...
round 3: ok
test_pool_shutdown: ok
rewind: run 28ce179fd23930e4 exited:0 after 1361 steps, 0.018s virtual, 1.363s wall (poweroff)
```

The first line is there because this laptop's AMD branch counter is not exact,
so the VM's clock moves only at exits; [Counter time](pmu.md) explains what
that changes and how to fix it, and the transcripts below leave the line out.

The tests pass, and they pass every time with this root and this command:
runs in Rewind VM are deterministic, and the run's id is the hash of its
inputs. Your image, and so your run ids, will differ from these.

## Find a failing interleaving

```console
$ rewind check --root mylib.tar --cwd /src -- make check
schedule   0: exited:0             1361 steps    run 28ce179fd23930e4
schedule   1: exited:0             1508 steps    run 7f3a578275a0fc8b
schedule   2: exited:0             1522 steps    run 9e2c600973c4b6d5
schedule   3: exited:2             1022 steps    run 622f538b82beff8a
schedule   4: exited:2              773 steps    run ec6ddf5bf37d2ba3
schedule   5: exited:2              987 steps    run 77961e97dc3fc005
schedule   6: exited:0             1484 steps    run 1474c5c7dfb4f4e2
schedule   7: exited:0             1495 steps    run 8c1d519e59569cf8
schedule   8: exited:0             1441 steps    run f14ce2c3e58683ad
schedule   9: exited:0             1471 steps    run 497396b1c64da60b
schedule  10: exited:0             1505 steps    run 64abcf444d9c27c7
schedule  11: exited:2             1458 steps    run 13fb38e56cad2d3f
schedule  12: exited:2             1231 steps    run 9da1bb2998aae284
schedule  13: exited:0             1456 steps    run 668a9b5916b7af00
schedule  14: exited:0             1500 steps    run 59379d55ae78e2aa
schedule  15: exited:0             1494 steps    run d6f1e55c173e1421
schedule  16: exited:0             1492 steps    run a1acab0a27d446d0

schedule 3 ends differently; narrowing the steps it perturbs
perturbing only steps 541..976 still ends differently

passing: run 28ce179fd23930e4
failing: run f225481037a971dd

where ./tests/test_pool_shutdown first behaves differently:
  both         551    38/40    write(1, "worker picked job 6\n")
  both         554    38/39    write(1, "job 5 done: 45034\n")
  both         555    38/39    write(1, "worker picked job 7\n")
  left         566    38/39    write(1, "job 7 done: 5785\n")
  left         567    38/39    write(1, "worker picked job 8\n")
  left         575    38/40    write(1, "job 6 done: 13750\n")
  left         576    38/40    write(1, "worker picked job 9\n")
  right        567    38/40    write(1, "job 6 done: 13750\n")
  right        568    38/40    write(1, "worker picked job 8\n")
  right        577    38/40    write(1, "job 8 done: 21929\n")
  right        578    38/40    write(1, "worker picked job 9\n")
```

`check` runs the command again under perturbed schedules, one machine per CPU
at a time: at some points the VM's kernel is asked to reschedule, and timers
fire a little late, as timer slack does on real hardware. Schedule 3 is the
first to make the test crash, and 5 of the first 16 do. `check` narrows
schedule 3's perturbation to steps 541 to 976 and keeps that run. The last
block shows where the test's own events first differ: in the failing run the
workers finish jobs 6 and 7 in the other order, and the interleaving drifts
from there until shutdown lands inside the unlocked window.

This took 6 seconds. `--all` tries every schedule and reports a failure rate:

```console
$ rewind check --all --schedules 32 --root mylib.tar --cwd /src -- make check | grep 'ended differently'
11 of 32 perturbed schedules ended differently
```

## Look at it, and keep it

The failing run's last output, the processes alive at its SIGSEGV (step 980,
which `rewind events f2254810` shows), and a replay:

```console
$ rewind log f2254810 --steps | tail -3
       975    38  job 17 done: 43360
       997    34  Segmentation fault
      1001    33  make: *** [Makefile:18: check] Error 1

$ rewind ps f2254810 --at 980
     1 /init
    33   make check
    34     /bin/sh -c for t in tests/test_pool_basic tests/test_pool_shutdown; do echo "running $t"; ./$t || exit 1; done
    38       ./tests/test_pool_shutdown
    41         (thread)
    42         (thread)

$ rewind replay f2254810
identical: 324 events over 1011 steps
```

`replay` runs the failing run's inputs again and checks every event comes out
the same, at the same step. The failure is now a directory you can keep and
bring back exactly. The VM sees a fixed x86-64-v3 CPU model, so a run replays
on other machines with the same CPU vendor: one recorded on AMD replays on AMD
from Zen 2 on, and not on Intel.

To scrub it in the desktop app:

```console
$ rewind-app ~/.local/share/rewind/runs/f225481037a971dd --compare ~/.local/share/rewind/runs/28ce179fd23930e4
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
