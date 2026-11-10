# Tutorial: advanced features

<!-- let epoch = 1790985600 -->
<!-- let flake = github:fzakaria/rewindvm#mylib -->

Short recipes for what the [Nix](tutorial-nix.md) and
[container](tutorial-container.md) tutorials leave out, each in the terminal
and in the desktop app. They use the Nix tutorial's runs and install.

## The runs

```console run name=check
$ rewind check --epoch {{epoch}} {{flake}} | grep -E 'ends differently|limited to|decides it|passing:|failing:|same run until|open both'
```

<!-- capture passing: passing: run (\w+) -->
<!-- capture failing: failing: run (\w+) -->
<!-- capture deciding: step (\d+) decides it -->
<!-- capture deciding_words: decides it: (.+) there makes the run fail -->
<!-- capture window_from: limited to steps (\d+)\.\. -->
<!-- set crash_step: rewind events {{failing}} | grep -m1 SIGSEGV | awk '{print $1}' -->
<!-- set pid: rewind events {{failing}} | grep -m1 SIGSEGV | awk '{print $2}' | cut -d/ -f1 -->
<!-- set crash_tid: rewind events {{failing}} | grep -m1 SIGSEGV | awk '{print $2}' | cut -d/ -f2 -->
<!-- set last_write: rewind events {{failing}} --to {{crash_step}} | grep -E " {{pid}}/{{crash_tid}} +write" | tail -1 | awk '{print $1}' -->
<!-- set watch_from: rewind events {{failing}} --from $(( {{crash_step}} - 50 )) --to {{crash_step}} | grep -E " {{pid}}/[0-9]+ +write" | head -1 | awk '{print $1}' -->

The failing run crashes at step {{crash_step}}.

## Find the line a thread was on

`rewind where` names the line of the program's own code a thread was on at a
step, skipping the C library and, in Rust, the standard library and
dependencies. By default it picks the thread of the step's event; at {{crash_step}}
that is the worker that segfaulted:

```console run name=where
$ rewind where {{failing|short}} {{crash_step}}
```

<!-- assert: grep -q '^#0 worker (src/pool.c:77)' {{out:where}} && grep -q '^> *77 .*p->queue->completed++;' {{out:where}} && grep -q 'start_thread' {{out:where}} && grep -q 'clone3' {{out:where}} -->
<!-- assert: grep -q '^  p = 0x' {{out:where}} -->
<!-- capture crash_job: (?m)^  job = (\d+)$ -->

Under the source come the frame's arguments and locals, as gdb's `info args`
and `info locals` print them: the worker had just reported job {{crash_job}}.

`--tid` picks another thread, on the CPU or not. At step {{crash_step}}, where
the worker faults, the main thread is waiting to join the workers:

```console run name=where_main
$ rewind where {{failing|short}} {{crash_step}} --tid {{pid}}
```

<!-- assert: grep -q 'pool_shutdown (src/pool.c:128)' {{out:where_main}} && grep -q 'pthread_join' {{out:where_main}} -->

`--json` prints every frame, with its arguments and locals, where each is
and which one was chosen. For every frame of every thread, ask gdb for all
the stacks of the process:

```console run name=all_bt
$ rewind gdb {{failing|short}} {{crash_step}} --pid {{pid}} -- -batch -ex 'thread apply all bt' 2>/dev/null | grep -E '^Thread|src/|tests/'
```

<!-- assert: grep -q 'in pool_shutdown .* at src/pool.c:128' {{out:all_bt}} && grep -q 'in main () at tests/test_pool_shutdown.c' {{out:all_bt}} -->

<!-- set main_frame: rewind where {{failing}} {{crash_step}} --tid {{pid}} --json 2>/dev/null | jq .chosen -->

`rewind gdb` starts in the thread `rewind where` looks at, in its innermost
frame. `--tid` and `--frame` start it elsewhere, with frames numbered as
`where --json` numbers them. In the main thread's frame that `where` chose,
`pool_shutdown` has already set the queue to NULL:

```console run name=gdb_frame
$ rewind gdb {{failing|short}} {{crash_step}} --tid {{pid}} --frame {{main_frame}} -- -batch -ex 'p p->queue'
```

