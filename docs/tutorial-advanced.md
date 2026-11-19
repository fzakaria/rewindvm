# Tutorial: advanced features

Short recipes for what the [Nix](tutorial-nix.md) and
[container](tutorial-container.md) tutorials leave out, each in the terminal
and in the desktop app. They use the Nix tutorial's runs and install.

## The runs

```console
$ rewind check --epoch 1790985600 github:fzakaria/rewindvm#mylib | grep -E 'ends differently|limited to|decides it|passing:|failing:|same run until|open both'
rewind: packing 62 store paths for mylib-0.3.0
schedule 7 ends differently from schedule 0; finding the step that decides it
schedule 7 limited to steps 2818..4978 still ends differently
step 4977 decides it: a request to switch threads there makes the run fail
passing: run 6a609b9c1faefbc8, schedule 7 over steps 2818..4977
failing: run e20758b82c63ae48, schedule 7 over steps 2818..4978
the two are the same run until step 4977
open both in the desktop app: rewind open e20758b82c63ae48 4981 --compare 6a609b9c1faefbc8
```

The failing run crashes at step 4992.

## Find the line a thread was on

`rewind where` names the line of the program's own code a thread was on at a
step, skipping the C library and, in Rust, the standard library and
dependencies. By default it picks the thread of the step's event; at 4992
that is the worker that segfaulted:

```console
$ rewind where e20758b8 4992
rewind: process 166 at step 4992; loading symbols for 4 of its files
rewind: fetched 3 source files from the VM
rewind: walking thread 170's stack in gdb
rewind: downloading debug info for libc.so.6; first time only
rewind: downloading debug info for libpthread.so.0; first time only
rewind: downloading debug info for ld-linux-x86-64.so.2; first time only
rewind: downloading source files of libc.so.6; each one only once
process 166 (test_pool_shutdown), thread 170, at step 4992
#0 worker (src/pool.c:77)
      75  			printf("job %d done: %ld\n", job, result & 0xffff);
      76  			fflush(stdout);
>     77  			p->queue->completed++;
      78  		}
      79  	}
arguments:
  arg = 0x55c074530010
locals:
  q = <optimized out>
  job = 14
  result = -804966335788770334
  p = 0x55c074530010
called from #1 start_thread (pthread_create.c:454)
called from #2 __GI___clone3 (../sysdeps/unix/sysv/linux/x86_64/clone3.S:78)
```

Under the source come the frame's arguments and locals, as gdb's `info args`
and `info locals` print them: the worker had just reported job 14.

`--tid` picks another thread, on the CPU or not. At step 4992, where
the worker faults, the main thread is waiting to join the workers:

```console
$ rewind where e20758b8 4992 --tid 166
rewind: process 166 at step 4992; loading symbols for 4 of its files
rewind: read 3 source files fetched from the VM earlier
rewind: walking thread 166's stack in gdb
rewind: downloading source files of libc.so.6; each one only once
process 166 (test_pool_shutdown), thread 166, at step 4992
#7 pool_shutdown (src/pool.c:128)
     126
     127  	for (int i = 0; i < POOL_WORKERS; i++)
>    128  		pthread_join(p->workers[i], NULL);
     129  	pthread_cond_destroy(&p->ready);
     130  	pthread_mutex_destroy(&p->lock);
arguments:
  p = 0x55c074530010
locals:
  i = 0
called from #8 main (tests/test_pool_shutdown.c:24)
called from #9 __libc_start_call_main (../sysdeps/nptl/libc_start_call_main.h:59)
```

`--json` prints every frame, with its arguments and locals, where each is
and which one was chosen. For every frame of every thread, ask gdb for all
the stacks of the process:

```console
$ rewind gdb e20758b8 4992 --pid 166 -- -batch -ex 'thread apply all bt' 2>/dev/null | grep -E '^Thread|src/|tests/'
#0  0x000055c06fa1d437 in worker (arg=0x55c074530010) at src/pool.c:77
Thread 4 (Thread 1.170 (test_pool_shutd, on the CPU)):
#0  0x000055c06fa1d437 in worker (arg=0x55c074530010) at src/pool.c:77
Thread 3 (Thread 1.169 (test_pool_shutd)):
#5  0x000055c06fa1d3a9 in run_job (job=17) at src/pool.c:45
#6  worker (arg=0x55c074530010) at src/pool.c:73
Thread 2 (Thread 1.166 (test_pool_shutd)):
#7  0x000055c06fa1d5b3 in pool_shutdown (p=p@entry=0x55c074530010) at src/pool.c:128
#8  0x000055c06fa1d272 in main () at tests/test_pool_shutdown.c:24
Thread 1 (Thread 1.4194305 (the CPU, in test_pool_shutd 170)):
```

`rewind gdb` starts in the thread `rewind where` looks at, in its innermost
frame. `--tid` and `--frame` start it elsewhere, with frames numbered as
`where --json` numbers them. In the main thread's frame that `where` chose,
`pool_shutdown` has already set the queue to NULL:

```console
$ rewind gdb e20758b8 4992 --tid 166 --frame 7 -- -batch -ex 'p p->queue'
rewind: process 166 at step 4992; loading symbols for 4 of its files
rewind: read 3 source files fetched from the VM earlier
rewind: gdb at step 4992 of e20758b82c63ae48
arch_local_irq_restore (flags=6) at ./arch/x86/include/asm/irqflags.h:146
146		return !(flags & X86_EFLAGS_IF);
[Switching to thread 2 (Thread 1.166)]
#0  __syscall_cancel_arch () at ../sysdeps/unix/sysv/linux/x86_64/syscall_cancel.S:56
56		ret
#7  0x000055c06fa1d5b3 in pool_shutdown (p=p@entry=0x55c074530010) at src/pool.c:128
128			pthread_join(p->workers[i], NULL);
$1 = (struct queue *) 0x0
[Inferior 1 (process 1) detached]
```

`where` and gdb read each thread's registers from the VM kernel's task list,
so they work on runs recorded with a guest that lists its tasks, as these
were.

In the app, the s key opens the source panel at the line `rewind where` names,
with the frame's variables and the thread's frames below it; click a frame to
see its line and variables instead. The panel follows the playhead, and each
new step takes a few seconds since it forks the run.

