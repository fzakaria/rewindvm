# Tutorial: a flaky Nix build

<!-- let epoch = 1790985600 -->
<!-- let flake = github:fzakaria/rewindvm#mylib -->

This tutorial takes a derivation whose tests fail now and then, has Rewind VM
find a failing run, looks at the crash, and checks the fix across 64 thread
interleavings.

The derivation is `mylib`, a small C thread pool in
[examples/mylib](https://github.com/fzakaria/rewindvm/tree/main/examples/mylib).
Its `pool_shutdown` frees the job queue before joining the workers, so a
worker still finishing a job can write through a freed, nulled pointer.

Every transcript below is real output from `rewind` on a 16 core AMD laptop.

## Install

You need x86_64 Linux with KVM and Nix with flakes enabled.

```console
$ nix profile install github:fzakaria/rewindvm
$ ls -l /dev/kvm
crw-rw-rw- 1 root kvm 10, 232 Oct  1 20:16 /dev/kvm
```

The build comes from `rewindvm.cachix.org`; say yes when Nix asks to trust
it. The desktop app is `github:fzakaria/rewindvm#app`. On NixOS, use the
module instead:

```nix
inputs.rewind.url = "github:fzakaria/rewindvm";

# with inputs.rewind.nixosModules.default imported
programs.rewind.enable = true;
programs.rewind.app.enable = true;
# AMD only: make the branch counter exact at every boot
programs.rewind.amdBranchCounterWorkaround = true;
```

If `/dev/kvm` is not yours to use, add yourself to the `kvm` group. On AMD,
run `sudo rewind pmu enable` once after each boot; [Counter time](pmu.md)
says why.

## The flaky build

On the host the build usually passes. Rebuilt 45 times, it failed twice:

```console
$ nix build --rebuild -L github:fzakaria/rewindvm#mylib
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

Run it again and it usually passes, leaving nothing to look at.

## Build it in Rewind VM

<!-- run: nix build {{flake}} --no-link -->

`rewind nix` builds a derivation as the Nix sandbox would, inside a
deterministic virtual machine:

```console run name=build show=:1,-2:
$ rewind nix --epoch {{epoch}} {{flake}}
```

<!-- capture built_steps: exited:0 after (\d+) steps -->
<!-- assert: grep -q 'matches your store' {{out:build}} -->

It passes, in the same {{built_steps}} steps every time. `--epoch` fixes the
VM's clock at boot, one of the run's inputs, so your runs are the ones shown
here. The last line says the output matches the copy in your store.

## Find a failing interleaving

`rewind check` builds it again under perturbed schedules, which reschedule
the VM's threads at different points, and stops at the first batch in which a
build ends differently:

```console run name=check time=check_seconds
$ rewind check --epoch {{epoch}} {{flake}}
```

<!-- capture passing: passing: run (\w+) -->
<!-- capture failing: failing: run (\w+) -->
<!-- capture narrowed: schedule (\d+) ends differently -->
<!-- capture window_from: perturbing only steps (\d+)\.\. -->
<!-- capture window_until: perturbing only steps \d+\.\.(\d+) -->
<!-- set failed: grep -E '^schedule +[0-9]+: exited:[1-9]' {{out:check}} | awk '{print $2}' | tr -d : | tr '\n' ' ' -->
<!-- assert: test $(echo {{failed}} | wc -w) -ge 2 -->
<!-- assert: test {{check_seconds}} -lt 60 -->

Schedules {{failed|and}} fail. `check` narrows schedule {{narrowed}}'s
perturbation to steps {{window_from}} to {{window_until}} and keeps two runs,
the passing one and the failing one, identical up to step {{window_from}}.
The last block shows where the test's own output first differs. The search
took {{check_seconds}} seconds.

`--all` tries every schedule, which measures how flaky a build is:

```console run
$ rewind check --all --epoch {{epoch}} {{flake}} | grep 'ended differently'
```

<!-- capture flaky: (\d+) of 64 perturbed schedules ended differently -->

## Look at the failure

<!-- set crash_step: rewind events {{failing}} | grep -m1 SIGSEGV | awk '{print $1}' -->
<!-- set pid: rewind events {{failing}} | grep -m1 SIGSEGV | awk '{print $2}' | cut -d/ -f1 -->
<!-- set crash_tid: rewind events {{failing}} | grep -m1 SIGSEGV | awk '{print $2}' | cut -d/ -f2 -->
<!-- set last_write: rewind events {{failing}} --to {{crash_step}} | grep -E " {{pid}}/{{crash_tid}} +write" | tail -1 | awk '{print $1}' -->
<!-- set events_to: echo $(( {{crash_step}} + 2 )) -->

The failing run keeps every event with its step:

```console run elide
$ rewind log {{failing|short}} --steps | tail -4
$ rewind events {{failing|short}} --from {{last_write}} --to {{events_to}}
```

<!-- assert: rewind events {{failing}} --from {{last_write}} --to {{events_to}} | grep -q '<83> 80 08 01 00 00 01' -->

The kernel's report marks the faulting instruction, `<83> 80 08 01 00 00 01`:
`addl $1, 0x108(%rax)`, which is `p->queue->completed++` with `p->queue`
null. The processes alive at the crash:

```console run elide
$ rewind ps {{failing|short}} {{crash_step}}
```

## Look inside the VM

`rewind cat`, `rewind shell` and `rewind gdb` work on a throwaway fork of the
run at a step. At the SIGSEGV, step {{crash_step}}:

```console run
$ rewind cat {{failing|short}} {{crash_step}} src/pool.c --pid {{pid}} | sed -n '/^void pool_shutdown/,/^}/p'
$ printf 'pwd; ls; exit\n' | rewind shell {{failing|short}} {{crash_step}} --pid {{pid}}
```

`rewind gdb` loads the symbols of the VM's kernel and of the process running
at the step. From step {{last_write}}, thread {{crash_tid}}'s last write,
continue to the line that faulted:

```console run name=gdb
$ rewind gdb {{failing|short}} {{last_write}} -- -batch -ex 'break src/pool.c:77 if p->queue == 0' -ex continue -ex 'bt 3' -ex 'p p->queue' -ex list
```

<!-- assert: grep -q 'Breakpoint 1, worker' {{out:gdb}} && grep -q '(struct queue \*) 0x0' {{out:gdb}} -->

The breakpoint is one of the CPU's debug registers, so the fork runs unchanged
until line 77 with `p->queue` null. Without `--`, gdb stays open for you to
type into.

## Replay it

```console run
$ rewind replay {{failing|short}}
$ rewind replay {{failing|short}} --from {{window_from}}
```

<!-- assert: rewind replay {{failing}} | grep -q '^identical' -->

`--from` starts at the nearest keyframe before the step. A run replays on any
machine with the same CPU vendor.

## Fork it

<!-- set fork_first: for s in $(seq 1 16); do rewind fork {{passing}} {{window_from}} --schedule $s --json 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin)["status"])'; done | python3 -c 'import sys; s=[int(l) != 0 for l in sys.stdin]; print(next(i + 1 for i in range(len(s) - 3) if sum(s[i:i + 4]) == 2))' -->
<!-- set f2: echo $(( {{fork_first}} + 1 )) -->
<!-- set f3: echo $(( {{fork_first}} + 2 )) -->
<!-- set f4: echo $(( {{fork_first}} + 3 )) -->

A fork is its parent up to a step, then another schedule:

```console run name=forks
$ rewind fork {{passing|short}} {{window_from}} --schedule {{fork_first}} --quiet
$ rewind fork {{passing|short}} {{window_from}} --schedule {{f2}} --quiet
$ rewind fork {{passing|short}} {{window_from}} --schedule {{f3}} --quiet
$ rewind fork {{passing|short}} {{window_from}} --schedule {{f4}} --quiet
```

<!-- set fork_failed: grep -o 'exited:[0-9]*' {{out:forks}} | paste - <(printf '%s\n' {{fork_first}} {{f2}} {{f3}} {{f4}}) | awk '$1 != "exited:0" {print $2}' | tr '\n' ' ' -->
<!-- set fork_passed: grep -o 'exited:[0-9]*' {{out:forks}} | paste - <(printf '%s\n' {{fork_first}} {{f2}} {{f3}} {{f4}}) | awk '$1 == "exited:0" {print $2}' | tr '\n' ' ' -->
<!-- set fork_run: grep -m1 -o 'run [0-9a-f]* exited:[1-9]' {{out:forks}} | awk '{print $2}' -->
<!-- set fork_crash: rewind events {{fork_run}} | grep -m1 SIGSEGV | awk '{print $1}' -->

From step {{window_from}} of the passing build, schedules {{fork_failed|and}}
crash and {{fork_passed|and}} pass.

## Scrub it in the app

The desktop app shows a run on a timeline: drag the playhead to any step for
the output, processes, files and event there, jump to the failure or to where
the run left the passing one, and fork from the playhead.

<!-- screenshot site/img/app-failure: {{home}}/runs/{{fork_run}} --compare {{home}}/runs/{{passing}} --step {{fork_crash}} -->

```console
$ rewind-app ~/.local/share/rewind/runs/{{failing}} --compare ~/.local/share/rewind/runs/{{passing}}
```

<!-- assert: rewind where {{failing}} {{crash_step}} 2>/dev/null | grep -q '^#0 worker (src/pool.c:77)' -->
<!-- assert: rewind where {{failing}} {{crash_step}} 2>/dev/null | grep -q '^> *77 .*p->queue->completed++;' -->

Press f to jump to the failure at step {{crash_step}}, then s to open the
source panel. After a few seconds it shows `worker` at `src/pool.c:77`, with
`p->queue->completed++;` marked: the line that read the queue after
`pool_shutdown` had set it to NULL.

## Fix it and check the fix

Clone the repository to edit the flake's `mylib`:

```console run
$ git clone https://github.com/fzakaria/rewindvm
$ cd rewindvm
```

In `examples/mylib/src/pool.c`, count the job under the lock, and free the
queue after the workers are joined:

<!-- diff: mylib-fix.patch -->
<!-- run: patch -p1 < {{dir}}/mylib-fix.patch -->

```console run name=fixed show=-2:
$ rewind check --all --epoch {{epoch}} .#mylib
```

<!-- assert: grep -q '^0 of 64 perturbed schedules ended differently' {{out:fixed}} -->

Before the fix, {{flaky}} of the same 64 schedules crashed.

## Limits

The VM has one vCPU, so threads interleave but never run at the same instant:
a race between plain loads and stores with no system call between them is out
of reach, while races across a system call, a lock or a sleep, like this one,
are in reach. [Design](design.md#limits) has the full list.

## What to read next

- [The container tutorial](tutorial-container.md): the same bug from a Docker
  image, with no Nix.
- [The advanced tutorial](tutorial-advanced.md): watchpoints, the kernel's
  side of a crash, tools inside the VM, more CPUs, and sharing a run.
- [Counter time](pmu.md): how the VM's clock follows its work.