<!-- assert: grep -q 'in pool_shutdown .* at src/pool.c:128' {{out:gdb_frame}} && grep -q '(struct queue \*) 0x0' {{out:gdb_frame}} -->

`where` and gdb read each thread's registers from the VM kernel's task list,
so they work on runs recorded with a guest that lists its tasks, as these
were.

In the app, the s key opens the source panel at the line `rewind where` names,
with the frame's variables and the thread's frames below it; click a frame to
see its line and variables instead. The panel follows the playhead, and each
new step takes a few seconds since it forks the run.

<!-- screenshot site/img/app-source: {{home}}/runs/{{failing}} --compare {{home}}/runs/{{passing}} --step {{crash_step}} --source ;; wait 20 -->

![The app's source panel at step {{crash_step}} of the failing run: worker at src/pool.c:77 with p->queue->completed++ marked and the values that can be blamed underlined, the worker's arguments and locals, and the frames worker, start_thread and clone3 below](../site/img/app-source.png)

## Find who wrote a value

The worker crashed because `p->queue` was NULL. `rewind blame` finds the
write that put a value there. Give it a step and an expression. It works out
the expression's address in the thread that `where` picks. Then it replays the
stretch of the run before the step, with that address in the CPU's debug
registers. If the stretch holds no write, it tries the stretch before that:

```console run name=blame
$ rewind blame {{failing|short}} --at {{crash_step}} 'p->queue'
```

<!-- assert: grep -q '^p->queue = 0x0 ' {{out:blame}} && grep -q 'pool.c:125  pool_shutdown  p->queue = NULL;' {{out:blame}} && grep -qE "^  thread {{crash_tid}}  pool.c:75  worker" {{out:blame}} -->
<!-- capture blame_step: last written at step (\d+) -->
<!-- set blame_gap: echo $(( {{crash_step}} - {{blame_step}} )) -->

The main thread set `p->queue` to NULL on line 125, in `pool_shutdown`. It did
so at step {{blame_step}}, {{blame_gap}} steps before the crash. blame also
prints the value before the write.

The last part shows where the other threads were just after the write. Thread
{{crash_tid}} is the worker that crashes later. It was on line 75, printing. It
had already passed the check of `p->stopping` on line 74.

A step runs from one exit of the VM to the next, so a step can cover many
lines of the program. At the start of step {{blame_step}}, the main thread had
not yet called `pool_shutdown`. `rewind where --after-write` looks just after
the write itself, with the address and size blame printed:

<!-- set queue_address: rewind blame {{failing}} --at {{crash_step}} 'p->queue' --json 2>/dev/null | jq -r '.values[0].address' -->

```console run name=where_write
$ rewind where {{failing|short}} {{blame_step}} --tid {{pid}} --after-write {{queue_address}} --size 8
```

<!-- assert: grep -q '^#0 pool_shutdown (src/pool.c:125)' {{out:where_write}} && grep -q '^> *125 .*p->queue = NULL;' {{out:where_write}} -->

`--writes 2` goes back one more write. Here that is the `calloc` in
`pool_create`.

blame can only watch values in memory. gdb keeps some values in registers, such
as `job` here. For those, blame says it cannot watch them and lists the values
on the same line that it can watch.

`--reads` lists the reads as well as the writes. It takes several expressions
at once. It searches back to the start of the process, or to the step given
with `--since`. Here it searches the last 40 steps. The output below keeps only
lines 74 and 77 of the worker and lines 119 and 125 of `pool_shutdown`:

<!-- set blame_since: echo $(( {{crash_step}} - 40 )) -->

```console run name=blame_reads keep=step\s\sthread|pool.c:(74|77|119|125)\s|^searched
$ rewind blame {{failing|short}} --at {{crash_step}} 'p->stopping' 'p->queue' --reads --since {{blame_since}}
```

<!-- assert: awk '/ {{crash_tid}} [^ ]+ +read +0 +pool.c:74 / && !r {r=NR} / write +1 +pool.c:119 / {w=NR} / {{crash_tid}} [^ ]+ +read +0x0 +pool.c:77 / {c=NR} END {exit !(r && w && c && r < w && w < c)}' {{out:blame_reads}} -->

Read in order, the rows show the race:

