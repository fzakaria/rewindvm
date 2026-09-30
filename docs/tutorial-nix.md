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
laptop.

## Install

You need x86_64 Linux with KVM and Nix with flakes enabled. The release
tarball is a flake too:

```console
$ nix profile install https://rewindvm.dev/download/rewind-0.1.0-x86_64-linux.tar.gz
$ ls -l /dev/kvm
crw-rw-rw- 1 root kvm 10, 232 Sep 30 15:37 /dev/kvm
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
usually succeeds. Rebuilt 45 times on the laptop, it failed 3 times, each in
`checkPhase`:

```console
$ nix build --rebuild -L https://rewindvm.dev/download/mylib-example.tar.gz
...
mylib> running tests/test_pool_shutdown
...
mylib> job 16 done: 5986
mylib> worker picked job 18
mylib> job 17 done: 43360
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
rewind: recording with exit time: this AMD CPU's branch counter is not exact until rr's workaround is set. Computation will not move the VM's clock, and a thread that computes without system calls is not preempted. Fix it with `sudo rewind pmu enable` (until reboot), then `rewind pmu status`. Why: https://rewindvm.dev/counter-time.html
rewind: packing 62 store paths for mylib-0.3.0
...
rewind: run 112912f87cd8d72f exited:0 after 5096 steps, 0.029s virtual, 2.793s wall (poweroff)
/nix/store/f6a9gy362szw6nxx3ikrklr8glr6rdln-mylib-0.3.0 aa30ea54dc47c30f (same as the host's build)
```

The first line is there because this laptop's AMD branch counter is not exact,
so the VM's clock moves only at exits; [Counter time](pmu.md) explains what
that changes and how to fix it, and the transcripts below leave the line out.

This build passes, and it passes every time: the same inputs make the same
run, down to the same 5096 steps. A step is one exit from the VM to Rewind,
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
schedule   0: exited:0             5096 steps  aa30ea54dc47  run 112912f87cd8d72f
schedule   1: exited:2             4001 steps    run 8c26b1b06a82bddb
schedule   2: exited:2             4310 steps    run b120a318ce77fef0
schedule   3: exited:2             4151 steps    run 1c2432ed4f110d83
schedule   4: exited:0             5110 steps  aa30ea54dc47  run ec8f35fbb5e23266
schedule   5: exited:0             5085 steps  aa30ea54dc47  run 2c3c7d45f62146de
schedule   6: exited:0             5108 steps  aa30ea54dc47  run a1aaf5f49f71e03c
schedule   7: exited:0             5080 steps  aa30ea54dc47  run 34d8dc15f8c62f08
schedule   8: exited:0             5096 steps  aa30ea54dc47  run 430669c4bf3660c0
schedule   9: exited:2             4159 steps    run 22e60c2efe6bbb2f
schedule  10: exited:2             4333 steps    run 2de73bfc4ae1c188
schedule  11: exited:2             3825 steps    run 286892a015341ff0
schedule  12: exited:2             4141 steps    run 558749e401b1f5fa
schedule  13: exited:0             5091 steps  aa30ea54dc47  run 29316d476b88d24e
schedule  14: exited:2             4170 steps    run 39ead661d037d0c7
schedule  15: exited:0             5108 steps  aa30ea54dc47  run 5e98129d1ca29860
schedule  16: exited:0             5126 steps  aa30ea54dc47  run 5b6677a6c13f43e2

schedule 1 ends differently; narrowing the steps it perturbs
perturbing only steps 3790..3970 still ends differently

passing: run 112912f87cd8d72f
failing: run 67dc92676d949035

where ./tests/test_pool_shutdown first behaves differently:
  both        3781   173/174   write(1, "job 14 done: 39906\n")
  both        3782   173/174   write(1, "worker picked job 17\n")
  both        3790   173/174   write(1, "job 17 done: 43360\n")
  left        3791   173/174   write(1, "worker picked job 18\n")
  left        3794   173/175   thread exit(test_pool_shutd) exited:0
  left        3804   173/174   thread exit(test_pool_shutd) exited:0
  left        3807   173/173   clone(CLONE_THREAD) = 176
  right       3792   173/175   thread exit(test_pool_shutd) exited:0
  right       3795   173/174   SIGSEGV code=1 addr=0x108
  right       3796   173/173   SIGKILL code=0 addr=0x0
  right       3798   173/174   thread exit(test_pool_shutd) killed:SIGSEGV
```

The first perturbed schedule already fails, and 8 of the first 16 do. `check`
then narrows schedule 1's perturbation to the smallest window that still
changes the outcome, here steps 3790 to 3970. It keeps two runs: the
unperturbed one and the failing one with the narrowed window. The two are
identical up to step 3790.

The last block compares only the failing program's own events. In both runs,
worker 174 reports job 17 at step 3790. In the passing run it then picks job
18, and both workers see the pool stopping and exit. In the failing run the
main thread starts shutdown before the worker counts job 17: the other worker
exits, and worker 174's count ends in a SIGSEGV at address `0x108`.

The whole search took 12 seconds. `rewind check --all` tries every schedule
and says how many failed, which measures how flaky a build is:

```console
$ rewind check --all https://rewindvm.dev/download/mylib-example.tar.gz | grep 'ended differently'
34 of 64 perturbed schedules ended differently
```

## Look at the failure

A failing run is kept like any other. It is a directory holding its inputs and
every event with its step.

```console
$ rewind log 67dc9267 --steps | tail -4
      3782   173  worker picked job 17
      3790   173  job 17 done: 43360
      3806   169  /nix/store/...-bash-5.3p15/bin/bash: line 1:   173 Segmentation fault         ./$t
      3809   168  make: *** [Makefile:18: check] Error 1
```

The events around the crash show the kernel's own report, with the faulting
instruction:

```console
$ rewind events 67dc9267 --from 3789 --to 3800
      3790   173/174   write(1, "job 17 done: 43360\n")
      3792   173/175   thread exit(test_pool_shutd) exited:0
      3793     0/0     console "[    0.020235] test_pool_shutd[174]: segfault at 108 ip 0000555cbf965437 sp 00007f36ef923e10 error 6 in test_pool_shutdown[1437,555cbf965000+1000] likely on CPU 0 (core 0, socket 0)"
      3794     0/0     console "[    0.020240] Code: fa 48 8d 35 2a 0c 00 00 bf 02 00 00 00 b8 00 00 00 00 e8 ac fc ff ff 48 8b 05 b5 2b 00 00 48 8b 38 e8 8d fc ff ff 49 8b 46 58 <83> 80 08 01 00 00 01 ..."
      3795   173/174   SIGSEGV code=1 addr=0x108
      3796   173/173   SIGKILL code=0 addr=0x0
      3798   173/174   thread exit(test_pool_shutd) killed:SIGSEGV
```

The instruction marked `<83> 80 08 01 00 00 01` is `addl $1, 0x108(%rax)`:
`p->queue->completed++` with `p->queue` null, 0x108 bytes into the queue.

The processes alive at the crash:

```console
$ rewind ps 67dc9267 --at 3795
     1 /init
    40   /nix/store/...-bash-5.3p15/bin/bash -e /nix/store/...-source-stdenv.sh /nix/store/...-default-builder.sh
   168     make SHELL=/nix/store/...-bash-5.3p15/bin/bash PREFIX=$(out) VERBOSE=y check
   169       /nix/store/...-bash-5.3p15/bin/bash -c for t in tests/test_pool_basic tests/test_pool_shutdown; do echo "running $t"; ./$t || exit 1; done
   173         ./tests/test_pool_shutdown
   174           (thread)
```

## Replay it

A failing run fails the same way every time it runs:

```console
$ rewind replay 67dc9267
identical: 1590 events over 3825 steps

$ rewind replay 67dc9267 --from 2800
identical from the keyframe at step 1762 to the end (0.66s)
```

`rewind` keeps keyframes while a run executes: snapshots of the machine, with
memory stored once per distinct page. `--from` restores the keyframe at or
before a step and runs from there. A keyframe that did not reproduce the rest
of the run exactly would make that command say so.

## Fork it

A fork is a run that is its parent up to a step, then explores another
schedule from there:

```console
$ rewind fork 112912f8 3790 --schedule 1 --quiet
rewind: run 220334a68b05f527 exited:2 after 3825 steps, 0.020s virtual, 0.919s wall (poweroff)
rewind: the fork first differs from its parent at step 3792

$ rewind fork 112912f8 3790 --schedule 2 --quiet
rewind: run 4cf2cf4e94e701d1 exited:2 after 4158 steps, 0.024s virtual, 0.945s wall (poweroff)
rewind: the fork first differs from its parent at step 3811
```

Both forks of the passing build at step 3790 crash. That says the bug is a
property of the code after step 3790, not an accident of anything earlier in
the build.

## Scrub it in the app

The desktop app shows the same run on a timeline. Drag the playhead to any
step to see the build log up to that step, the processes alive, the files
written, and the event at the step. Jump to the failure, jump to where the
failing run left the passing one, and fork from the playhead.

```console
$ rewind-app ~/.local/share/rewind/runs/67dc92676d949035 --compare ~/.local/share/rewind/runs/112912f87cd8d72f
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

Before the fix, 34 of the same 64 schedules crashed.

## What to read next

- [The container tutorial](tutorial-container.md) does the same with a
  Docker image instead of a derivation.
- [Counter time](pmu.md) explains the exit time warning, and how to make
  the VM's clock follow its work on AMD.