![The app's source panel at step 4992 of the failing run: worker at src/pool.c:77 with p->queue->completed++ marked and the values that can be blamed underlined, the worker's arguments and locals, and the frames worker, start_thread and clone3 below](../site/img/app-source.png)

## Find who wrote a value

The worker crashed because `p->queue` was NULL. `rewind blame` finds the
write that put a value there. Give it a step and an expression. It works out
the expression's address in the thread that `where` picks. Then it replays the
stretch of the run before the step, with that address in the CPU's debug
registers. If the stretch holds no write, it tries the stretch before that:

```console
$ rewind blame e20758b8 --at 4992 'p->queue'
rewind: process 166 at step 4992; loading symbols for 4 of its files
rewind: read 3 source files fetched from the VM earlier
rewind: walking thread 170's stack in gdb
p->queue = 0x0   8 bytes at 0x55c074530068, thread 170, pool.c:77

last written at step 4987 by thread 166 (test_pool_shutd)
  pool.c:125  pool_shutdown  p->queue = NULL;
  was 0x55c074530090

other threads just after the write, in step 4987:
  thread 169  pool.c:45  run_job  __syscall_cancel_arch
  thread 170  pool.c:75  worker  __GI___lll_lock_wake_private

searched back to step 4454, 1 keyframe window, 1 write, 0.6s
```

The main thread set `p->queue` to NULL on line 125, in `pool_shutdown`. It did
so at step 4987, 5 steps before the crash. blame also
prints the value before the write.

The last part shows where the other threads were just after the write. Thread
170 is the worker that crashes later. It was on line 75, printing. It
had already passed the check of `p->stopping` on line 74.

A step runs from one exit of the VM to the next, so a step can cover many
lines of the program. At the start of step 4987, the main thread had
not yet called `pool_shutdown`. `rewind where --after-write` looks just after
the write itself, with the address and size blame printed:

```console
$ rewind where e20758b8 4987 --tid 166 --after-write 0x55c074530068 --size 8
rewind: process 166 at step 4987; loading symbols for 4 of its files
rewind: read 3 source files fetched from the VM earlier
rewind: walking thread 166's stack in gdb
process 166 (test_pool_shutdown), thread 166, at step 4987, just after its write of the 8 bytes at 0x55c074530068
#0 pool_shutdown (src/pool.c:125)
     123  	/* The bug: the queue goes before the workers are joined. */
     124  	free(p->queue);
>    125  	p->queue = NULL;
     126
     127  	for (int i = 0; i < POOL_WORKERS; i++)
arguments:
  p = 0x55c074530010
locals:
  i = 0
called from #1 main (tests/test_pool_shutdown.c:24)
called from #2 __libc_start_call_main (../sysdeps/nptl/libc_start_call_main.h:59)
```

`--writes 2` goes back one more write. Here that is the `calloc` in
`pool_create`.

blame can only watch values in memory. gdb keeps some values in registers, such
as `job` here. For those, blame says it cannot watch them and lists the values
on the same line that it can watch.

`--reads` lists the reads as well as the writes. It takes several expressions
at once. It searches back to the start of the process, or to the step given
with `--since`. Here it searches the last 40 steps. The output below keeps only
lines 74 and 77 of the worker and lines 119 and 125 of `pool_shutdown`:

```console
$ rewind blame e20758b8 --at 4992 'p->stopping' 'p->queue' --reads --since 4952
...
  step  thread           access  value               where
...
  4962  169 test_pool_shutd read    0                   pool.c:74  worker  if (!p->stopping) {  p->stopping
  4963  169 test_pool_shutd read    0x55c074530090      pool.c:77  worker  p->queue->completed++;  p->queue
...
  4976  170 test_pool_shutd read    0                   pool.c:74  worker  if (!p->stopping) {  p->stopping
  4978  169 test_pool_shutd read    0                   pool.c:74  worker  if (!p->stopping) {  p->stopping
  4981  169 test_pool_shutd read    0x55c074530090      pool.c:77  worker  p->queue->completed++;  p->queue
...
  4987  166 test_pool_shutd write   1                   pool.c:119  pool_shutdown  p->stopping = 1;  p->stopping
...
  4987  166 test_pool_shutd write   0x0                 pool.c:125  pool_shutdown  p->queue = NULL;  p->queue
  4989  170 test_pool_shutd read    0x0                 pool.c:77  worker  p->queue->completed++;  p->queue
...
searched back to step 4952, 2 keyframe windows, 2 writes, 13 reads, 0.6s
```

Read in order, the rows show the race:

1. Thread 170 reads `p->stopping` as 0 on line 74. So it goes on to
   count its job.
2. The main thread sets `p->stopping` to 1, then sets `p->queue` to NULL.
3. Thread 170 reads `p->queue` on line 77 and gets NULL.

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

