# Tutorial: advanced features

The [Nix](tutorial-nix.md) and [container](tutorial-container.md) tutorials
follow one bug from a flaky test to its fix. This one is a set of short
recipes for what they leave out: watchpoints, the kernel's side of a crash,
your own gdb, tools inside the VM, more CPUs, steering the perturbation,
comparing and sharing runs, and keeping the run directory tidy. Each recipe
stands alone.

All of them use the same derivation, `github:fzakaria/rewindvm#mylib`, and
assume the Nix tutorial's install. Every transcript below is real output from
`rewind` on a 16 core AMD laptop, with `sudo rewind pmu enable` run since boot.

## The runs

The recipes start from one `rewind check`:

```console
$ rewind check github:fzakaria/rewindvm#mylib
schedule   0: exited:0             6176 steps  aa30ea54dc47  run 29485ce62b414b2d
schedule   1: exited:0             7214 steps  aa30ea54dc47  run ed6384f8632759ee
schedule   2: exited:0             7268 steps  aa30ea54dc47  run 81dfd5dc32c2650f
schedule   3: exited:2             5136 steps    run 4fa8c36f47f07ef3
...

schedule 3 ends differently; narrowing the steps it perturbs
perturbing only steps 4554..5081 still ends differently

passing: run 29485ce62b414b2d
failing: run 9427a02995aa7725
...
```

