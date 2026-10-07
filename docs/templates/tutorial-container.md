# Tutorial: a flaky test in a container

<!-- run: mkdir -p opt && tar -xzf {{release}}/rewind-x86_64-linux.tar.gz -C opt && tar -xzf {{release_debug}}/rewind-debug-x86_64-linux.tar.gz -C opt -->
<!-- env PATH: echo {{work}}/opt/rewind-x86_64-linux/bin:$PATH -->
<!-- assert: test "$(command -v rewind)" = {{work}}/opt/rewind-x86_64-linux/bin/rewind -->

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

```console run hide
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

```console run show=-3:
$ docker export $(docker create mylib) -o mylib.tar
$ rewind run --root mylib.tar --cwd /src -- make check
```

<!-- capture built_steps: exited:0 after (\d+) steps -->

It passes, in the same {{built_steps}} steps every time. Nothing the command
writes reaches your disk. A run's id is the hash of its inputs, so your image
makes ids other than these.

## Find a failing interleaving

`rewind check` runs it again under perturbed schedules, which reschedule the
VM's threads at different points, and stops at the first batch in which a run
ends differently:

```console run name=check time=check_seconds
$ rewind check --root mylib.tar --cwd /src -- make check
```

<!-- capture passing: passing: run (\w+) -->
<!-- capture failing: failing: run (\w+) -->
<!-- capture narrowed: schedule (\d+) ends differently -->
<!-- capture window_from: perturbing only steps (\d+)\.\. -->
<!-- capture window_until: perturbing only steps \d+\.\.(\d+) -->
<!-- capture deciding: step (\d+) decides it -->
<!-- capture deciding_words: decides it: (.+) there makes the run fail -->
<!-- capture base: schedule +0: .* run (\w+) -->
<!-- assert: test {{deciding}} -eq $(( {{window_until}} - 1 )) -->
<!-- assert: rewind show {{passing}} | tail -1 | grep -q -- '--schedule-until {{deciding}} ' -->
<!-- set failed: grep -E '^schedule +[0-9]+: exited:[1-9]' {{out:check}} | awk '{print $2}' | tr -d : | tr '\n' ' ' -->

`check` narrows schedule {{narrowed}}'s failure to one step, {{deciding}}. The
passing run perturbs the same steps less that one, and only the failing run
gets {{deciding_words}} there. The last block shows where the test's own output
then differs. The search took {{check_seconds}} seconds. `--all` tries every
schedule:

```console run
$ rewind check --all --root mylib.tar --cwd /src -- make check | grep 'ended differently'
```

<!-- capture flaky: (\d+) of 64 perturbed schedules ended differently -->

## Look at the failure

<!-- set crash_step: rewind events {{failing}} | grep -m1 SIGSEGV | awk '{print $1}' -->
<!-- set pid: rewind events {{failing}} | grep -m1 SIGSEGV | awk '{print $2}' | cut -d/ -f1 -->
<!-- set crash_tid: rewind events {{failing}} | grep -m1 SIGSEGV | awk '{print $2}' | cut -d/ -f2 -->
<!-- set last_write: rewind events {{failing}} --to {{crash_step}} | grep -E " {{pid}}/{{crash_tid}} +write" | tail -1 | awk '{print $1}' -->
<!-- set events_to: echo $(( {{crash_step}} + 2 )) -->

```console run
$ rewind log {{failing|short}} --steps | tail -4
$ rewind events {{failing|short}} --from {{last_write}} --to {{events_to}}
```

<!-- assert: rewind events {{failing}} --from {{last_write}} --to {{events_to}} | grep -q '<83> 80 08 01 00 00 01' -->

The kernel's report marks the faulting instruction, `<83> 80 08 01 00 00 01`:
`addl $1, 0x108(%rax)`, which is `p->queue->completed++` with `p->queue`
null. The processes alive at the crash:

```console run
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
at the step. Debian strips its libc; `DEBUGINFOD_URLS` names Debian's
debuginfod server, which has its symbols. From step {{last_write}}, thread
{{crash_tid}}'s last write, continue to the line that faulted:

```console run name=gdb
$ DEBUGINFOD_URLS=https://debuginfod.debian.net rewind gdb {{failing|short}} {{last_write}} -- -batch -ex 'break src/pool.c:77 if p->queue == 0' -ex continue -ex 'bt 3' -ex 'p p->queue' -ex list
```

<!-- assert: grep -q 'Breakpoint 1, worker' {{out:gdb}} && grep -q 'in start_thread' {{out:gdb}} -->

