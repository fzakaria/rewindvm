# Tutorial: a flaky Nix build

This tutorial takes a derivation whose test suite fails now and then, makes
Rewind VM find a failing run in under a minute, looks at the failure step by
step, and checks the fix across 64 thread interleavings.

The derivation is `mylib`, a small C thread pool, from the
[example tarball](https://rewindvm.dev/download/mylib-example.tar.gz). The
tarball is a flake, so its URL works anywhere Nix takes a flake reference. Its
source is [examples/mylib](../examples/mylib) in this repository. Its
`pool_shutdown` frees the job queue before joining the workers, and a worker
that has finished a job checks whether the pool is stopping without holding
the lock, then counts the job through the queue. When shutdown runs between
that check and the count, the worker writes through a freed, nulled pointer.

Every transcript below is real output from `rewind` 0.1.0 on a 16 core AMD
laptop, with `sudo rewind pmu enable` run since boot.

## Install

You need x86_64 Linux with KVM and Nix with flakes enabled. The release
tarball is a flake too:

```console
$ nix profile install https://rewindvm.dev/download/rewind-0.1.0-x86_64-linux.tar.gz
$ ls -l /dev/kvm
crw-rw-rw- 1 root kvm 10, 232 Sep 30 16:30 /dev/kvm
```

To try `rewind` without installing it, put
`nix run https://rewindvm.dev/download/rewind-0.1.0-x86_64-linux.tar.gz --` in
front of its arguments. On NixOS, add the tarball as a flake input and turn on
its module instead:

```nix
inputs.rewind.url = "https://rewindvm.dev/download/rewind-0.1.0-x86_64-linux.tar.gz";

# in your configuration, with inputs.rewind.nixosModules.default imported
programs.rewind.enable = true;
# AMD only: make the branch counter exact at every boot
programs.rewind.amdBranchCounterWorkaround = true;
```

If `/dev/kvm` is not readable and writable by you, add yourself to the `kvm`
group. On NixOS that is `users.users.<you>.extraGroups = [ "kvm" ];`.

Runs are kept under `~/.local/share/rewind`. Set `REWIND_HOME` to keep them
somewhere else.

## The flaky build

On the host, `nix build https://rewindvm.dev/download/mylib-example.tar.gz`
usually succeeds. Rebuilt 45 times on the laptop, it failed 7 times, each in
`checkPhase`:

```console
$ nix build --rebuild -L https://rewindvm.dev/download/mylib-example.tar.gz
...
mylib> running tests/test_pool_shutdown
...
mylib> job 15 done: 42559
mylib> worker picked job 17
mylib> job 16 done: 5986
mylib> /nix/store/...-bash-5.3p15/bin/bash: line 1:   133 Segmentation fault         (core dumped) ./$t
mylib> make: *** [Makefile:18: check] Error 1
error: Cannot build '/nix/store/hvp2d0h9l97d19d3xp5k3vf6xwhg4axr-mylib-0.3.0.drv'.
```

Nix reports the derivation as failed and moves on. Running it again usually
passes, so there is nothing left to look at.

## Build it in Rewind VM

`rewind nix` builds a derivation the way the Nix sandbox would, inside a
deterministic virtual machine:

```console
$ rewind nix https://rewindvm.dev/download/mylib-example.tar.gz
rewind: packing 62 store paths for mylib-0.3.0
...
rewind: run 68bba6bee6c22725 exited:0 after 6161 steps, 0.216s virtual, 2.947s wall (poweroff)
/nix/store/f6a9gy362szw6nxx3ikrklr8glr6rdln-mylib-0.3.0 aa30ea54dc47c30f (same as the host's build)
```

The run used counter time: the VM's clock follows the work done inside it. On
AMD, until `sudo rewind pmu enable` has been run since boot, `rewind` instead
prints a warning that begins `rewind: recording with exit time: this AMD
CPU's branch counter is not exact until rr's workaround is set.` and records
with exit time. [Counter time](pmu.md) explains the difference.

This build passes, and it passes every time: the same inputs make the same
run, down to the same 6161 steps. A step is one exit from the VM to Rewind,
and the step count is the run's clock. The last line is the hash of the output
tree. The host built the same derivation above, so `rewind nix` compares the
two outputs, and they are the same.

## Find a failing interleaving

Determinism means the unperturbed build will never show the bug. `rewind
check` builds the derivation again under perturbed schedules. Each schedule
asks the VM's kernel to reschedule at different points and lets timers fire a
little late, as timer slack does on real hardware. It runs one machine per CPU
at a time and stops after the first batch in which a build ends differently:

```console
$ rewind check https://rewindvm.dev/download/mylib-example.tar.gz
schedule   0: exited:0             6161 steps  aa30ea54dc47  run 68bba6bee6c22725
schedule   1: exited:0             6653 steps  aa30ea54dc47  run 5468d52c22cc894e
schedule   2: exited:0             6787 steps  aa30ea54dc47  run 80c46efa4ae2afdb
schedule   3: exited:0             6663 steps  aa30ea54dc47  run 4a4c6b1b9f6de070
schedule   4: exited:0             6763 steps  aa30ea54dc47  run ec53fc8957976a55
schedule   5: exited:0             6755 steps  aa30ea54dc47  run 2ecf6315b550f31e
schedule   6: exited:0             6671 steps  aa30ea54dc47  run 18bbe00e55fb3a74
schedule   7: exited:0             6705 steps  aa30ea54dc47  run 90a07dd3668ffda2
schedule   8: exited:0             6771 steps  aa30ea54dc47  run 2a217b80e72abb20
schedule   9: exited:0             6684 steps  aa30ea54dc47  run 0eecd17a07486c70
schedule  10: exited:0             6684 steps  aa30ea54dc47  run 833ff11470c716f9
schedule  11: exited:2             4810 steps    run 86d440db9febce86
schedule  12: exited:0             6759 steps  aa30ea54dc47  run 5a36bbf168c82b31
schedule  13: exited:0             6594 steps  aa30ea54dc47  run 5ab6c57821d36aef
schedule  14: exited:0             6802 steps  aa30ea54dc47  run 19cb026ef559fff9
schedule  15: exited:0             6810 steps  aa30ea54dc47  run 79b5a51ea938c535
schedule  16: exited:2             4863 steps    run 45b0091dfaf2345d

schedule 11 ends differently; narrowing the steps it perturbs
perturbing only steps 3821..4763 still ends differently

passing: run 68bba6bee6c22725
failing: run c4b6cd3b789285ea

where ./tests/test_pool_shutdown first behaves differently:
  both        4110   165/165   clone(CLONE_THREAD) = 167
  both        4117   165/166   write(1, "worker picked job 0\n")
  both        4124   165/167   write(1, "worker picked job 1\n")
  left        4134   165/166   write(1, "job 0 done: 12727\n")
  left        4135   165/166   write(1, "worker picked job 2\n")
  left        4145   165/167   write(1, "job 1 done: 35269\n")
  left        4146   165/167   write(1, "worker picked job 3\n")
  right       4173   165/166   write(1, "job 1 done: 35269\n")
  right       4174   165/166   write(1, "worker picked job 2\n")
  right       4184   165/167   write(1, "job 0 done: 12727\n")
  right       4185   165/167   write(1, "worker picked job 3\n")
```

Schedules 11 and 16 fail. `check` then narrows schedule 11's perturbation to
the smallest window that still changes the outcome, here steps 3821 to 4763.
It keeps two runs: the unperturbed one and the failing one with the narrowed
window. The two are identical up to step 3821.

The last block compares only the failing program's own events. In the failing
run the two workers finish their first jobs in the other order, job 1 before
job 0, and the interleaving drifts from there until shutdown lands between a
worker's check of the pool and its count of the job.

The whole search took 13 seconds. `rewind check --all` tries every schedule
and says how many failed, which measures how flaky a build is:

```console
$ rewind check --all https://rewindvm.dev/download/mylib-example.tar.gz | grep 'ended differently'
10 of 64 perturbed schedules ended differently
```

## Look at the failure

A failing run is kept like any other. It is a directory holding its inputs and
every event with its step.

```console
$ rewind log c4b6cd3b --steps | tail -4
      4595   165  worker picked job 17
      4606   165  job 17 done: 43360
      4629   161  /nix/store/...-bash-5.3p15/bin/bash: line 1:   165 Segmentation fault         ./$t
      4635   160  make: *** [Makefile:18: check] Error 1
```

The events around the crash show the kernel's own report, with the faulting
instruction:

```console
$ rewind events c4b6cd3b --from 4609 --to 4620
      4610   165/168   thread exit(test_pool_shutd) exited:0
      4613     0/0     console "[    0.197420] test_pool_shutd[169]: segfault at 108 ip 0000564c6aec2437 sp 00007fedbcddce10 error 6 in test_pool_shutdown[1437,564c6aec2000+1000] likely on CPU 0 (core 0, socket 0)"
      4614     0/0     console "[    0.197425] Code: fa 48 8d 35 2a 0c 00 00 bf 02 00 00 00 b8 00 00 00 00 e8 ac fc ff ff 48 8b 05 b5 2b 00 00 48 8b 38 e8 8d fc ff ff 49 8b 46 58 <83> 80 08 01 00 00 01 ..."
      4615   165/169   SIGSEGV code=1 addr=0x108
      4617   165/165   SIGKILL code=0 addr=0x0
```

The instruction marked `<83> 80 08 01 00 00 01` is `addl $1, 0x108(%rax)`:
`p->queue->completed++` with `p->queue` null, 0x108 bytes into the queue.

The processes alive at the crash:

```console
$ rewind ps c4b6cd3b --at 4615
     1 /init
    33   /nix/store/...-bash-5.3p15/bin/bash -e /nix/store/...-source-stdenv.sh /nix/store/...-default-builder.sh
   160     make SHELL=/nix/store/...-bash-5.3p15/bin/bash PREFIX=$(out) VERBOSE=y check
   161       /nix/store/...-bash-5.3p15/bin/bash -c for t in tests/test_pool_basic tests/test_pool_shutdown; do echo "running $t"; ./$t || exit 1; done
   165         ./tests/test_pool_shutdown
   169           (thread)
```

## Replay it

A failing run fails the same way every time it runs:

```console
$ rewind replay c4b6cd3b
identical: 1635 events over 4657 steps

$ rewind replay c4b6cd3b --from 3400
identical from the keyframe at step 1654 to the end (0.77s)
```

`rewind` keeps keyframes while a run executes: snapshots of the machine, with
memory stored once per distinct page. `--from` restores the keyframe at or
before a step and runs from there. A keyframe that did not reproduce the rest
of the run exactly would make that command say so.

## Fork it

A fork is a run that is its parent up to a step, then explores another
schedule from there:

```console
$ rewind fork 68bba6be 3821 --schedule 1 --quiet
rewind: run a43c60f790dabd65 exited:0 after 6485 steps, 0.219s virtual, 1.305s wall (poweroff)
rewind: the fork first differs from its parent at step 3824

$ rewind fork 68bba6be 3821 --schedule 2 --quiet
rewind: run 8efceb03a1f1cbbd exited:2 after 4622 steps, 0.198s virtual, 1.053s wall (poweroff)
rewind: the fork first differs from its parent at step 3830
```

Forked from the passing build at step 3821, schedule 1 passes and schedule 2
crashes. So the bug can be reached from that step by more than the one
interleaving `check` found.

## Scrub it in the app

The desktop app shows the same run on a timeline. Drag the playhead to any
step to see the build log up to that step, the processes alive, the files
written, and the event at the step. Jump to the failure, jump to where the
failing run left the passing one, and fork from the playhead.

```console
$ rewind-app ~/.local/share/rewind/runs/c4b6cd3b789285ea --compare ~/.local/share/rewind/runs/68bba6bee6c22725
```

## Fix it and check the fix

Unpack the example into a directory of your own. The directory is a flake as
well, so `rewind` can check your edit of it:

```console
$ curl -L https://rewindvm.dev/download/mylib-example.tar.gz | tar -xz
$ cd mylib
```

In `src/pool.c`, count the finished job under the lock, and free the queue
only after the workers are joined:

```diff
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
@@
-	/* The bug: the queue goes before the workers are joined. */
-	free(p->queue);
-	p->queue = NULL;
-
 	for (int i = 0; i < POOL_WORKERS; i++)
 		pthread_join(p->workers[i], NULL);
+	free(p->queue);
+	p->queue = NULL;
```

```console
$ rewind check --all .
...
0 of 64 perturbed schedules ended differently
same result under all 65 schedules
```

Before the fix, 10 of the same 64 schedules crashed.

## What to read next

- [The container tutorial](tutorial-container.md) does the same with a
  Docker image instead of a derivation.
- [Counter time](pmu.md) explains how the VM's clock follows its work, and
  the exit time warning on AMD.
