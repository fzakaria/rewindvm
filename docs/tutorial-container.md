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
crw-rw-rw- 1 root kvm 10, 232 Sep 30 21:30 /dev/kvm
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
rewind: run 7982e3de0d6f84bd exited:0 after 1304 steps, 0.019s virtual, 1.431s wall (poweroff)
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
schedule   0: exited:0             1304 steps    run 7982e3de0d6f84bd
schedule   1: exited:0             1408 steps    run b7e7fa5b7876bf70
schedule   2: exited:2             1447 steps    run 4bc54abc156bb6b2
schedule   3: exited:0             1415 steps    run 7f5b05d9ea9ceb9b
schedule   4: exited:0             1424 steps    run c6a4ae5f4442b7bc
schedule   5: exited:0             1439 steps    run fdc706f3e9d3e6ce
schedule   6: exited:0             1417 steps    run ed228b40ab998f2d
schedule   7: exited:0             1433 steps    run c887a714061a144a
schedule   8: exited:2              745 steps    run 4dfcf95a2b0a611d
schedule   9: exited:0             1462 steps    run 1a5510379612395e
schedule  10: exited:0             1447 steps    run 515ea5ca41685097
schedule  11: exited:0             1435 steps    run 92dfc176f104c28c
schedule  12: exited:0             1524 steps    run 7f8a91be9b2405fd
schedule  13: exited:0             1462 steps    run fd35116b5b4b0562
schedule  14: exited:0             1505 steps    run 9f826ecb58ee24c9
schedule  15: exited:0             1462 steps    run 069c2ad53f56cdee
schedule  16: exited:0             1473 steps    run a0d7fa2400622438

schedule 2 ends differently; narrowing the steps it perturbs
perturbing only steps 336..1403 still ends differently

passing: run 7982e3de0d6f84bd
failing: run ea772a3558ca02d5

where ./tests/test_pool_shutdown first behaves differently:
  both         480    38/38    clone(CLONE_THREAD) = 40
  both         487    38/39    write(1, "worker picked job 0\n")
  both         494    38/40    write(1, "worker picked job 1\n")
  left         506    38/39    write(1, "job 0 done: 12727\n")
  left         507    38/39    write(1, "worker picked job 2\n")
  left         517    38/40    write(1, "job 1 done: 35269\n")
  left         518    38/40    write(1, "worker picked job 3\n")
  right        533    38/39    write(1, "job 1 done: 35269\n")
  right        534    38/39    write(1, "worker picked job 2\n")
  right        548    38/40    write(1, "job 0 done: 12727\n")
  right        549    38/40    write(1, "worker picked job 3\n")
```

`check` runs the command again under perturbed schedules, one machine per CPU
at a time: at some points the VM's kernel is asked to reschedule, and timers
fire a little late, as timer slack does on real hardware. Schedules 2 and 8
make the test crash. `check` narrows schedule 2's perturbation to steps 336 to
1403 and keeps that run. The last block shows where the test's own events
first differ: in the failing run the workers finish their first jobs in the
other order, job 1 before job 0, and the interleaving drifts
from there until shutdown lands inside the unlocked window.

This took 6 seconds. `--all` tries every schedule and reports a failure rate:

```console
$ rewind check --all --schedules 32 --root mylib.tar --cwd /src -- make check | grep 'ended differently'
2 of 32 perturbed schedules ended differently
```

## Look at it, and keep it

The failing run's last output, the processes alive at its SIGSEGV (step 968,
which `rewind events ea772a35` shows), and a replay:

```console
$ rewind log ea772a35 --steps | tail -3
       959    38  job 17 done: 43360
       980    34  Segmentation fault
       984    33  make: *** [Makefile:18: check] Error 1

$ rewind ps ea772a35 --at 968
     1 /init
    33   make check
    34     /bin/sh -c for t in tests/test_pool_basic tests/test_pool_shutdown; do echo "running $t"; ./$t || exit 1; done
    38       ./tests/test_pool_shutdown
    42         (thread)

$ rewind replay ea772a35
identical: 313 events over 995 steps
```

`replay` runs the failing run's inputs again and checks every event comes out
the same, at the same step. The failure is now a directory you can keep and
bring back exactly. The VM sees a fixed x86-64-v3 CPU model, so a run replays
on other machines with the same CPU vendor: one recorded on AMD replays on AMD
from Zen 2 on, and not on Intel.

To scrub it in the desktop app:

```console
$ rewind-app ~/.local/share/rewind/runs/ea772a3558ca02d5 --compare ~/.local/share/rewind/runs/7982e3de0d6f84bd
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