Debian's server has libc's symbols but not its sources, so libc's frames show
no source and gdb prints `Download failed: Invalid argument` for each.

<!-- assert: grep -q 'Download failed: Invalid argument' {{out:gdb}} -->

Images built on Fedora, Ubuntu or Arch have debuginfod servers of their own,
listed by [elfutils](https://sourceware.org/elfutils/Debuginfod.html).

## Replay it

```console run
$ rewind replay {{failing|short}}
$ rewind replay {{failing|short}} --from {{window_from}}
```

<!-- assert: rewind replay {{failing}} | grep -q '^identical' -->

`--from` starts at the nearest keyframe before the step. A run replays on any
machine with the same CPU vendor.

## Fork it

<!-- set fork_first: for s in $(seq 1 16); do rewind fork {{base}} {{window_from}} --schedule $s --json 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin)["status"])'; done | python3 -c 'import sys; s=[int(l) != 0 for l in sys.stdin]; print(next(i + 1 for i in range(len(s) - 3) if 1 <= sum(s[i:i + 4]) <= 3))' -->
<!-- set f2: echo $(( {{fork_first}} + 1 )) -->
<!-- set f3: echo $(( {{fork_first}} + 2 )) -->
<!-- set f4: echo $(( {{fork_first}} + 3 )) -->

```console run name=forks
$ rewind fork {{base|short}} {{window_from}} --schedule {{fork_first}} --quiet
$ rewind fork {{base|short}} {{window_from}} --schedule {{f2}} --quiet
$ rewind fork {{base|short}} {{window_from}} --schedule {{f3}} --quiet
$ rewind fork {{base|short}} {{window_from}} --schedule {{f4}} --quiet
```

<!-- set fork_run: grep -m1 -o 'run [0-9a-f]* exited:[1-9]' {{out:forks}} | awk '{print $2}' -->
<!-- set fork_crash: rewind events {{fork_run}} | grep -m1 SIGSEGV | awk '{print $1}' -->

From step {{window_from}} of the schedule 0 run, some schedules crash and
some pass.

## Scrub it in the app

The desktop app shows a run on a timeline: drag the playhead to any step for
the output, processes, files and event there, jump to the failure or to where
the run left the passing one, and fork from the playhead.

<!-- screenshot site/img/app-container: {{home}}/runs/{{fork_run}} --compare {{home}}/runs/{{base}} --step {{fork_crash}} -->

```console
$ rewind-app ~/.local/share/rewind/runs/{{failing}} --compare ~/.local/share/rewind/runs/{{passing}}
```

<!-- assert: rewind where {{failing}} {{crash_step}} 2>/dev/null | grep -q '^#0 worker (src/pool.c:77)' -->
<!-- assert: rewind where {{failing}} {{crash_step}} 2>/dev/null | grep -q '^> *77 .*p->queue->completed++;' -->

Press f to jump to the failure at step {{crash_step}}, then s to open the
source panel. After a few seconds it shows `worker` at `src/pool.c:77`, with
`p->queue->completed++;` marked: the line that read the queue after
`pool_shutdown` had set it to NULL. The t key shows which thread held the CPU
around where the two runs part.

## Fix it and check the fix

In `src/pool.c`, count the job under the lock, and free the queue after the
workers are joined:

<!-- diff: mylib-fix.patch -->
<!-- run: patch -p3 < {{dir}}/mylib-fix.patch -->

Rebuild the image, export it, and check again:

```console run name=fixed show=-2:
$ docker build -q -t mylib -f Containerfile . && docker export $(docker create mylib) -o mylib.tar
$ rewind check --all --root mylib.tar --cwd /src -- make check
```

<!-- assert: grep -q '^0 of 64 perturbed schedules ended differently' {{out:fixed}} -->

Before the fix, {{flaky}} of the same 64 schedules crashed.

## Limits

The VM has one vCPU, so threads interleave but never run at the same instant:
a race between plain loads and stores with no system call between them is out
of reach, while races across a system call, a lock or a sleep, like this one,
are in reach. The container runs with no network. [Design](design.md#limits)
has the full list.

## What to read next

- [The Nix tutorial](tutorial-nix.md): the same bug as a Nix derivation.
- [The advanced tutorial](tutorial-advanced.md): threads, watchpoints, the
  kernel's side of a crash, sharing a run and more.
- [Counter time](pmu.md): how the VM's clock follows its work.
