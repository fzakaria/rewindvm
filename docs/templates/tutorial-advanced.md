# Tutorial: advanced features

<!-- let epoch = 1790985600 -->
<!-- let flake = github:fzakaria/rewindvm#mylib -->

Short recipes for what the [Nix](tutorial-nix.md) and
[container](tutorial-container.md) tutorials leave out, each in the terminal
and in the desktop app. They use the Nix tutorial's runs and install.

## The runs

```console run name=check
$ rewind check --epoch {{epoch}} {{flake}} | grep -E 'ends differently|perturbing only|passing:|failing:'
```

<!-- capture passing: passing: run (\w+) -->
<!-- capture failing: failing: run (\w+) -->
<!-- set crash_step: rewind events {{failing}} | grep -m1 SIGSEGV | awk '{print $1}' -->
<!-- set pid: rewind events {{failing}} | grep -m1 SIGSEGV | awk '{print $2}' | cut -d/ -f1 -->
<!-- set crash_tid: rewind events {{failing}} | grep -m1 SIGSEGV | awk '{print $2}' | cut -d/ -f2 -->
<!-- set last_write: rewind events {{failing}} --to {{crash_step}} | grep -E " {{pid}}/{{crash_tid}} +write" | tail -1 | awk '{print $1}' -->
<!-- set watch_from: rewind events {{failing}} --from $(( {{crash_step}} - 50 )) --to {{crash_step}} | grep -E " {{pid}}/[0-9]+ +write" | head -1 | awk '{print $1}' -->

The failing run crashes at step {{crash_step}}.

## Find the line a thread was on

`rewind where` names the line of the program's own code a thread was on at a
step, past the C library and, in Rust, the standard library and
dependencies. By default it looks at the thread of the step's own event, the
pid/tid pair `rewind events` prints; at {{crash_step}} that is the worker that
segfaulted:

```console run name=where
$ rewind where {{failing|short}} {{crash_step}}
```

<!-- assert: grep -q '^#0 worker (src/pool.c:77)' {{out:where}} && grep -q '^> *77 .*p->queue->completed++;' {{out:where}} && grep -q 'start_thread' {{out:where}} && grep -q 'clone3' {{out:where}} -->

`--tid` picks another thread, on the CPU or not. At step {{last_write}}, the
worker's last write, the main thread is waiting to join the workers:

```console run name=where_main
$ rewind where {{failing|short}} {{last_write}} --tid {{pid}}
```

<!-- assert: grep -q 'pool_shutdown (src/pool.c:128)' {{out:where_main}} && grep -q 'pthread_join' {{out:where_main}} -->

`--json` prints every frame and which one was chosen. For every frame of
every thread, ask gdb for all the stacks of the process:

```console run name=all_bt
$ rewind gdb {{failing|short}} {{last_write}} --pid {{pid}} -- -batch -ex 'thread apply all bt' 2>/dev/null | grep -E '^Thread|src/|tests/'
```

<!-- assert: grep -q 'in pool_shutdown .* at src/pool.c:128' {{out:all_bt}} && grep -q 'in main () at tests/test_pool_shutdown.c' {{out:all_bt}} -->

Both read each thread's registers from the VM kernel's task list, so they
work on runs recorded with a guest that lists its tasks, as these were.

In the app, Show source under Inspect, or the s key, opens the source panel
in place of At this step. It names the line `rewind where` would for the
thread of the playhead's event and shows that line's whole source file,
syntax colored, scrolled so the line, marked, sits in the middle. The thread's frames are
listed in a short list pinned below the file, with the chosen one marked.
Clicking another frame shows its file instead, scrolled to its line and
marked; a frame without source shows its address and program. A file over a
mebibyte comes as the lines around its frames' lines, and the panel says
which. Long lines scroll sideways with Shift and the wheel, or a sideways
swipe, while the line numbers stay put. When the playhead rests on another step the panel
asks again, back at the chosen frame, dimming the last answer meanwhile; each
answer forks the run, so it takes a few seconds. Runs the app cannot fork
say so instead: the bundled example and an export that holds only the trace.
So does a run recorded before the guest listed its tasks, which has to be
recorded again.

<!-- screenshot site/img/app-source: {{home}}/runs/{{failing}} --compare {{home}}/runs/{{passing}} --step {{crash_step}} --source ;; wait 20 -->

