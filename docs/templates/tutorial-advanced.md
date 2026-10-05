# Tutorial: advanced features

<!-- let epoch = 1790985600 -->
<!-- let flake = github:fzakaria/rewindvm#mylib -->

Short recipes for what the [Nix](tutorial-nix.md) and
[container](tutorial-container.md) tutorials leave out, each in the terminal
and in the desktop app. They use the Nix tutorial's runs and install.

## The runs

```console run name=check
$ rewind check --epoch {{epoch}} {{flake}} | grep -E 'ends differently|perturbing only|passing:|failing:|open both'
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

<!-- set main_frame: rewind where {{failing}} {{last_write}} --tid {{pid}} --json 2>/dev/null | jq .chosen -->

`rewind gdb` starts in the thread `rewind where` looks at, in its innermost
frame. `--tid` and `--frame` start it elsewhere, with frames numbered as
`where --json` numbers them. In the main thread's frame that `where` chose,
`pool_shutdown` has already set the queue to NULL:

```console run name=gdb_frame
$ rewind gdb {{failing|short}} {{last_write}} --tid {{pid}} --frame {{main_frame}} -- -batch -ex 'p p->queue'
```

<!-- assert: grep -q 'in pool_shutdown .* at src/pool.c:128' {{out:gdb_frame}} && grep -q '(struct queue \*) 0x0' {{out:gdb_frame}} -->

`where` and gdb read each thread's registers from the VM kernel's task list,
so they work on runs recorded with a guest that lists its tasks, as these
were.

In the app, Show source under Inspect, or the s key, opens the source panel
in a tab beside At this step. It names the line `rewind where` would for the
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

The app's Runs tab, or the runs pill in its header, shows every run of the
build, with forks under the run and step they branched from:

<!-- screenshot site/img/app-runs: {{home}}/runs/{{failing}} --compare {{home}}/runs/{{passing}} --step {{crash_step}} ;; click 785 27 ;; wait 3 -->

![The app's Runs panel: check's schedules, and the forks of the failing run, failing and passing](../site/img/app-runs.png)

## Find a run again

A command takes a run by its id or the start of one, by `@` for the newest
run and `@2`, `@3` and on for the ones before it, by a name for the newest
run with that name, or by its directory. `rewind ls` lists runs newest first,
and filters them:

```console run name=ls
$ rewind ls -n 3
$ rewind ls --forks-of {{failing|short}} --status failed
```

<!-- assert: rewind ls -n 1 | grep -q 'fork of {{failing}} at {{fork_from}}, schedule 4)' -->

`--name mylib` keeps the runs whose name contains `mylib`, and `--since` the
runs made in the last `30m`, `2h` or `7d`, or since a day such as
`2026-10-01`.
`--status` takes `passed`, `failed`, `timed-out`, `running`, `interrupted` or
`unreadable`, and `failed` takes timed-out runs too. A Nix run is named after
its derivation, and `--name` names any run.

`rewind open` starts the app on a run, at a step and beside another run.
`check` and `fork` print the command that opens what they made, as the last
line of `check` above shows. The failing run at the crash, beside the passing
one:

```console
$ rewind open {{failing|short}} {{crash_step}} --compare {{passing|short}}
```

## Rebuild a run exactly

A run's id is the hash of its inputs, which its `manifest.json` lists, so the
same command makes the same run on a machine with the same CPU vendor and
guest. `rewind show` prints that command with every input spelled out, the
epoch above all, which by default is the start of the day the run was made.
For a fork it prints its parents' commands first, each with the id it makes.
`@`, the newest run, is the schedule 4 fork:

```console run name=show elide
$ rewind show @
```

<!-- assert: grep -q '# {{failing}}$' {{out:show}} && grep -q '^rewind fork {{failing}} {{fork_from}} --schedule 4  # ' {{out:show}} -->

The failing run's command makes it again, with the same id:

```console run name=rebuild show=-1:
$ rewind show {{failing|short}} | tail -1 | sh
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

## Script it

`run`, `nix` and `fork` exit with the job's status, as a shell would report
it, so a shell loop can ask which schedules fail from a step:

