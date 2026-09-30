# Tutorial: a flaky test in a container

This tutorial runs a test suite from a Docker image in Rewind VM, finds the
thread interleaving that breaks it, and replays the failure exactly. It needs
no Nix. [The Nix tutorial](tutorial-nix.md) covers the same bug as a Nix
derivation, and goes further into inspecting the failure.

The example is `mylib`, a small C thread pool in
[examples/mylib](../examples/mylib), with a shutdown race: `pool_shutdown`
frees the job queue before joining the workers, and a worker that has
finished a job counts it through the queue without holding the lock. On a
laptop its shutdown test fails about one run in nine.

## Install

You need x86_64 Linux with KVM, `mkfs.erofs` and Docker or Podman.

```console
$ curl -L https://github.com/fzakaria/rewind/releases/latest/download/rewind-0.1.0-x86_64-linux.tar.gz \
    | tar -xz -C ~/.local/opt
$ ln -s ~/.local/opt/rewind-0.1.0-x86_64-linux/bin/rewind ~/.local/bin/rewind
$ sudo apt install erofs-utils        # Debian and Ubuntu; dnf install erofs-utils on Fedora
$ ls -l /dev/kvm
crw-rw---- 1 root kvm 10, 232 Sep 28 09:40 /dev/kvm
$ sudo usermod -aG kvm $USER          # if /dev/kvm is not yours to use; log in again after
```

The tarball holds a static `rewind`, the guest kernel and the guest's
initramfs. Until the first release is published, build the same tarball with
`nix build github:fzakaria/rewind#release`. Runs are kept under `~/.local/share/rewind`; set `REWIND_HOME` to
keep them elsewhere.

## Build the image and export its filesystem

The example comes with a Containerfile that installs a compiler on Debian,
copies the source to `/src` and builds it:

```console
$ git clone https://github.com/fzakaria/rewind && cd rewind/examples/mylib
$ docker build -t mylib -f Containerfile .
$ docker export $(docker create mylib) -o mylib.tar
```

`rewind` runs a command in any root filesystem: a directory, or a tarball like
the one `docker export` writes. It converts the tarball once into a read-only
erofs image and caches it by the tarball's hash. The guest mounts it under a
writable overlay, so the command can write anywhere, and nothing it writes
reaches your disk.

## Run the tests

```console
$ rewind run --root mylib.tar --cwd /src -- make check
...
round 3: ok
test_pool_shutdown: ok
rewind: run 6d05c8a08d473976 exited:0 after 1146 steps, 0.017s virtual, 0.795s wall (poweroff)
```

It passes, and it passes every time with this root and this command: runs in
Rewind VM are deterministic, and the run's id is the hash of its inputs.

## Find a failing interleaving

```console
$ rewind check --root mylib.tar --cwd /src -- make check
schedule   0: exited:0             1146 steps    run 6d05c8a08d473976
schedule   1: exited:0             1140 steps    run e2117da115ed12a6
schedule   2: exited:2             1160 steps    run 364d5df2d1274b1b

schedule 2 ends differently; narrowing the steps it perturbs
perturbing only steps 409..1136 still ends differently

passing: run 6d05c8a08d473976
failing: run d4c5d3b0153ec15d

where ./tests/test_pool_shutdown first behaves differently:
  both         498    47/47    clone(CLONE_THREAD) = 49
  both         502    47/49    write(1, "worker picked job 0\n")
  both         506    47/48    write(1, "worker picked job 1\n")
  left         514    47/48    write(1, "job 1 done: 35269\n")
  left         515    47/48    write(1, "worker picked job 2\n")
  left         520    47/49    write(1, "job 0 done: 12727\n")
  left         521    47/49    write(1, "worker picked job 3\n")
  right        521    47/48    write(1, "job 0 done: 12727\n")
  right        522    47/48    write(1, "worker picked job 2\n")
  right        523    47/49    write(1, "job 1 done: 35269\n")
  right        527    47/49    write(1, "worker picked job 3\n")
```

`check` runs the command again under perturbed schedules: at some points the
guest kernel is asked to reschedule, and timers fire a little late, as timer
slack does on real hardware. The second perturbed schedule makes the test
crash. The last block shows where the test's own events first differ: in the
failing run the workers finish their first two jobs in the other order, and
the interleaving drifts from there until shutdown lands inside the unlocked
window.

This took 18 seconds. `--all` tries every schedule and reports a failure
rate:

```console
$ rewind check --all --schedules 32 --root mylib.tar --cwd /src -- make check | tail -1
10 of 32 perturbed schedules ended differently
```

## Look at it, and keep it

```console
$ rewind log d4c5d3b0 --steps | tail -3
$ rewind events d4c5d3b0 | grep -B3 SIGSEGV
$ rewind ps d4c5d3b0 --at <step of the SIGSEGV>
$ rewind replay d4c5d3b0
```

`replay` runs the failing run's inputs again and checks every event comes out
the same, at the same step. The failure is now a directory you can keep and bring back exactly, on the
machine that recorded it or one with the same CPU model; see
[Design](design.md#limits) for why the CPU matters.

To scrub it in the desktop app:

```console
$ rewind-app ~/.local/share/rewind/runs/d4c5d3b0153ec15d --compare ~/.local/share/rewind/runs/6d05c8a08d473976
```

## Check the fix

Fix `pool_shutdown` as [the Nix tutorial](tutorial-nix.md#fix-it-and-check-the-fix)
does, rebuild the image, export it, and check again:

```console
$ docker build -t mylib -f Containerfile . && docker export $(docker create mylib) -o mylib.tar
$ rewind check --all --root mylib.tar --cwd /src -- make check | tail -2
0 of 64 perturbed schedules ended differently
same result under all 65 schedules
```

## Limits worth knowing here

- The guest has one vCPU. Threads interleave, but never run at the same
  instant, so a data race between two plain loads and stores with no system
  call between them is out of reach. Races across a system call, a lock or a
  sleep, like this one, are in reach.
- A thread that computes for a long time without a system call is not
  preempted. A thread spinning on a flag without yielding stalls the guest.
- The container runs with no network.
