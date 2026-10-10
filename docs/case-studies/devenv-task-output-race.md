# Case study: lost task output in devenv

In November 2025 [devenv](https://devenv.sh/) could drop the last lines a
task printed: a task with `showOutput = true` showed its output two lines
short ([cachix/devenv#2281](https://github.com/cachix/devenv/issues/2281)).

## The software

devenv builds developer environments with Nix and runs
tasks in them: commands with dependencies between them, such as a database
migration before a test suite. The `devenv-tasks` crate runs each task's
command as a child process on tokio and collects what it prints. At
[cecb0452](https://github.com/cachix/devenv/blob/cecb0452cacd9c524ccfc973d5caffff834cbf02/devenv-tasks/src/task_state.rs#L365-L431),
`TaskState` reads the child's standard output and standard error a line at a
time and waits for it to exit, all in one `tokio::select!` loop:

```rust {18}
        loop {
            tokio::select! {
                result = stdout_reader.next_line(), if !stdout_closed => {
                    match result {
                        Ok(Some(line)) => {
                            ...
                            stdout_lines.push((std::time::Instant::now(), line));
                        },
                        Ok(None) => {
                            stdout_closed = true;
                        },
                        ...
                    }
                }
                result = stderr_reader.next_line(), if !stderr_closed => {
                    ...
                }
                result = child.wait() => {
                    match result {
                        Ok(status) => {
                            ...
                            if status.success() {
                                return Ok(TaskCompleted::Success(now.elapsed(), Self::get_outputs(&outputs_file).await));
                            } else {
                                return Ok(TaskCompleted::Failed(
                                    now.elapsed(),
                                    TaskFailure {
                                        stdout: stdout_lines,
                                        stderr: stderr_lines,
                                        error: format!("Task exited with status: {status}"),
                                    },
                                ));
                            }
                        },
```

When the `child.wait()` branch runs, the function returns with whatever lines
it has collected so far.

## The test

The derivation builds `devenv-tasks`' unit tests at cecb0452, in release mode
with debug info, and adds one test to them:

```rust
#[tokio::test]
async fn test_failed_task_keeps_last_lines() -> Result<(), Error> {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("tasks.db");
    let script = create_script("#!/bin/sh\necho line1\necho line2\necho line3\nexit 1\n")?;
    let tasks = Tasks::builder(
        Config::try_from(json!({
            "roots": ["myapp:task_1"],
            "run_mode": "all",
            "tasks": [{ "name": "myapp:task_1", "command": script.to_str().unwrap() }]
        }))
        .unwrap(),
        VerbosityLevel::Verbose,
        Shutdown::new(),
    )
    .with_db_path(db_path)
    .build()
    .await?;
    tasks.run().await;
    match inspect_tasks(&tasks).await.as_slice() {
        [(_, TaskStatus::Completed(TaskCompleted::Failed(_, failure)))] => {
            let lines: Vec<&str> = failure.stdout.iter().map(|(_, l)| l.as_str()).collect();
            assert_eq!(lines, vec!["line1", "line2", "line3"]);
        }
        other => panic!("unexpected task statuses: {other:?}"),
    }
    Ok(())
}
```

`create_script`, `inspect_tasks` and the builder are the test module's own
helpers, used the same way by the tests around it. The derivation runs the
test binary with this one test selected.

## How Rewind found it

`tokio::select!` takes its branch order from the VM's randomness, which
`--seed` sets. With this kernel seed 0 draws the order that drains the pipe,
so the page uses seed 2, where the build fails. The first run in the VM
failed:

```console
$ rewind nix --seed 2 'github:fzakaria/rewindvm?dir=examples/case-studies#devenv-last-lines-before-2296'
rewind: packing 57 store paths for devenv-tasks-test-last-lines-before-2296
iteration 1

running 1 test
test tests::test_failed_task_keeps_last_lines ... FAILED

failures:

---- tests::test_failed_task_keeps_last_lines stdout ----
[myapp:task_1] line1
[myapp:task_1] line2

thread 'tests::test_failed_task_keeps_last_lines' (41) panicked at devenv-tasks/src/tests/mod.rs:3006:13:
assertion `left == right` failed
  left: ["line1", "line2"]
 right: ["line1", "line2", "line3"]
...
rewind: run 81b5bb745c2ef3e6 exited:101 after 1234 steps, 0.034s virtual, 0.346s wall (poweroff)

$ rewind replay 81b5bb74
identical: 767 events over 1234 steps
```

The task printed three lines and devenv reported two, short at the end as
in the issue. The issue was filed against devenv 1.10.0, and cecb0452 is
1.10.1. Most of 256 perturbed schedules lose the line as well:

```console
$ rewind check --seed 2 --all --schedules 256 'github:fzakaria/rewindvm?dir=examples/case-studies#devenv-last-lines-before-2296'
schedule   0: exited:101       1234 steps    run 81b5bb745c2ef3e6
schedule   1: exited:101       1258 steps    run a3729e93e94d6aa0
schedule   2: exited:101       1328 steps    run 60b944913ee157a1
schedule   3: exited:101       1337 steps    run dca729acfe308cb9
...
schedule 256: exited:101       1326 steps    run 46ee295f5306979b
schedule 0 failed; 50 of 256 perturbed schedules ended differently

schedule 12 passes where schedule 0 fails; narrowing the steps it perturbs
perturbing only steps 518..1270 still ends differently
step 1269 decides it: a reschedule there makes the run pass

passing: run ce9c756366a37c55, schedule 12 over steps 518..1270
failing: run f800da6bd5088cca, schedule 12 over steps 518..1269
the two are the same run until step 1269

where /nix/store/195dbbvj5h0yq9f4vnp833dsbgxpp8w0-devenv-tasks-tests-1.10.1/bin/devenv-tasks-tests test_failed_task_keeps_last_lines first behaves differently:
  both           1166    40/42    open("/build/.tmpJp3M0f/tasks.db-shm", 0o2400102)
  both           1257    40/41    open("/build/devenv_task_output26U1TT.json", 0o2000302)
  both           1264    40/41    fork() = 43
  failing        1273    40/41    SIGCHLD code=1 addr=0x0
  failing        1284    40/41    unlink("/build/devenv_task_output26U1TT.json")
  failing        1291    40/41    unlink("/build/scriptT2boZm.sh")
  failing        1292    40/41    unlink("/build/.tmpJp3M0f/tasks.db-shm")
  passing        1297    40/41    unlink("/build/devenv_task_output26U1TT.json")
  passing        1304    40/41    unlink("/build/scriptT2boZm.sh")
  passing        1305    40/41    unlink("/build/.tmpJp3M0f/tasks.db-shm")
  passing        1306    40/41    unlink("/build/.tmpJp3M0f/tasks.db-wal")

open both in the desktop app: rewind open f800da6bd5088cca 1273 --compare ce9c756366a37c55
```

207 of the 257 runs fail, all with `["line1", "line2"]`; 50 pass.

`check` narrows schedule 12, which passes, to steps 518 to 1270, and names
the last of them: without the reschedule at step 1269, the same window fails
like schedule 0. Its two runs are the same until that step. In both, the
task's shell forks at step 1264 and exits at 1266, and only in the failing one
does a SIGCHLD reach the test's thread 41, at step 1273, before the task is
done.

`rewind threads` replays a window of steps one at a time and prints which
thread held the VM's CPU through each stretch of it. Over the same steps of
the failing run and the passing one:

```console
$ rewind threads f800da6bd5088cca --from 1260 --to 1300
      1260       1261         40/41  tests::test_fai
      1262       1262     kernel 11  ksoftirqd/0
      1263       1264         40/41  tests::test_fai
      1265       1270         43/43  scriptT2boZm.sh
      1271       1271     kernel 11  ksoftirqd/0
      1272       1297         40/41  tests::test_fai
      1298       1299     kernel 11  ksoftirqd/0
      1300       1300         40/40  devenv-tasks-te

$ rewind threads ce9c756366a37c55 --from 1260 --to 1300
      1260       1261         40/41  tests::test_fai
      1262       1262     kernel 11  ksoftirqd/0
      1263       1264         40/41  tests::test_fai
      1265       1270         43/43  scriptT2boZm.sh
      1271       1271     kernel 11  ksoftirqd/0
      1272       1287         40/41  tests::test_fai
      1288       1289     kernel 11  ksoftirqd/0
      1290       1292         43/43  scriptT2boZm.sh
      1293       1293     kernel 11  ksoftirqd/0
      1294       1300         40/41  tests::test_fai
```

Both give the CPU to the shell, 43, from step 1265 to 1270, and to thread 41
at 1272. In the failing run the shell had finished exiting by then, and its
SIGCHLD reaches thread 41 at step 1273. In the passing run, after the
reschedule at step 1269, the shell gave up the CPU before its exit was done:
thread 41 runs from 1272 to 1287 with no signal, and the shell gets the CPU
back at 1290 to finish. The app's Threads tab draws the same thing, a lane per
thread, the failing run above the passing one:

![The Threads tab on the failing and passing runs: the task's shell, 43, on the CPU from step 1265 in both, and again at 1290 only in the passing run](../../site/img/devenv-threads.png)

How much of the failure rate is decided after the task starts? `check
--run` tries schedules as forks of a run already recorded, from a step of
it. From step 1158, where schedule 0's run forks the task's shell:

```console
$ rewind check --run 81b5bb74 --schedule-from 1158 --schedules 64 --all --no-narrow
schedule   0: exited:101       1234 steps    run 81b5bb745c2ef3e6
schedule   1: exited:101       1250 steps    run 285a73bbf713624c
schedule   2: exited:101       1255 steps    run c4b1b4f920210e73
schedule   3: exited:101       1244 steps    run 6a936f2e9f842af7
...
schedule  64: exited:0       1286 steps  77ac62e2629d  run c8fbd031eb1c0604
run 81b5bb745c2ef3e6 failed; 15 of 64 perturbed schedules ended differently
```

15 of 64 pass, about the share `check` found from the start of the test, 50
of 256: perturbing the steps before the task starts adds little.

## Where the third line went

The events of schedule 0's run, which fails, around the task:

```console
$ rewind events 81b5bb74 --from 1150 --to 1170
      1155    40/41    open("/build/devenv_task_outputnbzDbE.json", 0o2000302)
      1158    40/41    fork() = 43
      1162    43/43    execve("/build/script08XGOT.sh", ["/bin/sh", "/build/script08XGOT.sh"])
      1163    43/43    exit_group(script08XGOT.sh) exited:1
      1166    40/41    SIGCHLD code=1 addr=0x0
```

Thread 41 of the test process (40) forks the task's shell (43). The shell runs
all three `echo`s and exits at step 1163, and the SIGCHLD that tells tokio
the child is done reaches thread 41 at step 1166. Everything the loop will
see is there before it reads anything.

`rewind gdb` opens gdb on a fork of the run at a step, with the symbols of
the process running there, loaded from the VM. A breakpoint on `read` for
the stdout pipe (descriptor 13) and one on the `child.wait()` branch show
what the loop did, from step 1155 on:

```console
$ rewind gdb 81b5bb74 1155 -- -batch -ex 'break read if $rdi == 13' -ex 'break task_state.rs:409' -ex continue -ex 'set $buf = $rsi' -ex finish -ex 'x/s $buf' -ex 'delete 1' -ex continue
rewind: step 1155 ran in process 40; loading symbols for 5 of its files
rewind: source files for /nix/store/195dbbvj5h0yq9f4vnp833dsbgxpp8w0-devenv-tasks-tests-1.10.1 from /nix/store/hs2328bm8f7s0cq77jjjgdlcqrc13mhm-source
rewind: gdb at step 1155 of 81b5bb745c2ef3e6
arch_local_irq_restore (flags=514) at ./arch/x86/include/asm/irqflags.h:146
146		return !(flags & X86_EFLAGS_IF);
[Switching to thread 3 (Thread 1.41)]
#0  __syscall_cancel_arch () at ../sysdeps/unix/sysv/linux/x86_64/syscall_cancel.S:56
56		ret
Breakpoint 1 at 0x7f95b0d31190: file ../sysdeps/unix/sysv/linux/read.c, line 25.
Breakpoint 2 at 0x5568a5fdf8a7: task_state.rs:409. (2 locations)

Thread 3 hit Breakpoint 1, __GI___libc_read (fd=13, buf=0x7f95ac015f20, nbytes=8192) at ../sysdeps/unix/sysv/linux/read.c:25
25	{
0x00005568a62b69bd in std::fs::{impl#9}::read (buf=Python Exception <class 'gdb.error'>: value has been optimized out
&mut [u8](size=8192), self=<optimized out>) at /rustc/48a229ceaefd4985c50990b14116b6d856af0985/library/std/src/fs.rs:1335
warning: 1335	/rustc/48a229ceaefd4985c50990b14116b6d856af0985/library/std/src/fs.rs: No such file or directory
Value returned is $1 = 18
0x7f95ac015f20:	"line1\nline2\nline3\n"

Thread 3 hit Breakpoint 2.1, devenv_tasks::task_state::{impl#1}::run::{async_fn#0}::{async_block#0}::{async_block#0} () at devenv-tasks/src/task_state.rs:415
415	                            let expanded_paths = expand_glob_patterns(&self.task.exec_if_modified);
[Inferior 1 (process 1) detached]
```

There is one read of the pipe, and it returns all 18 bytes:
`line1\nline2\nline3\n`, into the `BufReader` that `next_line` takes lines
from. The loop then took two lines from that buffer, which the log shows it
printing, and the next time round ran the `child.wait()` branch with `line3`
still in the buffer. The function returned, and the buffer was dropped with
it.

The same breakpoints on the passing run ce9c7563, from the step where it
opens the task's output file, hit `read` on standard output once before the
`child.wait()` branch. That read also returns all 18 bytes, but the loop took
all three lines before it took the exit.

## Root cause

`tokio::select!` polls its branches in a random order each time it is
evaluated, unless it is told `biased;`; tokio does that so a loop with one
branch that is always ready does not starve the others. Once the child has
exited and tokio has reaped it, `child.wait()` is ready on every pass. Lines
already in the `BufReader` are ready too. So each pass round the loop is a
draw between "take the next line" and "return now", and every line still
buffered when the draw goes to `child.wait()` is lost. The more output a
task prints just before exiting, the more draws, and the more lines it can
lose.

The window is open only when the exit is visible before the output is read.
On the host the test process is usually reading the pipe while the shell is
still writing to it, so it reaches the end of the pipe first. In the VM, with
one CPU, the shell often runs from `execve` to `exit` without giving up the
CPU, and both the output and the exit are waiting together when the loop
starts. Perturbing the schedule changes who runs when. In the passing run
the shell also finishes before the loop reads, but no SIGCHLD for the test
process appears among its events, and the loop takes all three lines before
it takes the exit. In 207 of the 257 schedules the loop takes the exit with a
line still buffered.

## The fix, checked

[ef7fb697](https://github.com/cachix/devenv/commit/ef7fb6972ac033b7aa191345b93f77251ffadfb2),
merged as
[de0dc6a8](https://github.com/cachix/devenv/commit/de0dc6a85ae88eb8194c2f7e053f3e933b77c2ac),
stops returning from the `child.wait()` branch. It stores the exit status,
turns that branch off, and keeps reading until both pipes are closed:

```diff
         loop {
+            // If child has exited and both pipes are closed, we're done
+            if exit_status.is_some() && stdout_closed && stderr_closed {
+                break;
+            }
+
             tokio::select! {
...
-                result = child.wait() => {
+                result = child.wait(), if exit_status.is_none() => {
...
-                            if status.success() {
-                                return Ok(TaskCompleted::Success(now.elapsed(), Self::get_outputs(&outputs_file).await));
-                            } else {
-                                ...
-                            }
+                            // Store exit status and continue draining pipes
+                            exit_status = Some(status);
```

The same test at the merge commit passes every schedule:

```console
$ rewind check --seed 2 --all --schedules 256 'github:fzakaria/rewindvm?dir=examples/case-studies#devenv-last-lines-2296'
rewind: packing 57 store paths for devenv-tasks-test-last-lines-2296
schedule   0: exited:0       1259 steps  77ac62e2629d  run cf2425f775f0be3d
schedule   1: exited:0       1297 steps  77ac62e2629d  run 526f8e3e7b7947a2
schedule   2: exited:0       1355 steps  77ac62e2629d  run 467780b75b06c24a
...
0 of 256 perturbed schedules ended differently
same result under all 257 schedules
```

All 44 of `devenv-tasks`' tests at the merge commit, the added one included,
also passed under 65 schedules
(`github:fzakaria/rewindvm?dir=examples/case-studies#devenv-tasks-2296`).

## On the host

The same test binary on the host:

| What ran on the host                       | Failed    |
| ------------------------------------------ | --------- |
| the test, 16 CPUs                          | 0 of 1000 |
| the test, pinned to one CPU with `taskset` | 0 of 1000 |

The issue's reporter saw it in real use, on an aarch64 Linux machine.

## The recording

Both files are in the
[case-studies release](https://github.com/fzakaria/rewindvm/releases/tag/case-studies). `rewind import` and the
desktop app (`rewind-app`) take either one, by path or URL, and unpack it as
it downloads:

- [devenv-task-output-race.rwd](https://github.com/fzakaria/rewindvm/releases/download/case-studies/devenv-task-output-race.rwd)
  (10.0 KB) is the trace alone, enough for `rewind events`, `rewind log` and
  the app.
- [devenv-task-output-race-replayable.rwd](https://github.com/fzakaria/rewindvm/releases/download/case-studies/devenv-task-output-race-replayable.rwd)
  (185.1 MB) adds the kernel, the input image and the keyframes, so
  `rewind replay`, `rewind gdb` and `rewind shell` work on it.

```console
$ rewind import https://github.com/fzakaria/rewindvm/releases/download/case-studies/devenv-task-output-race-replayable.rwd
$ rewind replay 81b5bb74
$ rewind gdb 81b5bb74 1155
$ rewind-app https://github.com/fzakaria/rewindvm/releases/download/case-studies/devenv-task-output-race.rwd
```

How they were made:

```console
$ rewind replay 81b5bb74 --from 1150
identical from the keyframe at step 512 to the end (0.32s)
$ rewind export 81b5bb74 --replayable -o devenv-task-output-race-replayable.rwd
rewind: wrote devenv-task-output-race-replayable.rwd (185.1 MB)
$ rewind export 81b5bb74 -o devenv-task-output-race.rwd
rewind: wrote devenv-task-output-race.rwd (10.0 KB)
```