```console run name=loop
$ for s in 5 6 7 8; do rewind fork {{failing|short}} {{fork_from}} --schedule $s --quiet 2>/dev/null; echo "schedule $s: exit $?"; done
```

<!-- assert: test $(grep -c '^schedule [5-8]: exit [0-9]*$' {{out:loop}}) -eq 4 -->

`check` exits 1 when a schedule ends differently and 0 when none does, `diff`
1 when the runs differ, `cat` 3 when the file did not exist at the step, and
`doctor` 1 when this machine cannot record runs. With `--json`, commands
print JSON for a program to read in place of their text: one object, or from
`ls` and `events` one a line.

```console run name=json
$ rewind ls --forks-of {{failing|short}} --status failed --json | jq -r .id
$ rewind diff {{passing|short}} {{failing|short}} --json | jq -c '.divergence | {left_step, right_step}'
```

<!-- assert: grep -q '"left_step":[0-9]*,"right_step":[0-9]*' {{out:json}} -->

## Move around a long run

Builds of larger projects run to hundreds of thousands of steps. In the app,
the step readout is a field: click it, or press g, and type a step such as
3,495, or +100 or -100 to move from the playhead. Previous and Next, and the
Left and Right keys, stop where the Stop at chooser beside them says: at every
event, the build log's lines, processes starting and exiting, the bookmarks,
or the thread, process, kind of event or file of the event at the playhead. A
click on a log line, a file's step or a process goes to its step.

Alt+Left and Alt+Right, or the mouse's back and forward buttons, go back and
forward through jumps, so a press of f or d can be undone. The b key
bookmarks the playhead's step with a note: the timeline marks it, the
Bookmarks tab lists every bookmark, and they are kept with the run. Ctrl+F or
/ searches the build log, the kernel's console, file paths and events, and
lists the matches by step.

The wheel over the timeline zooms around the pointer, down to 16 steps
across; Shift and the wheel pan, + and - zoom around the playhead, and 0 shows
the whole run. A label names the step under the pointer. The ? key, or the ?
button in the header, opens the sheet of every key:

<!-- screenshot site/img/app-keys: {{home}}/runs/{{failing}} --compare {{home}}/runs/{{passing}} --step {{crash_step}} ;; key question ;; wait 1 -->

![The app's sheet of keys: moving the playhead, the timeline, looking around, and the app](../site/img/app-keys.png)

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
Export button writes the replayable kind. Either kind carries the run's
bookmarks from the app, notes included.

## Which clock

```console run
$ rewind pmu status
```

With counter time the VM's clock follows the work done inside it; with exit
time computation takes no virtual time. `--clock` picks one. [Counter
time](pmu.md) explains. `rewind doctor` checks the clock with KVM, gdb and
the rest of what rewind uses, and says what to do about each problem.

## Find where a run hung

A thread gives up the VM's one CPU only at a step, so a loop that makes no
system call keeps it, and the run hangs. `--timeout` stops a run after that
many seconds on your machine. When the guest had gone a second or more
without an exit, rewind says where it was stuck. A program that spins, built
static with its symbols:

```console run hide
$ printf 'volatile unsigned long n;\nint main(void) { for (;;) n++; }\n' > spin.c
$ mkdir -p spin/bin
$ nix shell nixpkgs#pkgsStatic.stdenv.cc -c x86_64-unknown-linux-musl-cc -static -g -o spin/bin/spin spin.c
```

```console run name=spin show=-1:
$ rewind run --root spin --timeout 5 --name spin -- /bin/spin
```

<!-- assert: grep -q 'timed out computing without exits for [0-9.]*s, in user space in main[+0-9]* (spin.c:2), process [0-9]* (spin)' {{out:spin}} -->

The place is a function, offset and source line from the program's symbols,
and the process the VM's kernel had on the CPU. A run that timed out while
still making exits was slow rather than stuck, and says so. `rewind check`
gives each perturbed schedule ten times as long as schedule 0 took, and at
least a minute, and names the place the same way for a schedule that hit its
limit. `--status timed-out` lists such runs:

```console run
$ rewind ls --status timed-out
```

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