The failing run's test prints `job 28 done` from thread 173 at step 5099 and
takes its SIGSEGV at step 5102. The VM's clock at boot is one of a run's
inputs, and it defaults to the start of the day, UTC, so a check on another
day makes runs with other ids. [Rebuild a run exactly](#rebuild-a-run-exactly)
shows how to pin it.

## Watch both sides of the race

The tutorials stop gdb where the worker reads the freed queue. A watchpoint
also finds who freed it. Break in a worker so `p` is in scope, watch
`p->queue`, and continue:

```console
$ rewind gdb 9427a029 5060 -- -batch -ex 'break src/pool.c:74' -ex continue -ex 'watch -l p->queue' -ex 'delete 1' -ex continue -ex 'bt 2' -ex 'break src/pool.c:77 if p->queue == 0' -ex continue -ex 'bt 1'
rewind: step 5060 ran in process 166; loading symbols for 4 of its files
rewind: fetched 3 source files from the VM
rewind: gdb at step 5060 of 9427a02995aa7725
0xffffffff81285085 in __outl (value=<optimized out>, port=1504) at ./arch/x86/include/asm/shared/io.h:24
24	BUILDIO(l,  , u32)
Breakpoint 1 at 0x55bfaf4373a9: file src/pool.c, line 74.

Breakpoint 1, worker (arg=0x55bfe41ff010) at src/pool.c:74
74			if (!p->stopping) {
Hardware watchpoint 2: -location p->queue

Hardware watchpoint 2: -location p->queue

Old value = (struct queue *) 0x55bfe41ff090
New value = (struct queue *) 0x0
pool_shutdown (p=p@entry=0x55bfe41ff010) at src/pool.c:128
128			pthread_join(p->workers[i], NULL);
#0  pool_shutdown (p=p@entry=0x55bfe41ff010) at src/pool.c:128
#1  0x000055bfaf437272 in main () at tests/test_pool_shutdown.c:24
Breakpoint 3 at 0x55bfaf437433: file src/pool.c, line 77.

Breakpoint 3, worker (arg=0x55bfe41ff010) at src/pool.c:77
77				p->queue->completed++;
#0  worker (arg=0x55bfe41ff010) at src/pool.c:77
```

The first stop is `main` nulling the queue in `pool_shutdown`; the second is
the worker reading it. gdb shows line 128 because a data watchpoint traps
just after the instruction that wrote, here the one for `p->queue = NULL`.

Watchpoints and breakpoints are the CPU's four debug registers, so the VM
runs at full speed until one fires and nothing is written into its memory.
`watch` and `awatch` work; `rwatch` is refused, since x86 has no trap on reads
alone. Breakpoints and watchpoints at user addresses stop gdb only in the
process running at the step `rewind gdb` started at: another process that
maps the same address, such as a forked child running the same program, runs
past them. Threads share their process's memory, so the watch set in a worker
stops at `main`'s write.

## Follow the fault into the kernel

`rewind gdb` loads the VM kernel's symbols too, so a breakpoint can sit in
the kernel. At the step before the crash, stop where the kernel sends the
SIGSEGV, then use the kernel's own gdb scripts to list tasks and read its log:

```console
$ rewind gdb 9427a029 5099 -- -batch -ex 'break force_sig_fault' -ex continue -ex 'bt 4' -ex 'pipe lx-ps | tail -4' -ex 'pipe lx-dmesg | tail -2'
rewind: step 5099 ran in process 166; loading symbols for 4 of its files
rewind: fetched 3 source files from the VM
rewind: gdb at step 5099 of 9427a02995aa7725
0xffffffff81285085 in __outl (value=<optimized out>, port=1504) at ./arch/x86/include/asm/shared/io.h:24
24	BUILDIO(l,  , u32)
Downloading 133.41 K source file /build/linux-7.2.8/kernel/signal.c...
Breakpoint 1 at 0xffffffff812c3630: file kernel/signal.c, line 1757.

Breakpoint 1, force_sig_fault (sig=11, code=1, addr=0x108) at kernel/signal.c:1757
1757	{
#0  force_sig_fault (sig=11, code=1, addr=0x108) at kernel/signal.c:1757
#1  0xffffffff81e70210 in handle_page_fault (regs=0xffffc900001d3f58, error_code=6, address=264) at arch/x86/mm/fault.c:1483
#2  exc_page_fault (regs=0xffffc900001d3f58, error_code=6) at arch/x86/mm/fault.c:1536
#3  0xffffffff810012a6 in asm_exc_page_fault () at ./arch/x86/include/asm/idtentry.h:595
0xffff888003810fc0  162  bash
0xffff888003810000  166  test_pool_shutd
0xffff888003bdcec0  173  test_pool_shutd
0xffff888003bdbf00  174  test_pool_shutd
[    0.202894] test_pool_shutd[173]: segfault at 108 ip 000055bfaf437437 sp 00007f6157ca8e10 error 6 in test_pool_shutdown[1437,55bfaf437000+1000] likely on CPU 0 (core 0, socket 0)
[    0.202899] Code: fa 48 8d 35 2a 0c 00 00 bf 02 00 00 00 b8 00 00 00 00 e8 ac fc ff ff 48 8b 05 b5 2b 00 00 48 8b 38 e8 8d fc ff ff 49 8b 46 58 <83> 80 08 01 00 00 01 e9 b5 fe ff ff 55 48 89 e5 41
```

`addr=0x108` is the address the worker wrote through, and error code 6 is a
write from user mode to a page not present. `lx-ps`, `lx-dmesg` and the rest
of the kernel's scripts need its DWARF. With Nix, `rewind gdb` fetches the
kernel's `debug` output from `rewindvm.cachix.org` the first time, and serves
its source files through a debuginfod server of its own; that is the
`Downloading` line. Without Nix, unpack
`rewind-debug-x86_64-linux.tar.gz` from the same release next to
`rewind-x86_64-linux`; it holds the DWARF and the source files.

## Use your own gdb

`--listen` serves the fork on a port and starts no gdb. It prints the
command line that loads the same symbols, for a gdb started by an IDE, under
another user, or on another machine through a tunnel:

```console
$ rewind gdb 9427a029 5102 --listen 127.0.0.1:1234
rewind: step 5102 ran in process 166; loading symbols for 4 of its files
rewind: fetched 3 source files from the VM
rewind: gdb at step 5102 of 9427a02995aa7725; connect with: gdb -q -iex 'set debuginfod enabled on' -iex 'set debuginfod urls http://127.0.0.1:39763' -ex 'file /nix/store/vjncslv1ind0ani0jv2cy33icbw04ipd-rewind-guest-kernel-7.2.8-debug/lib/debug/vmlinux' ...
```

The session ends when that gdb detaches.

## Bring tools into the VM

`rewind shell --with` adds a package from Nix to the shell's `PATH`, built or
fetched on the host, without changing the run or rebuilding anything. Here
binutils disassembles the instruction the kernel reported:

```console
$ printf 'objdump -d --no-show-raw-insn --start-address=0x142c --stop-address=0x143e tests/test_pool_shutdown | tail -4; exit\n' | rewind shell 9427a029 5102 --pid 166 --with nixpkgs#binutils
rewind: a shell at step 5102 of 9427a02995aa7725; exit it to leave
[rewind] /build/mylib # objdump -d --no-show-raw-insn --start-address=0x142c --stop-address=0x143e tests/test_pool_shutdown | tail -4; exit
    142c:	mov    (%rax),%edi
    142e:	call   10c0 <fflush@plt>
    1433:	mov    0x58(%r14),%rax
    1437:	addl   $0x1,0x108(%rax)
```

`0x58(%r14)` loads `p->queue`, and the `addl` at offset 0x1437, the one the
kernel's report names, adds to `completed`, 0x108 bytes into it. `--with`
takes any installable, so `nixpkgs#strace` or `nixpkgs#gdb` work the same
way. It needs Nix on the host.

## Give programs more CPUs

The VM has one vCPU, and by default its programs see one CPU: a thread pool
sized by the CPU count starts one worker, and `NIX_BUILD_CORES=1` makes a Nix
build run one job at a time. `--cores` tells them there are more:

```console
$ rewind nix --cores 4 github:fzakaria/rewindvm#mylib
...
rewind: run 0204ae327d4206d5 exited:0 after 6182 steps, 0.216s virtual, 5.839s wall (poweroff)
/nix/store/f6a9gy362szw6nxx3ikrklr8glr6rdln-mylib-0.3.0 aa30ea54dc47c30f (not built on the host)

$ printf 'nproc; echo $NIX_BUILD_CORES; exit\n' | rewind shell 0204ae32 2471 --pid 110
rewind: a shell at step 2471 of 0204ae327d4206d5; exit it to leave
[rewind] /build/mylib # nproc; echo $NIX_BUILD_CORES; exit
4
4
```

The extra workers and jobs still interleave on the one vCPU, and the
schedules reorder them, which is how races between them come into reach. Go's
garbage collector is the common exception: above one core it waits for a
thread by spinning, and the VM stops making steps. Run Go programs with
`GOMAXPROCS=1`, or at `--cores 1`. [Design](design.md) has the details.

## Steer the perturbation

`rewind check` tries 64 perturbed schedules by default and stops after the
first batch in which a run ends differently. `--schedules` changes how many
it tries, and `--all` tries every one and counts the failures.

`rewind fork` asks a narrower question: from this step on, which schedules
fail? From step 5000 of the failing run, a hundred steps before the crash:

```console
$ rewind fork 9427a029 5000 --schedule 1 --quiet
rewind: run cc00d9ed118f2fb7 exited:0 after 6719 steps, 0.225s virtual, 5.345s wall (poweroff)
rewind: the fork first differs from its parent at step 5005

$ rewind fork 9427a029 5000 --schedule 2 --quiet
rewind: run 157cafba57f96210 exited:0 after 6578 steps, 0.220s virtual, 5.057s wall (poweroff)
rewind: the fork first differs from its parent at step 5009

$ rewind fork 9427a029 5000 --schedule 3 --quiet
rewind: run adf6c0918af18cce exited:2 after 5148 steps, 0.203s virtual, 4.918s wall (poweroff)
rewind: the fork first differs from its parent at step 5102

$ rewind fork 9427a029 5000 --schedule 4 --quiet
rewind: run e462890a9210bb74 exited:0 after 6608 steps, 0.222s virtual, 4.976s wall (poweroff)
rewind: the fork first differs from its parent at step 5003
```

Three of four schedules pass from there: by step 5000 the crash is likely but
not settled.

## Rebuild a run exactly

A run's id is the hash of its inputs, and the failing run's are in
`~/.local/share/rewind/runs/9427a02995aa7725/manifest.json`: schedule 3,
perturbing steps 4554 to 5081, booted with the clock at 1790985600, the start
of 2 October 2026, UTC. The same flags on `rewind nix` make the same run:

```console
$ rewind nix --quiet --epoch 1790985600 --schedule 3 --schedule-from 4554 --schedule-until 5081 github:fzakaria/rewindvm#mylib
rewind: run 9427a02995aa7725 exited:2 after 5144 steps, 0.203s virtual, 0.579s wall (poweroff)
```

`--schedule-from` and `--schedule-until` limit the perturbation to a window
of steps, which is how `check` narrows a failure. `--epoch` sets the VM's
clock at boot, so a run can be made again on another day.

## Compare any two runs

`check` compares only the failing program's own events. `rewind diff`
compares every event of any two runs, and shows where they first differ:

```console
$ rewind diff 29485ce6 9427a029
first difference at event 1614: step 4556 on the left, step 4558 on the right
  both        4533   166/166   clone(CLONE_THREAD) = 172
  both        4540   166/171   write(1, "round 1: ok\nworker picked job 0\n")
  both        4547   166/172   write(1, "worker picked job 1\n")
  left        4556   166/171   write(1, "job 0 done: 12727\n")
  left        4557   166/171   write(1, "worker picked job 2\n")
  left        4566   166/172   write(1, "job 1 done: 35269\n")
  left        4567   166/172   write(1, "worker picked job 3\n")
  right       4558   166/171   write(1, "job 0 done: 12727\n")
  right       4559   166/171   write(1, "worker picked job 2\n")
  right       4567   166/172   write(1, "job 1 done: 35269\n")
  right       4568   166/172   write(1, "worker picked job 3\n")
```

The first difference is in when, not what: the same lines in the same order,
two steps apart.

## Hand a failure to someone else

`rewind export --replayable` writes a run to one file, with its keyframes,
their pages, the input image and the VM's kernel:

```console
$ rewind export 9427a029 --replayable -o crash.rwd
rewind: wrote crash.rwd (176.6 MB)
```

On the other end, `rewind import` reads it, from a file or an http or https
URL, and the run replays exactly. A second run directory stands in for the
other machine here:

```console
$ REWIND_HOME=elsewhere rewind import crash.rwd
9427a02995aa7725  exited:2          5144 steps  mylib-0.3.0

$ REWIND_HOME=elsewhere rewind replay 9427a029
identical: 1734 events over 5144 steps
```

A run replays on machines with the same CPU vendor as the one that recorded
it: one recorded on AMD replays on AMD from Zen 2 on, and not on Intel.
Without `--replayable` the file holds the run's manifest and events, 17.9 KB
for this one: enough to read with `rewind log`, `ps` and `events` and to scrub
in the desktop app, but not to replay without the inputs. The app's Export
button writes the replayable kind.

## Which clock

`rewind pmu status` says whether this machine's branch counter can drive the
VM's clock:

```console
$ rewind pmu status
cpu: AMD Zen (family 25)
amd workaround (MSR 0xc0011020 bit 54): set by `rewind pmu enable` this boot
perf_event_paranoid: 2
self-test: 40046131 and 40046131 branches, exact at every exit
runs will use counter time
```

With counter time the VM's clock follows the work done inside it, and a
thread that computes for a long time is preempted as on real hardware. With
exit time, used when the counter is not exact, computation takes no virtual
time. `--clock exits` or `--clock branches` picks one; the clock is an input,
so the two make different runs. [Counter time](pmu.md) explains the
difference.

## Keep the run directory tidy

Runs are kept under `~/.local/share/rewind`, or `REWIND_HOME`. `rewind ls`
lists them newest first, with what each was forked from. Continuing in the
second directory after forking the imported run as above:

```console
$ REWIND_HOME=elsewhere rewind ls
adf6c0918af18cce  exited:2          5148 steps  mylib-0.3.0 (fork of 9427a02995aa7725 at 5000, schedule 3)
157cafba57f96210  exited:0          6578 steps  mylib-0.3.0 (fork of 9427a02995aa7725 at 5000, schedule 2)
e462890a9210bb74  exited:0          6608 steps  mylib-0.3.0 (fork of 9427a02995aa7725 at 5000, schedule 4)
cc00d9ed118f2fb7  exited:0          6719 steps  mylib-0.3.0 (fork of 9427a02995aa7725 at 5000, schedule 1)
9427a02995aa7725  exited:2          5144 steps  mylib-0.3.0

$ REWIND_HOME=elsewhere rewind remove 157cafba
removed 157cafba57f96210
```

`rewind remove` takes a run and every run forked from it. `rewind prune
--identical` removes the forks of a run whose trace is the same as an older
fork's, and `--dry-run` shows what either would remove first.

## What to read next

- [Design](design.md) explains how the machine is made deterministic, and
  where that stops; [its list of limits](design.md#limits) is the full one.
- [Counter time](pmu.md) explains how the VM's clock follows its work.
- The [case studies](case-studies/nix-gc-closure-sigpipe.md) use these
  tools on bugs in real projects.