1. Thread {{crash_tid}} reads `p->stopping` as 0 on line 74. So it goes on to
   count its job.
2. The main thread sets `p->stopping` to 1, then sets `p->queue` to NULL.
3. Thread {{crash_tid}} reads `p->queue` on line 77 and gets NULL.

The worker does not hold the lock between line 74 and line 77. Nothing stops
the main thread from running in between.

In the app, the source panel underlines each value that blame can watch.
Hover over a value to see where it is kept, or why blame cannot watch it. Click
it to search. You can keep using the app while the search runs. The popover
counts the steps searched so far, and the timeline marks the stretch being
searched.

The answer names the write, the value before it, and where the other threads
were. Click the write, or the button, to see the writer just after the write;
click another thread to see it at that moment. The other buttons show the reads
with one lane per thread, or blame the write before this one. The popover moves
by its top line. At the step where the run failed, the panel
starts searching for the failing line's values before you click.

<!-- screenshot site/img/app-blame: {{home}}/runs/{{failing}} --compare {{home}}/runs/{{passing}} --step {{crash_step}} --source ;; wait 20 ;; click 1090 477 ;; wait 6 -->

![The app's source panel at step {{crash_step}} with the answer for p->queue: it became 0x0 at step {{blame_step}}, written by the main thread in pool_shutdown at pool.c:125, p->queue = NULL, with the value before it, buttons to go to the write, show the reads and blame the earlier value, and where the other threads were](../site/img/app-blame.png)

## Find the step that decides it

`check` names step {{deciding}} as the one that decides it. Its passing run is the
same schedule without that step, so the two runs are the same machine until
{{deciding}}, where only the failing run gets {{deciding_words}}, and everything
they do differently follows from it. `rewind where` names the thread that was
on the CPU there:

<!-- set gap: echo $(( {{crash_step}} - {{deciding}} )) -->

```console run name=decides
$ rewind where {{failing|short}} {{deciding}}
```

<!-- assert: grep -q 'worker (src/pool.c:75)' {{out:decides}} && grep -q 'if (!p->stopping) {' {{out:decides}} -->

A worker checks `p->stopping` without the lock, prints its job, and then
counts it through `p->queue`, which `pool_shutdown` frees. At step
{{deciding}} the worker has made its check and is printing; {{deciding_words}}
there holds it before the count. The test's main thread runs meanwhile, sees
half the jobs done, and shuts the pool down, setting the queue to NULL, and
the worker faults on it {{gap}} steps later. Without that step's
perturbation, the worker counts first and the test passes.

`check --where` looks up both threads itself: the one on the CPU at the
deciding step, and the one at the failing run's first differing event. Each
lookup forks the run and starts gdb, so it is opt-in:

```console run name=check_where
$ rewind check --where --epoch {{epoch}} {{flake}} 2>/dev/null | sed -n '/^where the threads were/,/first event that differs$/p'
```

<!-- capture held_tid: \d+/(\d+) +worker \(src/pool.c:75\), on the CPU at the deciding step -->
<!-- capture diff_step: (\d+) +\d+/\d+ +worker \(src/pool.c:75\), at the first event that differs -->
<!-- capture diff_tid: \d+/(\d+) +worker \(src/pool.c:75\), at the first event that differs -->
<!-- assert: test {{held_tid}} = {{crash_tid}} && test {{diff_tid}} != {{crash_tid}} && test {{diff_step}} -lt {{crash_step}} -->
<!-- set diff_job: rewind events {{failing}} --from {{diff_step}} --to {{diff_step}} | grep -oE '{{pid}}/{{diff_tid}} +write\(1, "job [0-9]+ done' | grep -oE '[0-9]+ done' | cut -d' ' -f1 -->
<!-- set next_job: rewind events {{failing}} --from {{diff_step}} --to {{crash_step}} | grep -oE '{{pid}}/{{diff_tid}} +write\(1, "worker picked job [0-9]+' | head -1 | grep -oE '[0-9]+$' -->
<!-- assert: rewind events {{passing}} --from {{deciding}} --to {{diff_step}} | grep -q '{{pid}}/{{crash_tid}} *write(1, "worker picked job {{next_job}}' -->
<!-- set late_gap: echo $(( {{crash_step}} - {{diff_step}} )) -->

The first event that differs is the other worker, thread {{diff_tid}}, printing
job {{diff_job}} done from line 75. With thread {{crash_tid}} held at the
deciding step, the other worker finishes its job and picks job {{next_job}},
where in the passing run thread {{crash_tid}} counted job {{crash_job}} and
picked job {{next_job}} first. That is the cause showing up before the
symptom: the held thread's fault at line 77, which `rewind where` showed at
the top of this page, comes {{late_gap}} steps later, at step {{crash_step}}.

In the app, the deciding step is a dashed blue mark on the timeline, before
the solid one where the runs' events first differ. Zoomed in around the crash:

<!-- screenshot site/img/app-decides: {{home}}/runs/{{failing}} --compare {{home}}/runs/{{passing}} --step {{crash_step}} ;; key plus ;; key plus ;; key plus ;; key plus ;; key plus ;; key plus ;; key plus ;; key plus ;; key plus ;; key plus ;; key plus ;; key plus ;; key plus ;; key plus -->

![The app zoomed in around the crash: a dashed blue mark at step {{deciding}} on the timeline, a solid one at step {{diff_step}} where the events first differ, the crash after them, and the divergence card saying the two runs are identical until step {{deciding}}, where only this run gets {{deciding_words}}](../site/img/app-decides.png)

## See which thread held the CPU

The VM has one CPU, so a race is an order of threads. Every context switch
in the VM is a step of its own, and `rewind threads` prints which thread held
the CPU over a window of steps, a line per stretch. Around the deciding step,
in both runs:

<!-- set threads_from: echo $(( {{deciding}} - 9 )) -->

```console run name=threads_failing
$ rewind threads {{failing|short}} --from {{threads_from}} --to {{crash_step}}
```

```console run name=threads_passing
$ rewind threads {{passing|short}} --from {{threads_from}} --to {{crash_step}}
```

<!-- assert: head -1 {{out:threads_failing}} | grep -q . && test "$(head -1 {{out:threads_failing}})" = "$(head -1 {{out:threads_passing}})" -->
<!-- assert: tail -1 {{out:threads_failing}} | grep -qE " {{crash_step}} +{{pid}}/{{crash_tid}} " -->
<!-- assert: ! diff -q {{out:threads_failing}} {{out:threads_passing}} > /dev/null -->

The lines match up to step {{deciding}}. From there the threads take the CPU in
another order, and in the failing run thread {{crash_tid}} holds it at the crash.
Every switch is a step, so no stretch is too short to appear.

In the app, the t key opens the Threads tab: a lane per thread around where
the two runs part, this run's above the compared run's, with a bar wherever
the thread held the CPU. Ctrl and the wheel zoom the lanes and Shift and the
wheel pan them. Clicking a bar moves the playhead there. The band under the
timeline is darker where the switches are dense, which says where to look,
and the Stop at menu offers the switches of the thread on the CPU at the
playhead, so Next goes to where that thread took the CPU or lost it:

<!-- screenshot site/img/app-threads: {{home}}/runs/{{failing}} --compare {{home}}/runs/{{passing}} --step {{crash_step}} ;; key t ;; wait 8 -->

![The app's Threads tab around the crash: a lane per thread of test_pool_shutdown, the failing run's above the passing run's, with the worker that crashes taking the CPU in another order after the deciding step](../site/img/app-threads.png)

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

<!-- screenshot site/img/app-gdb-watch: {{home}}/runs/{{failing}} --compare {{home}}/runs/{{passing}} --step {{watch_from}} ;; click 1140 756 ;; wait 25 ;; type break src/pool.c:74 ;; wait 1 ;; type continue ;; wait 8 ;; type watch -l p->queue ;; wait 1 ;; type delete 1 ;; wait 1 ;; type continue ;; wait 8 -->

![The app's gdb pane stopped at the watchpoint in pool_shutdown, with the old and new values of p->queue](../site/img/app-gdb-watch.png)

## Run backwards to the write

gdb gives the same answer as blame and can go on from there: it can run the
fork backwards. From the crash itself, watch the pointer the worker read and
run back to the last write to it:

```console run name=reverse
$ rewind gdb {{failing|short}} {{crash_step}} -- -batch -ex 'watch -l p->queue' -ex reverse-continue -ex 'bt 2'
```

<!-- assert: grep -q 'Old value = (struct queue \*) 0x0' {{out:reverse}} && grep -q 'pool_shutdown (p=.*) at src/pool.c:125' {{out:reverse}} && grep -q 'in main ()' {{out:reverse}} -->

The fork stops before `main` nulls the queue in `pool_shutdown`. Going back,
gdb shows the values swapped: the old value is the one after the write.
`reverse-stepi`, `reverse-next`, `reverse-finish` and `reverse-continue` to a
breakpoint work too. Rewind replays the run from the step's start to go back,
so a step back takes a moment, and longer under exit time, which single-steps
the way. A fork whose memory or registers gdb changed cannot go back.

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

In the app, Open shell starts a shell at the playhead in the terminal pane,
in the crashing process's working directory:

<!-- screenshot site/img/app-shell: {{home}}/runs/{{failing}} --compare {{home}}/runs/{{passing}} --step {{crash_step}} ;; click 1330 756 ;; wait 25 ;; type pwd ;; wait 1 ;; type ls tests ;; wait 2 -->

![The app's shell pane at step {{crash_step}} of the failing run, in /build/mylib, after pwd and ls tests](../site/img/app-shell.png)

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

Go programs see the extra CPUs too, and Go's garbage collector can then
spin waiting for a thread that never runs. Run them with `GOGC=off` if they
fit in the VM's memory.

## Steer the perturbation

<!-- set fork_from: d={{work}}/fork-search; rm -rf $d; mkdir -p $d; for x in runs store cache; do cp -a {{home}}/$x $d/; done; ln -s {{home}}/images $d/images; for back in 100 80 60 40 30 20 10; do s=$(( {{crash_step}} - back )); mixed=$(for k in 1 2 3 4; do REWIND_HOME=$d rewind fork {{failing}} $s --schedule $k --json 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin)["status"])'; done | python3 -c 'import sys; s=[int(l) != 0 for l in sys.stdin]; print(int(any(s) and not all(s)))'); if [ "$mixed" = 1 ]; then echo $s; break; fi; done; rm -rf $d -->
<!-- assert: test -n "{{fork_from}}" -->

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

Fork from here forks the run at the playhead with the next schedule. Clicking
a run opens it compared with the run it hangs under; right-clicking offers
more.

## Find a run again

A command takes a run by its id or a prefix of it, by `@` for the newest run
and `@2`, `@3` and on for the ones before it, by name, or by directory.
`rewind ls` lists runs newest first, and filters them:

```console run name=ls
$ rewind ls -n 3
$ rewind ls --forks-of {{failing|short}} --status failed
```

<!-- assert: rewind ls -n 1 | grep -q 'fork of {{failing}} at {{fork_from}}, schedule 4)' -->

`--name` and `--since` (`30m`, `2h`, `7d` or a date) filter too.

`rewind open` starts the app on a run, at a step and beside another run.
`check` and `fork` print the command that opens what they made:

```console
$ rewind open {{failing|short}} {{crash_step}} --compare {{passing|short}}
```

## Rebuild a run exactly

A run's id is the hash of its inputs, which its `manifest.json` lists, so the
same command follows the same run on a machine with the same CPU vendor and
guest. `rewind show` prints that command with every input spelled out,
including the epoch, which defaults to the start of the day the run was made.
For a fork it prints its parents' commands first. `@`, the newest run, is the
schedule 4 fork:

```console run name=show elide show=:1,2:
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

The card's Compare side by side link opens the Compare tab: the last events
both runs shared, then each run's next events side by side, with what differs
in amber. The x key swaps to the other run at the matching step, and x again
comes back:

<!-- screenshot site/img/app-compare-tab: {{home}}/runs/{{failing}} --compare {{home}}/runs/{{passing}} ;; key d ;; click 1153 221 ;; wait 2 -->

![The app's Compare tab: the writes both runs made before they part, then the other worker's exit in the failing run beside the passing run's write, and the failing run's SIGSEGV a few events below, each run's text in full with what differs in amber](../site/img/app-compare-tab.png)

## Script it

`run`, `nix` and `fork` exit with the job's status, as a shell would report
it, so a shell loop can ask which schedules fail from a step:

```console run name=loop
$ for s in 5 6 7 8; do rewind fork {{failing|short}} {{fork_from}} --schedule $s --quiet 2>/dev/null; echo "schedule $s: exit $?"; done
```

<!-- assert: test $(grep -c '^schedule [5-8]: exit [0-9]*$' {{out:loop}}) -eq 4 -->

`check` exits 1 when a run fails a property, by default by ending
differently from schedule 0, and 0 when none does, `diff`
1 when the runs differ, `cat` 3 when the file did not exist at the step and
4 when the path named a directory, which `cat --archive tar` writes whole,
`files` 3 when the directory did not exist and 4 when the path named
something else, and `doctor` 1 when this machine cannot record runs. Every
command exits 2 when it could not do what was asked, as for a run that does
not exist, so `rewind diff a b || report_flaky` reports only runs that differ. With `--json`, commands
print JSON for a program to read in place of their text: one object, or from
`ls` and `events` one a line. `check --json` lists each run it tried under
`schedules`, with the properties the run violated under `violations`.

```console run name=json
$ rewind ls --forks-of {{failing|short}} --status failed --json | jq -r .id
$ rewind diff {{passing|short}} {{failing|short}} --json | jq -c '.divergence | {left_step, right_step}'
```

<!-- assert: grep -q '"left_step":[0-9]*,"right_step":[0-9]*' {{out:json}} -->

## Judge runs by properties

By default `check` fails a run that ends differently from schedule 0.
`--property` replaces that test with others, such as `no-crash` or
`single-writer=GLOB`, and can be repeated. `rewind check --help` lists the
built-in properties, and the [properties tutorial](tutorial-properties.md)
explains each one.

A program that calls the [Rewind
SDK](https://github.com/fzakaria/rewindvm/tree/main/sdk) asserts from the
inside: `always` must hold every time it runs, and `sometimes` must hold in
at least one run. Check adds the `sdk` property when schedule 0 calls the
SDK, and the app marks each failed assertion in red above the track.

A property can also be a script. It gets the run in `$REWIND_RUN`, can read
it with `rewind log`, `rewind events --json` or `rewind cat`, and exits 0
when the run is correct or 1 with the reason on its first line. This one
checks that `bank`'s deposits add up:

<!-- run: printf '%s\n' '#!/bin/sh' '# The deposits add up: bank prints its balance and what it expected.' 'line=$(rewind log "$REWIND_RUN" | grep "^balance ")' 'test "$line" = "balance 400, expected 400" && exit 0' 'echo "$line"' 'exit 1' > adds-up.sh && chmod +x adds-up.sh -->
<!-- run: printf '%s\n' '# What a correct bank run is.' 'no-crash' 'script=adds-up.sh' > rewind.properties -->

```console run name=script_cat
$ cat adds-up.sh
```

```console run name=script
$ rewind check --epoch {{epoch}} --property script=./adds-up.sh github:fzakaria/rewindvm#bank | grep -E 'decides it|failing run violates adds-up'
```

<!-- assert: grep -qE '^failing run violates adds-up: balance [0-9]+, expected 400$' {{out:script}} -->

`--properties` reads properties from a file, one a line, so a repository
can keep them beside its tests. A script's path in the file is relative to
the file's directory:

```console run name=properties
$ cat rewind.properties
$ rewind check --epoch {{epoch}} --properties rewind.properties github:fzakaria/rewindvm#bank | grep 'decides it'
```

<!-- assert: grep -q ' decides it: ' {{out:properties}} -->

## Count the schedules that fail from a step

`rewind fork` tries one schedule from a step. `check --run` tries many, each a
fork at `--schedule-from`, with the run itself as schedule 0. `--no-narrow`
stops at the count. From the deciding step of the passing run:

```console run name=sweep
$ rewind check --run {{passing|short}} --schedule-from {{deciding}} --schedules 16 --all --no-narrow
```

<!-- assert: grep -qE '^[0-9]+ of 16 other schedules ended differently$' {{out:sweep}} -->
<!-- capture sweep_differing: ([0-9]+) of 16 other schedules ended differently -->

{{sweep_differing}} of the 16 crash. Without `--no-narrow`, `check --run` narrows the first of
them to the step that decides it, as for a build.

In the app, the chevron beside Fork from here turns it into Check from here,
which runs the schedules from the playhead and fills a cell per schedule:
grey for ended as this run did, blue for differently. Pick a cell to compare
with or open its run:

<!-- screenshot site/img/app-check: {{home}}/runs/{{passing}} --step {{deciding}} ;; click 1408 168 ;; wait 1 ;; click 1296 261 ;; wait 1 ;; click 1300 168 ;; wait 40 ;; click 1075 492 ;; wait 1 -->

![The app after Check from here at step {{deciding}} of the passing run: a card counting how the 16 schedules ended, a cell per schedule, the first picked with its run and Compare and Open](../site/img/app-check.png)

## Hold a task off the CPU for longer

A stall holds the running task off the CPU for at most 1.28 ms. A race whose
window includes a sleep, a timer or slow I/O in another task needs a longer
hold. This program gives eight threads 20 ms to start, built static:

```console run hide
$ printf '#include <pthread.h>\n#include <stdio.h>\n#include <time.h>\n#include <unistd.h>\nvolatile int ready[8];\nvoid *work(void *i) { write(1, "started\\n", 8); ready[(long)i] = 1; return 0; }\nint main(void) {\n  pthread_t t[8];\n  for (long i = 0; i < 8; i++) pthread_create(&t[i], 0, work, (void *)i);\n  nanosleep(&(struct timespec){0, 20000000}, 0); /* 20 ms to start */\n  for (int i = 0; i < 8; i++) if (!ready[i]) { printf("worker %%d not ready\\n", i); return 1; }\n  puts("every worker ready");\n}\n' > ready.c
$ mkdir -p ready/bin
$ nix shell nixpkgs#pkgsStatic.stdenv.cc -c x86_64-unknown-linux-musl-cc -static -o ready/bin/ready ready.c
```

No schedule stalls a thread for 20 ms by default:

```console run name=short_stalls show=-1:
$ rewind check --root ready --schedules 64 --all --no-narrow -- /bin/ready
```

<!-- assert: grep -q '^same result under all 65 schedules$' {{out:short_stalls}} -->

`--stall-max` on `run`, `nix`, `check` and `fork` raises the longest stall,
up to 1 s:

```console run name=long_stalls
$ rewind check --root ready --stall-max 50ms -- /bin/ready | grep -E 'ends differently|limited to|decides it|passing:|failing:'
```

<!-- assert: grep -qE 'decides it: .*[0-9.]+ ms pause of the running thread there makes the run fail$' {{out:long_stalls}} && grep -q 'stalls up to 50 ms$' {{out:long_stalls}} -->

<!-- set mylib_default: rewind check --epoch {{epoch}} --schedules 32 --all --no-narrow {{flake}} 2>/dev/null | tail -1 | awk '{print $1}' -->
<!-- set mylib_long: rewind check --epoch {{epoch}} --schedules 32 --all --no-narrow --stall-max 100ms {{flake}} 2>/dev/null | tail -1 | awk '{print $1}' -->
<!-- set mylib_default_secs: TIMEFORMAT=%R; { time rewind check --epoch {{epoch}} --schedules 32 --all --no-narrow {{flake}} > /dev/null 2>&1; } 2>&1 | xargs printf '%.1f' -->
<!-- set mylib_long_secs: TIMEFORMAT=%R; { time rewind check --epoch {{epoch}} --schedules 32 --all --no-narrow --stall-max 100ms {{flake}} > /dev/null 2>&1; } 2>&1 | xargs printf '%.1f' -->

The same steps stall, one exit in 128, but each stall picks one of the
doublings of 10 µs up to the maximum, 14 of them for 50 ms, so fewer stalls
are short. How often a race turns up then depends on how long it needs a
thread held: 32 schedules of mylib crash {{mylib_default}} times by default and
{{mylib_long}} times with `--stall-max 100ms`, in {{mylib_default_secs}} s and {{mylib_long_secs}} s. A stall is virtual time, so the guest's clock
moves further, and a program's own timeouts see the extra time. The maximum is
part of each run's spec, so `rewind show`, a replay and a fork of the run keep
the maximum, and a run made without the flag keeps its id. A fork's own
schedule takes the default unless it is given `--stall-max` too.

## Try other seeds from a step

A fork keeps its parent's randomness unless you reseed it. `rewind fork
--seed` reseeds the VM's randomness at the fork's step. `check --run
--seeds` tries the run's own seed and several others, each a fork of the
run reseeded at `--schedule-from`. `hash-order`'s test depends on the order
of a Rust `HashSet`, which Rust draws from that randomness:

```console run name=hash_run show=-1:
$ rewind nix -q --epoch {{epoch}} --name hash-order github:fzakaria/rewindvm#hash-order
```

```console run name=reseed
$ rewind check --run hash-order --seeds 8 --schedules 0 | grep -E 'decides it|passing:|failing:'
```

<!-- assert: grep -qE 'decides it: under schedule 0, seed [0-9]+ passes and seed [0-9]+ fails$' {{out:reseed}} && grep -qE 'seed [0-9]+ from step [0-9]+, schedule 0$' {{out:reseed}} -->

## List directories in another order

Inside the VM a directory lists its entries in the same order in every run,
so a build that depends on that order never differs there. On a real
machine, ext4 lists a directory in an order set when the disk was formatted.
The kernel argument `rewind.dirents=N` lists every directory the build sees
in the order of a hash of each name keyed by N, as another ext4 disk would;
0 keeps the usual order. The key is part of the run, so a run made with one
replays:

```console
$ rewind nix --kernel-args rewind.dirents=2 {{flake}}
```

`rewind check --dirents 3` builds under keys 1 to 3 besides the stock
order, a line each, and reports the first key whose build ends differently;
`rewind diff --outputs` shows what it changed. setuptools 80.9.0 builds a
wheel that names its own version as `1.0` under key 2, because it reads its
version from whichever `.egg-info` directory lists first.
[Directory order](design.md#directory-order) in the design notes says how
the kernel does it.

## Move around a long run

Builds of larger projects run to hundreds of thousands of steps. The app has
keys to jump to a step, step between events of one kind, bookmark, search and
zoom the timeline. The ? key lists them all:

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
Export button writes the replayable kind. Both kinds carry the run's
bookmarks.

The other machine also needs the features of the run's CPU model. A run
without `--cpu` gets `v3`, whose features include ERMS, the fast `rep movsb`
flag. On a host that lacks only ERMS it gets `v3-compat` instead, the same CPU
with ERMS hidden, and rewind prints a line saying so. A `v3` run needs a host
with ERMS, and a `v3-compat` run replays on hosts with or without it. A replay
on a host without the model's features names what is missing and where the
run replays, and `rewind doctor` says which model a run gets on a machine.

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

A run that timed out while still making exits was slow rather than stuck, and
says so. `rewind check` gives each perturbed schedule ten times as long as
schedule 0 took, at least a minute, and reports a stuck schedule the same
way. Schedule 0 itself has no limit, since a long build is no reason to stop
it, so a test suite that hangs there holds up the whole check. Give such a
suite `rewind check --timeout`, which bounds every schedule, schedule 0
included. `--status timed-out` lists the runs that timed out:

```console run
$ rewind ls --status timed-out
```

## Keep the run directory tidy

<!-- run: for s in 1 2; do REWIND_HOME=elsewhere rewind fork {{failing}} {{fork_from}} --schedule $s --quiet || true; done -->
<!-- set removed: REWIND_HOME=elsewhere rewind ls | head -1 | awk '{print $1}' -->

Runs live under `~/.local/share/rewind`, or `REWIND_HOME`:

```console run
$ REWIND_HOME=elsewhere rewind ls
$ REWIND_HOME=elsewhere rewind remove {{removed|short}}
```

`remove` takes a run with every run forked from it. To remove every fork and
keep the runs they came from, run
`rewind ls | awk '/fork of/ {print $1}' | xargs rewind remove`.
`prune --identical` removes forks that ran exactly as an older one did.

## What to read next

- [Design](design.md): how the machine is made deterministic, and its
  [limits](design.md#limits).
- The [case studies](case-studies/nix-gc-closure-sigpipe.md): these tools on
  bugs in real projects.