![The app's source panel at step {{crash_step}} of the failing run: worker at src/pool.c:77 with p->queue->completed++ marked, and the frames worker, start_thread and clone3 below](../site/img/app-source.png)

## Watch both sides of the race

A watchpoint finds who freed the queue the crash reads. Break in a worker so
`p` is in scope, watch `p->queue`, and continue:

```console run name=watch
$ rewind gdb {{failing|short}} {{watch_from}} -- -batch -ex 'break src/pool.c:74' -ex continue -ex 'watch -l p->queue' -ex 'delete 1' -ex continue -ex 'bt 2' -ex 'break src/pool.c:77 if p->queue == 0' -ex continue -ex 'bt 1'
```

<!-- assert: grep -q 'New value = (struct queue \*) 0x0' {{out:watch}} && grep -q 'in main ()' {{out:watch}} && grep -q 'Breakpoint 3, worker' {{out:watch}} -->

The first stop is `main` nulling the queue in `pool_shutdown`, the second the
worker reading it. Watchpoints are the CPU's four debug registers, so the VM
runs at full speed until one fires. `watch` and `awatch` work; x86 has no
`rwatch`. At user addresses they stop only in the process gdb started in.

In the app, Attach gdb opens the same session under the timeline:

<!-- screenshot site/img/app-gdb-watch: {{home}}/runs/{{failing}} --compare {{home}}/runs/{{passing}} --step {{watch_from}} ;; click 1140 698 ;; wait 25 ;; type break src/pool.c:74 ;; wait 1 ;; type continue ;; wait 8 ;; type watch -l p->queue ;; wait 1 ;; type delete 1 ;; wait 1 ;; type continue ;; wait 8 -->

![The app's gdb pane stopped at the watchpoint in pool_shutdown, with the old and new values of p->queue](../site/img/app-gdb-watch.png)

## Follow the fault into the kernel

`rewind gdb` has the VM kernel's symbols too. Stop where the kernel sends the
SIGSEGV, and use its own gdb scripts:

```console run name=kernel
$ rewind gdb {{failing|short}} {{last_write}} -- -batch -ex 'break force_sig_fault' -ex continue -ex 'bt 4' -ex 'pipe lx-ps | tail -4' -ex 'pipe lx-dmesg | tail -2'
```

<!-- assert: grep -q 'force_sig_fault (sig=11, code=1, addr=0x108)' {{out:kernel}} -->

The kernel's DWARF comes from `rewindvm.cachix.org` with Nix, and from the
debug tarball next to `rewind` without. The app's build log shows the kernel's
messages beside the program's with kernel console on:

<!-- screenshot site/img/app-kernel: {{home}}/runs/{{failing}} --compare {{home}}/runs/{{passing}} --step {{crash_step}} ;; click 550 227 ;; wait 2 -->

![The app's build log with the kernel console on, showing the segfault report under the test's output](../site/img/app-kernel.png)

## Use your own gdb

`--listen` serves the fork for a gdb started elsewhere, such as an IDE's, and
prints the command line that loads the same symbols:

```console run bg=connect cut=230
$ rewind gdb {{failing|short}} {{crash_step}} --listen 127.0.0.1:1234
```

<!-- after: gdb -q -batch -ex 'target remote 127.0.0.1:1234' -ex detach -->

## Bring tools into the VM

`rewind shell --with` adds a Nix package to the shell, without changing the
run. Here binutils disassembles the faulting instruction:

```console run name=objdump
$ printf 'objdump -d --no-show-raw-insn --start-address=0x142c --stop-address=0x143e tests/test_pool_shutdown | tail -4; exit\n' | rewind shell {{failing|short}} {{crash_step}} --pid {{pid}} --with nixpkgs#binutils
```

<!-- assert: grep -q 'addl   $0x1,0x108(%rax)' {{out:objdump}} -->

## Give programs more CPUs

The VM has one vCPU, so by default programs see one CPU and Nix builds run
one job at a time. `--cores` tells them there are more, and the extra threads
interleave on the one vCPU:

```console run name=cores show=-2:
$ rewind nix --cores 4 --epoch {{epoch}} {{flake}}
```

<!-- capture cores_run: run (\w+) exited -->
<!-- set make_step: rewind events {{cores_run}} | grep -m1 'execve.*"make"' | awk '{print $1}' -->
<!-- set make_pid: rewind events {{cores_run}} | grep -m1 'execve.*"make"' | awk '{print $2}' | cut -d/ -f1 -->

```console run name=nproc
$ printf 'nproc; echo $NIX_BUILD_CORES; exit\n' | rewind shell {{cores_run|short}} {{make_step}} --pid {{make_pid}}
```

<!-- assert: test "$(tail -2 {{out:nproc}} | tr '\n' ' ')" = '4 4 ' -->

Run Go programs with `GOMAXPROCS=1` above one core: Go's garbage collector
spins waiting for a thread that never runs.

## Steer the perturbation

<!-- set fork_from: echo $(( {{crash_step}} - 100 )) -->

`check --schedules N` tries more or fewer schedules, `--all` all of them.
`rewind fork` asks which schedules fail from a given step:

```console run name=forks
$ rewind fork {{failing|short}} {{fork_from}} --schedule 1 --quiet
$ rewind fork {{failing|short}} {{fork_from}} --schedule 2 --quiet
$ rewind fork {{failing|short}} {{fork_from}} --schedule 3 --quiet
$ rewind fork {{failing|short}} {{fork_from}} --schedule 4 --quiet
```

<!-- assert: grep -q 'exited:0' {{out:forks}} && grep -q 'exited:[1-9]' {{out:forks}} -->

The app's Runs panel shows every run of the build, with forks under the run
and step they branched from:

<!-- screenshot site/img/app-runs: {{home}}/runs/{{failing}} --compare {{home}}/runs/{{passing}} --step {{crash_step}} ;; click 785 27 ;; wait 3 -->

![The app's Runs panel: check's schedules, and the forks of the failing run, failing and passing](../site/img/app-runs.png)

## Rebuild a run exactly

<!-- set spec: python3 -c 'import json,sys; s=json.load(open(sys.argv[1]))["spec"]; print(s["epoch"], s["schedule"], s["schedule_from"], s["schedule_until"])' {{home}}/runs/{{failing}}/manifest.json -->
<!-- set s_epoch: echo {{spec}} | awk '{print $1}' -->
<!-- set s_schedule: echo {{spec}} | awk '{print $2}' -->
<!-- set s_from: echo {{spec}} | awk '{print $3}' -->
<!-- set s_until: echo {{spec}} | awk '{print $4}' -->

A run's id is the hash of its inputs, which its `manifest.json` lists. The
same flags make the same run:

```console run name=rebuild
$ rewind nix --quiet --epoch {{s_epoch}} --schedule {{s_schedule}} --schedule-from {{s_from}} --schedule-until {{s_until}} {{flake}}
```

<!-- assert: grep -q 'run {{failing}} ' {{out:rebuild}} -->

## Compare any two runs

`rewind diff` compares every event of two runs, where `check` compares only
the failing program's:

```console run
$ rewind diff {{passing|short}} {{failing|short}}
```

In the app, any run compared with another shows where they part, on the
timeline and in the divergence card:

<!-- screenshot site/img/app-compare: {{home}}/runs/{{failing}} --compare {{home}}/runs/{{passing}} ;; key d ;; wait 2 -->

![The app at the step where the failing run leaves the passing one, with the divergence card naming both threads' writes](../site/img/app-compare.png)

## Hand a failure to someone else

```console run
$ rewind export {{failing|short}} --replayable -o crash.rwd
$ REWIND_HOME=elsewhere rewind import crash.rwd
$ REWIND_HOME=elsewhere rewind replay {{failing|short}}
```

<!-- set small: rewind export {{failing}} -o small.rwd 2>&1 | grep -o '([^)]*)' | tr -d '()' -->

A replayable export carries the keyframes, the image and the VM's kernel, and
replays on any machine with the same CPU vendor. Without `--replayable` it is
the events alone, {{small}}: enough to read and to scrub in the app, whose
Export button writes the replayable kind.

## Which clock

```console run
$ rewind pmu status
```

With counter time the VM's clock follows the work done inside it; with exit
time computation takes no virtual time. `--clock` picks one. [Counter
time](pmu.md) explains.

## Keep the run directory tidy

<!-- run: for s in 1 2; do REWIND_HOME=elsewhere rewind fork {{failing}} {{fork_from}} --schedule $s --quiet; done -->
<!-- set removed: REWIND_HOME=elsewhere rewind ls | head -1 | awk '{print $1}' -->

Runs live under `~/.local/share/rewind`, or `REWIND_HOME`:

```console run
$ REWIND_HOME=elsewhere rewind ls
$ REWIND_HOME=elsewhere rewind remove {{removed|short}}
```

`remove` takes a run with every run forked from it, and takes many runs in
one call. Every fork goes, and the runs they were forked from stay, with
`rewind ls | awk '/fork of/ {print $1}' | xargs rewind remove`.
`prune --identical` removes forks that ran exactly as an older one did.

## What to read next

- [Design](design.md): how the machine is made deterministic, and its
  [limits](design.md#limits).
- The [case studies](case-studies/nix-gc-closure-sigpipe.md): these tools on
  bugs in real projects.