![The app's source panel at step 4992 with the answer for p->queue: it became 0x0 at step 4987, written by the main thread in pool_shutdown at pool.c:125, p->queue = NULL, with the value before it, buttons to go to the write, show the reads and blame the earlier value, and where the other threads were](../site/img/app-blame.png)

## Find the step that decides it

`check` names step 4977 as the one that decides it. Its passing run is the
same schedule without that step, so the two runs are the same machine until
4977, where only the failing run gets a request to switch threads, and everything
they do differently follows from it. `rewind where` names the thread that was
on the CPU there:

```console
$ rewind where e20758b8 4977
rewind: process 166 at step 4977; loading symbols for 4 of its files
rewind: read 3 source files fetched from the VM earlier
rewind: walking thread 170's stack in gdb
rewind: downloading source files of libc.so.6; each one only once
process 166 (test_pool_shutdown), thread 170, at step 4977
#14 worker (src/pool.c:75)
      73  		long result = run_job(job);
      74  		if (!p->stopping) {
>     75  			printf("job %d done: %ld\n", job, result & 0xffff);
      76  			fflush(stdout);
      77  			p->queue->completed++;
arguments:
  arg = 0x55c074530010
locals:
  q = <optimized out>
  job = 14
  result = -804966335788770334
  p = 0x55c074530010
called from #15 start_thread (pthread_create.c:454)
called from #16 __GI___clone3 (../sysdeps/unix/sysv/linux/x86_64/clone3.S:78)
```

A worker checks `p->stopping` without the lock, prints its job, and then
counts it through `p->queue`, which `pool_shutdown` frees. At step
4977 the worker has made its check and is printing; a request to switch threads
there holds it before the count. The test's main thread runs meanwhile, sees
half the jobs done, and shuts the pool down, setting the queue to NULL, and
the worker faults on it 15 steps later. Without that step's
perturbation, the worker counts first and the test passes.

`check --where` looks up both threads itself: the one on the CPU at the
deciding step, and the one at the failing run's first differing event. Each
lookup forks the run and starts gdb, so it is opt-in:

```console
$ rewind check --where --epoch 1790985600 github:fzakaria/rewindvm#mylib 2>/dev/null | sed -n '/^where the threads were/,/first event that differs$/p'
where the threads were:
        4977   166/170   worker (src/pool.c:75), on the CPU at the deciding step
        4981   166/169   worker (src/pool.c:75), at the first event that differs
```

The first event that differs is the other worker, thread 169,
reporting job 16 on the same line, 75. With thread 170
held at the deciding step before it counts job 14, thread
169 counts its own job and picks job 17, the one thread
170 picks in the passing run. The main thread then shuts the pool
down, nulling the queue at step 4987 while thread 169 runs
job 17, as blame showed. In the passing run thread 170
counts job 14 and picks job 17 first, at step 4978. That is the cause showing up before the symptom: the held
thread's fault at line 77, which `rewind where` showed at the top of this
page, comes 11 steps later, at step 4992.

In the app, the deciding step is a dashed blue mark on the timeline, before
the solid one where the runs' events first differ. Zoomed in around the crash:

![The app zoomed in around the crash: a dashed blue mark at step 4977 on the timeline, a solid one at step 4981 where the events first differ, the crash after them, and the divergence card saying the two runs are identical until step 4977, where only this run gets a request to switch threads](../site/img/app-decides.png)

## See which thread held the CPU

The VM has one CPU, so a race is an order of threads. Every context switch
in the VM is a step of its own, and `rewind threads` prints which thread held
the CPU over a window of steps, a line per stretch. Around the deciding step,
in both runs:

```console
$ rewind threads e20758b8 --from 4968 --to 4992
      4968       4974          idle  swapper/0
      4975       4975       166/166  test_pool_shutd
      4976       4977       166/170  test_pool_shutd
      4978       4978       166/169  test_pool_shutd
      4979       4979       166/170  test_pool_shutd
      4980       4986       166/169  test_pool_shutd
      4987       4988       166/166  test_pool_shutd
      4989       4992       166/170  test_pool_shutd
```

```console
$ rewind threads 6a609b9c --from 4968 --to 4992
      4968       4974          idle  swapper/0
      4975       4975       166/166  test_pool_shutd
      4976       4982       166/170  test_pool_shutd
      4983       4983       166/166  test_pool_shutd
      4984       4986       166/169  test_pool_shutd
      4987       4987       166/166  test_pool_shutd
      4988       4988     kernel 11  ksoftirqd/0
      4989       4992          idle  swapper/0
```

The lines match up to step 4977. From there the threads take the CPU in
another order, and in the failing run thread 170 holds it at the crash.
Every switch is a step, so no stretch is too short to appear.

In the app, the t key opens the Threads tab: a lane per thread around where
the two runs part, this run's above the compared run's, with a bar wherever
the thread held the CPU. Ctrl and the wheel zoom the lanes and Shift and the
wheel pan them. Clicking a bar moves the playhead there. The band under the
timeline is darker where the switches are dense, which says where to look,
and the Stop at menu offers the switches of the thread on the CPU at the
playhead, so Next goes to where that thread took the CPU or lost it:

![The app's Threads tab around the crash: a lane per thread of test_pool_shutdown, the failing run's above the passing run's, with the worker that crashes taking the CPU in another order after the deciding step](../site/img/app-threads.png)

## Watch both sides of the race

A watchpoint finds who freed the queue the crash reads. Break in a worker so
`p` is in scope, watch `p->queue`, and continue:

```console
$ rewind gdb e20758b8 4942 -- -batch -ex 'break src/pool.c:74' -ex continue -ex 'watch -l p->queue' -ex 'delete 1' -ex continue -ex 'bt 2' -ex 'break src/pool.c:77 if p->queue == 0' -ex continue -ex 'bt 1'
rewind: step 4942 ran in process 166; loading symbols for 4 of its files
rewind: read 3 source files fetched from the VM earlier
rewind: gdb at step 4942 of e20758b82c63ae48
arch_local_irq_restore (flags=518) at ./arch/x86/include/asm/irqflags.h:146
146		return !(flags & X86_EFLAGS_IF);
[Switching to thread 4 (Thread 1.170)]
#0  __syscall_cancel_arch () at ../sysdeps/unix/sysv/linux/x86_64/syscall_cancel.S:56
56		ret
Breakpoint 1 at 0x55c06fa1d3a9: file src/pool.c, line 74.
[Switching to Thread 1.169]

Thread 3 hit Breakpoint 1, worker (arg=0x55c074530010) at src/pool.c:74
74			if (!p->stopping) {
Hardware watchpoint 2: -location p->queue
[Switching to Thread 1.166]

Thread 2 hit Hardware watchpoint 2: -location p->queue

Old value = (struct queue *) 0x55c074530090
New value = (struct queue *) 0x0
pool_shutdown (p=p@entry=0x55c074530010) at src/pool.c:128
128			pthread_join(p->workers[i], NULL);
#0  pool_shutdown (p=p@entry=0x55c074530010) at src/pool.c:128
#1  0x000055c06fa1d272 in main () at tests/test_pool_shutdown.c:24
Breakpoint 3 at 0x55c06fa1d433: file src/pool.c, line 77.
[Switching to Thread 1.170]

Thread 4 hit Breakpoint 3, worker (arg=0x55c074530010) at src/pool.c:77
77				p->queue->completed++;
#0  worker (arg=0x55c074530010) at src/pool.c:77
[Inferior 1 (process 1) detached]
```

The first stop is `main` nulling the queue in `pool_shutdown`, the second the
worker reading it. Watchpoints are the CPU's four debug registers, so the VM
runs at full speed until one fires. `watch` and `awatch` work; x86 has no
`rwatch`. At user addresses they stop only in the process gdb started in.

In the app, Attach gdb opens the same session under the timeline:

![The app's gdb pane stopped at the watchpoint in pool_shutdown, with the old and new values of p->queue](../site/img/app-gdb-watch.png)

## Run backwards to the write

gdb gives the same answer as blame and can go on from there: it can run the
fork backwards. From the crash itself, watch the pointer the worker read and
run back to the last write to it:

```console
$ rewind gdb e20758b8 4992 -- -batch -ex 'watch -l p->queue' -ex reverse-continue -ex 'bt 2'
rewind: step 4992 ran in process 166; loading symbols for 4 of its files
rewind: read 3 source files fetched from the VM earlier
rewind: gdb at step 4992 of e20758b82c63ae48
arch_local_irq_restore (flags=6) at ./arch/x86/include/asm/irqflags.h:146
146		return !(flags & X86_EFLAGS_IF);
[Switching to thread 4 (Thread 1.170)]
#0  0x000055c06fa1d437 in worker (arg=0x55c074530010) at src/pool.c:77
77				p->queue->completed++;
Hardware watchpoint 1: -location p->queue
[Switching to Thread 1.166]

Thread 2 hit Hardware watchpoint 1: -location p->queue

Old value = (struct queue *) 0x0
New value = (struct queue *) 0x55c074530090
pool_shutdown (p=p@entry=0x55c074530010) at src/pool.c:125
125		p->queue = NULL;
#0  pool_shutdown (p=p@entry=0x55c074530010) at src/pool.c:125
#1  0x000055c06fa1d272 in main () at tests/test_pool_shutdown.c:24
[Inferior 1 (process 1) detached]
```

The fork stops before `main` nulls the queue in `pool_shutdown`. Going back,
gdb shows the values swapped: the old value is the one after the write.
`reverse-stepi`, `reverse-next`, `reverse-finish` and `reverse-continue` to a
breakpoint work too. Rewind replays the run from the step's start to go back,
so a step back takes a moment, and longer under exit time, which single-steps
the way. A fork whose memory or registers gdb changed cannot go back.

## Follow the fault into the kernel

`rewind gdb` has the VM kernel's symbols too. Stop where the kernel sends the
SIGSEGV, and use its own gdb scripts:

```console
$ rewind gdb e20758b8 4977 -- -batch -ex 'break force_sig_fault' -ex continue -ex 'bt 4' -ex 'pipe lx-ps | tail -4' -ex 'pipe lx-dmesg | tail -2'
rewind: step 4977 ran in process 166; loading symbols for 4 of its files
rewind: read 3 source files fetched from the VM earlier
rewind: gdb at step 4977 of e20758b82c63ae48
arch_local_irq_restore (flags=518) at ./arch/x86/include/asm/irqflags.h:146
146		return !(flags & X86_EFLAGS_IF);
[Switching to thread 4 (Thread 1.170)]
#0  __syscall_cancel_arch () at ../sysdeps/unix/sysv/linux/x86_64/syscall_cancel.S:56
56		ret
Downloading 133.41 K source file /build/linux-7.2.8/kernel/signal.c...
Breakpoint 1 at 0xffffffff812c4b70: file kernel/signal.c, line 1757.
[Switching to Thread 1.4194305]

Thread 1 hit Breakpoint 1, force_sig_fault (sig=11, code=1, addr=0x108) at kernel/signal.c:1757
1757	{
#0  force_sig_fault (sig=11, code=1, addr=0x108) at kernel/signal.c:1757
#1  0xffffffff81eea4d0 in handle_page_fault (regs=0xffffc900001dbf58, error_code=6, address=264) at arch/x86/mm/fault.c:1483
#2  exc_page_fault (regs=0xffffc900001dbf58, error_code=6) at arch/x86/mm/fault.c:1536
#3  0xffffffff810012a6 in asm_exc_page_fault () at ./arch/x86/include/asm/idtentry.h:595
0xffff888003811f80  162  bash
0xffff888003bdde80  166  test_pool_shutd
0xffff888003810fc0  169  test_pool_shutd
0xffff888003bdcec0  170  test_pool_shutd
[    0.204005] test_pool_shutd[170]: segfault at 108 ip 000055c06fa1d437 sp 00007fda39950e10 error 6 in test_pool_shutdown[1437,55c06fa1d000+1000] likely on CPU 0 (core 0, socket 0)
[    0.204010] Code: fa 48 8d 35 2a 0c 00 00 bf 02 00 00 00 b8 00 00 00 00 e8 ac fc ff ff 48 8b 05 b5 2b 00 00 48 8b 38 e8 8d fc ff ff 49 8b 46 58 <83> 80 08 01 00 00 01 e9 b5 fe ff ff 55 48 89 e5 41 54 53 be 78 00
[Inferior 1 (process 1) detached]
```

The kernel's DWARF comes from `rewindvm.cachix.org` with Nix, and from the
debug tarball next to `rewind` without. The app's build log shows the kernel's
messages beside the program's with kernel console on:

![The app's build log with the kernel console on, showing the segfault report under the test's output](../site/img/app-kernel.png)

## Use your own gdb

`--listen` serves the fork for a gdb started elsewhere, such as an IDE's, and
prints the command line that loads the same symbols:

```console
$ rewind gdb e20758b8 4992 --listen 127.0.0.1:1234
rewind: step 4992 ran in process 166; loading symbols for 4 of its files
rewind: read 3 source files fetched from the VM earlier
rewind: gdb at step 4992 of e20758b82c63ae48; connect with: gdb -q -iex 'add-auto-load-safe-path /nix/store/h4wfwic161kxrr74jlzla5lsm28hgary-glibc-2.44-25' -iex 'set debuginfod enabled on' -iex 'set debuginfod urls http://127.0.0. ...
```

## Bring tools into the VM

`rewind shell --with` adds a Nix package to the shell, without changing the
run. Here binutils disassembles the faulting instruction:

```console
$ printf 'objdump -d --no-show-raw-insn --start-address=0x142c --stop-address=0x143e tests/test_pool_shutdown | tail -4; exit\n' | rewind shell e20758b8 4992 --pid 166 --with nixpkgs#binutils
rewind: packing 21 store paths for --with
rewind: a shell at step 4992 of e20758b82c63ae48; exit it to leave
[rewind] /build/mylib # objdump -d --no-show-raw-insn --start-address=0x142c --stop-address=0x143e tests/test_pool_shutdown | tail -4; exit
    142c:	mov    (%rax),%edi
    142e:	call   10c0 <fflush@plt>
    1433:	mov    0x58(%r14),%rax
    1437:	addl   $0x1,0x108(%rax)
```

In the app, Open shell starts a shell at the playhead in the terminal pane,
in the crashing process's working directory:

![The app's shell pane at step 4992 of the failing run, in /build/mylib, after pwd and ls tests](../site/img/app-shell.png)

## Give programs more CPUs

The VM has one vCPU, so by default programs see one CPU and Nix builds run
one job at a time. `--cores` tells them there are more, and the extra threads
interleave on the one vCPU:

```console
$ rewind nix --cores 4 --epoch 1790985600 github:fzakaria/rewindvm#mylib
...
rewind: run 2d8d15fd39c67d2e exited:0 after 6033 steps, 0.215s virtual, 0.442s wall (poweroff)
/nix/store/f6a9gy362szw6nxx3ikrklr8glr6rdln-mylib-0.3.0 a9d703ba89774f3d  matches rewindvm.cachix.org
```

```console
$ printf 'nproc; echo $NIX_BUILD_CORES; exit\n' | rewind shell 2d8d15fd 2688 --pid 110
rewind: a shell at step 2688 of 2d8d15fd39c67d2e; exit it to leave
[rewind] /build/mylib # nproc; echo $NIX_BUILD_CORES; exit
4
4
```

Go programs see the extra CPUs too, and Go's garbage collector can then
spin waiting for a thread that never runs. Run them with `GOGC=off` if they
fit in the VM's memory.

## See where the time went

`rewind time` reads where a run's time went from its trace. The first
number is how long the run took on the VM's one CPU. The second is its
critical path: the longest chain of work in which each part waited for the
one before, which is the least the run can take on any number of CPUs.

```console
$ rewind time 2d8d15fd
mylib-0.3.0
    212 ms  elapsed on the VM's one CPU
    205 ms  critical path: the least it can take on any number of CPUs
            98% computing, 2% asleep (clock_nanosleep 4.4 ms)
...
What would shorten it
    sooner      pid  if this took no time
     76 ms           every cc1, 3 processes
     70 ms           every bash, 80 processes
     52 ms       34  bash -e source-stdenv.sh default-builder.sh
     36 ms      112  cc1 /build/mylib/src/pool.c
     20 ms      139  cc1 /build/mylib/tests/test_pool_shutdown.c
     20 ms      120  cc1 /build/mylib/tests/test_pool_basic.c
     14 ms           every gcc, 3 processes
     12 ms           every ld, 2 processes
    7.6 ms      123  bash binutils-wrapper-2.46/bin/ld -plugin gcc-16.2.0/libexec/gcc/x86_64…
    7.6 ms      142  bash binutils-wrapper-2.46/bin/ld -plugin gcc-16.2.0/libexec/gcc/x86_64…
...
```

The build took 212 ms and its critical path is 205 ms: more CPUs could
save at most the difference, since mylib's Makefile is run without `-j`
and make compiles one file at a time. Each
row under "What would shorten it" is how much sooner the run would finish
if that part took no time, worked out by taking it out of the recorded
chain and finding the longest chain again. `--in PID` shows one process
the same way, with the `rewind where` command for the source line its time
starts at. `--folded` writes the path as folded stacks for flamegraph.pl
or speedscope, and `--perfetto F` writes every process on a timeline for
ui.perfetto.dev.

In the app, the Time tab, or w, shows the same with the path as a flame
graph, computing warm and asleep blue. Clicking a frame moves the playhead
to it; a sleep or a stretch of computing shows its source line, with the
source panel and gdb a click away.

![The app's Time tab on the four-CPU build of mylib: the elapsed time and the critical path, the path as a bar and as a flame graph of processes, and what would make it finish sooner](../site/img/app-time.png)

## Steer the perturbation

`check --schedules N` tries more or fewer schedules, `--all` all of them.
`rewind fork` asks which schedules fail from a given step:

```console
$ rewind fork e20758b8 4892 --schedule 1 --quiet
rewind: run 31a3965755540384 exited:2 after 5324 steps, 0.208s virtual, 0.105s wall (poweroff)
rewind: the fork first differs from its parent at step 4923
rewind: open it beside its parent in the desktop app: rewind open 31a3965755540384 4923 --compare e20758b82c63ae48

$ rewind fork e20758b8 4892 --schedule 2 --quiet
rewind: run 154b30d3a0384957 exited:0 after 7021 steps, 0.227s virtual, 0.112s wall (poweroff)
rewind: the fork first differs from its parent at step 4926
rewind: open it beside its parent in the desktop app: rewind open 154b30d3a0384957 4926 --compare e20758b82c63ae48

$ rewind fork e20758b8 4892 --schedule 3 --quiet
rewind: run 5a61400513c562da exited:0 after 7176 steps, 0.232s virtual, 0.110s wall (poweroff)
rewind: the fork first differs from its parent at step 4927
rewind: open it beside its parent in the desktop app: rewind open 5a61400513c562da 4927 --compare e20758b82c63ae48

$ rewind fork e20758b8 4892 --schedule 4 --quiet
rewind: run 32b30a2780fc9e23 exited:0 after 7050 steps, 0.230s virtual, 0.111s wall (poweroff)
rewind: the fork first differs from its parent at step 4901
rewind: open it beside its parent in the desktop app: rewind open 32b30a2780fc9e23 4901 --compare e20758b82c63ae48
```

The app's Runs tab, or the runs pill in its header, shows every run of the
build, with forks under the run and step they branched from:

![The app's Runs panel: check's schedules, and the forks of the failing run, failing and passing](../site/img/app-runs.png)

Fork from here forks the run at the playhead with the next schedule. Clicking
a run opens it compared with the run it hangs under; right-clicking offers
more.

## Find a run again

A command takes a run by its id or a prefix of it, by `@` for the newest run
and `@2`, `@3` and on for the ones before it, by name, or by directory.
`rewind ls` lists runs newest first, and filters them:

```console
$ rewind ls -n 3
32b30a2780fc9e23  exited:0          7050 steps  mylib-0.3.0 (fork of e20758b82c63ae48 at 4892, schedule 4)
5a61400513c562da  exited:0          7176 steps  mylib-0.3.0 (fork of e20758b82c63ae48 at 4892, schedule 3)
154b30d3a0384957  exited:0          7021 steps  mylib-0.3.0 (fork of e20758b82c63ae48 at 4892, schedule 2)

$ rewind ls --forks-of e20758b8 --status failed
31a3965755540384  exited:2          5324 steps  mylib-0.3.0 (fork of e20758b82c63ae48 at 4892, schedule 1)
```

`--name` and `--since` (`30m`, `2h`, `7d` or a date) filter too.

`rewind open` starts the app on a run, at a step and beside another run.
`check` and `fork` print the command that opens what they made:

```console
$ rewind open e20758b8 4992 --compare 6a609b9c
```

## Rebuild a run exactly

A run's id is the hash of its inputs, which its `manifest.json` lists, so the
same command follows the same run on a machine with the same CPU vendor and
guest. `rewind show` prints that command with every input spelled out,
including the epoch, which defaults to the start of the day the run was made.
For a fork it prints its parents' commands first. `@`, the newest run, is the
schedule 4 fork:

```console
$ rewind show @
32b30a2780fc9e23  exited:0          7050 steps  mylib-0.3.0 (fork of e20758b82c63ae48 at 4892, schedule 4)
...
rewind nix /nix/store/...-mylib-0.3.0.drv --epoch 1790985600 --schedule 7 --schedule-from 2818 --schedule-until 4978 --cpu v3 --clock branches --name mylib-0.3.0  # e20758b82c63ae48
rewind fork e20758b82c63ae48 4892 --schedule 4  # 32b30a2780fc9e23
```

The failing run's command makes it again, with the same id:

```console
$ rewind show e20758b8 | tail -1 | sh
...
rewind: run e20758b82c63ae48 exited:2 after 5037 steps, 0.204s virtual, 0.377s wall (poweroff)
```

## Compare any two runs

`rewind diff` compares every event of two runs, where `check` compares only
the failing program's:

```console
$ rewind diff 6a609b9c e20758b8
first difference at event 1622: step 4978 on the left, step 4981 on the right
  both        4963   166/169   write(1, "job 15 done: 42559\n")
  both        4964   166/169   write(1, "worker picked job 16\n")
  both        4977   166/170   write(1, "job 14 done: 39906\n")
  left        4978   166/170   write(1, "worker picked job 17\n")
  left        4986   166/169   thread exit(test_pool_shutd) exited:0
  left        4998   166/170   thread exit(test_pool_shutd) exited:0
  left        5001   166/166   write(1, "round 1: ok\n")
  right       4981   166/169   write(1, "job 16 done: 5986\n")
  right       4982   166/169   write(1, "worker picked job 17\n")
  right       4990     0/0     console "[    0.204005] test_pool_shutd[170]: segfault at 108 ip 000055c06fa1d437 sp 00007fda39950e10 error 6 in test_pool_shutdown[1437,55c06fa1d000+1000] likely on CPU 0 (core 0, socket 0)"
  right       4991     0/0     console "[    0.204010] Code: fa 48 8d 35 2a 0c 00 00 bf 02 00 00 00 b8 00 00 00 00 e8 ac fc ff ff 48 8b 05 b5 2b 00 00 48 8b 38 e8 8d fc ff ff 49 8b 46 58 <83> 80 08 01 00 00 01 e9 b5 fe ff ff 55 48 89 e5 41 54 53 be 78 00"
```

In the app, any run compared with another shows where they part, on the
timeline and in the divergence card:

![The app at the step where the failing run leaves the passing one, with the divergence card naming both threads' writes](../site/img/app-compare.png)

The card's Compare side by side link opens the Compare tab: the last events
both runs shared, then each run's next events side by side, with what differs
in amber. The x key swaps to the other run at the matching step, and x again
comes back:

![The app's Compare tab: the writes both runs made before they part, then the other worker's report of its job in the failing run beside the passing run's pick of the next one, and the failing run's SIGSEGV a few events below, each run's text in full with what differs in amber](../site/img/app-compare-tab.png)

## Script it

`run`, `nix` and `fork` exit with the job's status, as a shell would report
it, so a shell loop can ask which schedules fail from a step:

```console
$ for s in 5 6 7 8; do rewind fork e20758b8 4892 --schedule $s --quiet 2>/dev/null; echo "schedule $s: exit $?"; done
schedule 5: exit 0
schedule 6: exit 2
schedule 7: exit 2
schedule 8: exit 0
```

The other commands answer a question with their exit status:

| Command                   | 0                        | 1                                                                 | 3                             | 4                                                      |
| ------------------------- | ------------------------ | ----------------------------------------------------------------- | ----------------------------- | ------------------------------------------------------ |
| `check`                   | no run fails a property  | a run fails one, by default by ending differently from schedule 0 |                               |                                                        |
| `diff`, `replay`, `blame` | the runs are identical   | they differ                                                       |                               |                                                        |
| `doctor`                  | this machine can record  | it cannot                                                         |                               |                                                        |
| `cat`                     | the file was printed     |                                                                   | no such file at the step      | the path named a directory (`--archive tar` writes it) |
| `files`                   | the directory was listed |                                                                   | no such directory at the step | the path named something other than a directory        |

Every command exits 2 when it could not do what was asked, as for a run that
does not exist, so `rewind diff a b || report_flaky` reports only runs that
differ. With `--json`, commands
print JSON for a program to read in place of their text: one object, or from
`ls` and `events` one a line. `check --json` lists each run it tried under
`schedules`, with the properties the run violated under `violations`.

```console
$ rewind ls --forks-of e20758b8 --status failed --json | jq -r .id
1b7eb9b2f8c8f287
8d6497cd10c90990
31a3965755540384

$ rewind diff 6a609b9c e20758b8 --json | jq -c '.divergence | {left_step, right_step}'
{"left_step":4978,"right_step":4981}
```

## Give a run to an agent

`rewind summary` prints a run as plain text: the verdict and every reason
it failed, how it ended, the failing step with its process and thread, the
last 20 lines of output, and with `--compare`, the step where two runs part
and what each did there. It exits 0 for a passed run, 1 for a failed one
and 2 when it could not read the run. A coding agent or a CI job can read a
failure from it without the app, and every export carries the same text as
`summary.txt`.

```console
$ rewind summary e20758b8 --compare 6a609b9c
run e20758b82c63ae48: mylib-0.3.0
verdict: failed
  exited with status 2
ended: exited:2 at step 5037 (poweroff)
failing step: 4992, in process 166 (test_pool_shutdown), thread 170: SIGSEGV delivered, fault address 0x108
output (the last 20 of 108 lines):
...
compared with run 6a609b9c1faefbc8: mylib-0.3.0
  verdict: passed
  ended: exited:0 at step 6528 (poweroff)
the runs part at step 4981 here and step 4978 there, in the events of ./tests/test_pool_shutdown
  here:  process 166 (test_pool_shutdown), thread 169: write(1, "job 16 done: 5986\n")
  there: process 166 (test_pool_shutdown), thread 170: write(1, "worker picked job 17\n")
```

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

```console
$ cat adds-up.sh
#!/bin/sh
# The deposits add up: bank prints its balance and what it expected.
line=$(rewind log "$REWIND_RUN" | grep "^balance ")
test "$line" = "balance 400, expected 400" && exit 0
echo "$line"
exit 1
```

```console
$ rewind check --epoch 1790985600 --property script=./adds-up.sh github:fzakaria/rewindvm#bank | grep -E 'decides it|failing run violates adds-up'
rewind: packing 62 store paths for bank-0.1.0
schedule 1 violates adds-up, sdk; finding the step that decides it
step 3357 decides it: a request to switch threads there makes the run violate adds-up, sdk
failing run violates adds-up: balance 390, expected 400
```

`--properties` reads properties from a file, one a line, so a repository
can keep them beside its tests. A script's path in the file is relative to
the file's directory:

```console
$ cat rewind.properties
# What a correct bank run is.
no-crash
script=adds-up.sh

$ rewind check --epoch 1790985600 --properties rewind.properties github:fzakaria/rewindvm#bank | grep 'decides it'
schedule 1 violates adds-up, sdk; finding the step that decides it
step 3357 decides it: a request to switch threads there makes the run violate adds-up, sdk
```

## Count the schedules that fail from a step

`rewind fork` tries one schedule from a step. `check --run` tries many, each a
fork at `--schedule-from`, with the run itself as schedule 0. `--no-narrow`
stops at the count. From the deciding step of the passing run:

```console
$ rewind check --run 6a609b9c --schedule-from 4977 --schedules 16 --all --no-narrow
schedule   0: exited:0             6528 steps  a9d703ba8977  run 6a609b9c1faefbc8
schedule   1: exited:2             5045 steps    run 2da8aef95f58ed1c
schedule   2: exited:0             6999 steps  a9d703ba8977  run 23982c2308947737
schedule   3: exited:2             5051 steps    run 1fa8016c30b2dc4e
schedule   4: exited:0             7096 steps  a9d703ba8977  run 4c5f615a35d54108
schedule   5: exited:0             6987 steps  a9d703ba8977  run 1009d5915ca52552
schedule   6: exited:0             7087 steps  a9d703ba8977  run 128417e6e6b1eb46
schedule   7: exited:2             5043 steps    run b834f8bcccf4e4fb
schedule   8: exited:0             7088 steps  a9d703ba8977  run 2bba6fa85987de82
schedule   9: exited:0             6948 steps  a9d703ba8977  run 8a3491f03e9baa19
schedule  10: exited:2             5043 steps    run 3ba7d6de801b1bd4
schedule  11: exited:0             7040 steps  a9d703ba8977  run 95e2a8f08fd1986e
schedule  12: exited:0             7158 steps  a9d703ba8977  run a48530d4c8af504b
schedule  13: exited:0             7038 steps  a9d703ba8977  run e32dc22ebb19ffa0
schedule  14: exited:0             7079 steps  a9d703ba8977  run 98150b12f8b6c611
schedule  15: exited:2             5040 steps    run eea86ab4c81f6176
schedule  16: exited:2             5049 steps    run e1b60d6852674558
6 of 16 other schedules ended differently
```

6 of the 16 crash. Without `--no-narrow`, `check --run` narrows the first of
them to the step that decides it, as for a build.

In the app, the chevron beside Fork from here turns it into Check from here,
which runs the schedules from the playhead and fills a cell per schedule:
grey for ended as this run did, blue for differently. Pick a cell to compare
with or open its run:

![The app after Check from here at step 4977 of the passing run: a card counting how the 16 schedules ended, a cell per schedule, the first picked with its run and Compare and Open](../site/img/app-check.png)

## Hold a task off the CPU for longer

A stall holds the running task off the CPU for at most 1.28 ms. A race whose
window includes a sleep, a timer or slow I/O in another task needs a longer
hold. This program gives eight threads 20 ms to start, built static:

```console
$ printf '#include <pthread.h>\n#include <stdio.h>\n#include <time.h>\n#include <unistd.h>\nvolatile int ready[8];\nvoid *work(void *i) { write(1, "started\\n", 8); ready[(long)i] = 1; return 0; }\nint main(void) {\n  pthread_t t[8];\n  for (long i = 0; i < 8; i++) pthread_create(&t[i], 0, work, (void *)i);\n  nanosleep(&(struct timespec){0, 20000000}, 0); /* 20 ms to start */\n  for (int i = 0; i < 8; i++) if (!ready[i]) { printf("worker %%d not ready\\n", i); return 1; }\n  puts("every worker ready");\n}\n' > ready.c
$ mkdir -p ready/bin
$ nix shell nixpkgs#pkgsStatic.stdenv.cc -c x86_64-unknown-linux-musl-cc -static -o ready/bin/ready ready.c
```

No schedule stalls a thread for 20 ms by default:

```console
$ rewind check --root ready --schedules 64 --all --no-narrow -- /bin/ready
...
same result under all 65 schedules
```

`--stall-max` on `run`, `nix`, `check` and `fork` raises the longest stall,
up to 1 s:

```console
$ rewind check --root ready --stall-max 50ms -- /bin/ready | grep -E 'ends differently|limited to|decides it|passing:|failing:'
schedule 5 ends differently from schedule 0; finding the step that decides it
schedule 5 limited to steps 787..788 still ends differently
step 787 decides it: a 40.96 ms pause of the running thread there makes the run fail
passing: run 3479ddf3369a0786, schedule 0
failing: run a63379752ad0aaba, schedule 5 over steps 787..788, stalls up to 50 ms
```

The same steps stall, one step in 128, but each stall picks one of the
doublings of 10 µs up to the maximum, 14 of them for 50 ms, so fewer stalls
are short. How often a race turns up then depends on how long it needs a
thread held: 32 schedules of mylib crash 8 times by default and
2 times with `--stall-max 100ms`, in 3.0 s and 3.0 s. A stall is virtual time, so the guest's clock
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

```console
$ rewind nix -q --epoch 1790985600 --name hash-order github:fzakaria/rewindvm#hash-order
...
/nix/store/8fzgvvjf4iffwijx3njlhg40m0xxnlw4-hash-order-0.1.0 83f239c2e3135166  not in your store or 6 caches
```

```console
$ rewind check --run hash-order --seeds 8 --schedules 0 | grep -E 'decides it|passing:|failing:'
seed 2 decides it: under schedule 0, seed 0 passes and seed 2 fails
passing: run 6d8e37f286966443, seed 0, schedule 0
failing: run aae1d6a86be614d5, seed 2 from step 713, schedule 0
```

## List directories in another order

Inside the VM a directory lists its entries in the same order in every run,
so a build that depends on that order never differs there. On a real
machine, ext4 lists a directory in an order set when the disk was formatted.
The kernel argument `rewind.dirents=N` lists every directory the build sees
in the order of a hash of each name keyed by N, as another ext4 disk would;
0 keeps the usual order. The key is part of the run, so a run made with one
replays:

```console
$ rewind nix --kernel-args rewind.dirents=2 github:fzakaria/rewindvm#mylib
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

![The app's sheet of keys: moving the playhead, the timeline, looking around, and the app](../site/img/app-keys.png)

## Hand a failure to someone else

```console
$ rewind export e20758b8 --replayable -o crash.rwd
rewind: wrote crash.rwd (194.8 MB)

$ REWIND_HOME=elsewhere rewind import crash.rwd
e20758b82c63ae48  exited:2          5037 steps  mylib-0.3.0

$ REWIND_HOME=elsewhere rewind replay e20758b8
identical to the recording: 1646 events, 1399 context switches and 1476 read and write records over 5037 steps, and the whole machine at its 2 keyframes
```

A replayable export carries the keyframes, the image and the VM's kernel, and
replays on any machine with the same CPU vendor. Without `--replayable` it is
the events alone, 37.0 KB: enough to read and to scrub in the app, whose
Export button writes the replayable kind. Both kinds carry the run's
bookmarks, and `summary.txt`, the run in plain text as `rewind summary`
prints it: the verdict, the failing step and the last lines of output. With
`--with-answers`, an export also carries where each thread was and the
files the job had written at the failing step, at each broken assertion
and at each bookmark, which the browser viewer shows without a machine
to replay on.

The other machine also needs the features of the run's CPU model. A run
without `--cpu` gets `v3`, whose features include ERMS, the fast `rep movsb`
flag. On a host that lacks only ERMS it gets `v3-compat` instead, the same CPU
with ERMS hidden, and rewind prints a line saying so. A `v3` run needs a host
with ERMS, and a `v3-compat` run replays on hosts with or without it. A replay
on a host without the model's features names what is missing and where the
run replays, and `rewind doctor` says which model a run gets on a machine.

## Which clock

```console
$ rewind pmu status
cpu: AMD Zen (family 25)
amd workaround (MSR 0xc0011020 bit 54): set by `rewind pmu enable` this boot
perf_event_paranoid: 2
probe branches: 3000000 branches, as built
probe locked: 2000000 branches, as built
probe not-branches: 1000000 branches, as built
probe exits: 8192 branches, as built
self-test: 58072275 and 58072275 branches, exact at every step
runs will use counter time
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

```console
$ printf 'volatile unsigned long n;\nint main(void) { for (;;) n++; }\n' > spin.c
$ mkdir -p spin/bin
$ nix shell nixpkgs#pkgsStatic.stdenv.cc -c x86_64-unknown-linux-musl-cc -static -g -o spin/bin/spin spin.c
```

```console
$ rewind run --root spin --timeout 5 --name spin -- /bin/spin
rewind: run b9d96e55b3741144 timed-out after 723 steps, 0.004s virtual, 5.010s wall (timed out computing without exits for 4.9s, in user space in main+15 (spin.c:2), process 34 (spin))
```

A run that timed out while still making exits was slow rather than stuck, and
says so. `rewind check` gives each perturbed schedule ten times as long as
schedule 0 took, at least a minute, and reports a stuck schedule the same
way. Schedule 0 itself has no limit, since a long build is no reason to stop
it, so a test suite that hangs there holds up the whole check. Give such a
suite `rewind check --timeout`, which bounds every schedule, schedule 0
included. `--status timed-out` lists the runs that timed out:

```console
$ rewind ls --status timed-out
b9d96e55b3741144  timed-out          723 steps  spin
```

## Keep the run directory tidy

Runs live under `~/.local/share/rewind`, or `REWIND_HOME`:

```console
$ REWIND_HOME=elsewhere rewind ls
154b30d3a0384957  exited:0          7021 steps  mylib-0.3.0 (fork of e20758b82c63ae48 at 4892, schedule 2)
31a3965755540384  exited:2          5324 steps  mylib-0.3.0 (fork of e20758b82c63ae48 at 4892, schedule 1)
e20758b82c63ae48  exited:2          5037 steps  mylib-0.3.0

$ REWIND_HOME=elsewhere rewind remove 154b30d3
removed 154b30d3a0384957
rewind: `rewind gc` removes the pages no run uses any more
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
