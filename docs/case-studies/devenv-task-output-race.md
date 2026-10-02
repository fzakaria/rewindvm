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

The first run in the VM failed:

```console
$ rewind nix 'github:fzakaria/rewindvm?dir=examples/case-studies#devenv-last-lines-before-2296'
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
rewind: run d28e74115cdc3ac5 exited:101 after 1236 steps, 0.034s virtual, 0.727s wall (poweroff)

$ rewind replay d28e7411
identical: 769 events over 1236 steps
```

The task printed three lines and devenv reported two, as in the issue. The
issue was filed against devenv 1.10.0, and cecb0452 is 1.10.1. Most of 256
perturbed schedules lose the line as well:

```console
$ rewind check --all --schedules 256 'github:fzakaria/rewindvm?dir=examples/case-studies#devenv-last-lines-before-2296'
schedule   0: exited:101           1236 steps    run d28e74115cdc3ac5
schedule   1: exited:101           1304 steps    run 55cfd8487763ef48
schedule   2: exited:101           1329 steps    run 9815b2e4cb02bb68
schedule   3: exited:101           1355 steps    run b37e8aec71907f4f
...
schedule 256: exited:101           1371 steps    run f63237fbe3f0851d
schedule 0 failed; 42 of 256 perturbed schedules ended differently

schedule 12 passes where schedule 0 fails; narrowing the steps it perturbs
perturbing only steps 518..1270 still ends differently

passing: run 876919114d9f07f0
failing: run d28e74115cdc3ac5

where /bin/sh /build/scriptbbBbRR.sh first behaves differently:
  right       1163    43/43    execve("/build/scriptbbBbRR.sh", ["/bin/sh", "/build/scriptbbBbRR.sh"])
  right       1164    43/43    exit_group(scriptbbBbRR.sh) exited:1
```

215 of the 257 runs fail, all with `["line1", "line2"]`; 42 pass.

## Where the third line went

The events of the failing run around the task:

```console
$ rewind events d28e7411 --from 1150 --to 1170
      1156    40/41    open("/build/devenv_task_outputF96GeT.json", 0o2000302)
      1159    40/41    fork() = 43
      1163    43/43    execve("/build/scriptbbBbRR.sh", ["/bin/sh", "/build/scriptbbBbRR.sh"])
      1164    43/43    exit_group(scriptbbBbRR.sh) exited:1
      1167    40/41    SIGCHLD code=1 addr=0x0
```

Thread 41 of the test process (40) forks the task's shell (43). The shell runs
all three `echo`s and exits at step 1164, and the SIGCHLD that tells tokio
the child is done reaches thread 41 at step 1167. Everything the loop will
see is there before it reads anything.

`rewind gdb` opens gdb on a fork of the run at a step, with the symbols of
the process running there, loaded from the VM. A breakpoint on `read` for
the stdout pipe (descriptor 13) and one on the `child.wait()` branch show
what the loop did, from step 1156 on:

```console
$ rewind gdb d28e7411 1156 -- -batch -ex 'directory /nix/store/195dbbvj5h0yq9f4vnp833dsbgxpp8w0-devenv-tasks-tests-1.10.1/src' -ex 'break read if $rdi == 13' -ex 'break task_state.rs:409' -ex continue -ex 'set $buf = $rsi' -ex finish -ex 'x/s $buf' -ex 'delete 1' -ex continue
rewind: step 1156 ran in process 40; loading symbols for 5 of its files
rewind: gdb at step 1156 of d28e74115cdc3ac5
0xffffffff81285085 in __outl (value=<optimized out>, port=1504) at ./arch/x86/include/asm/shared/io.h:24
24	BUILDIO(l,  , u32)
Breakpoint 1 at 0x7f9960fbf190: file ../sysdeps/unix/sysv/linux/read.c, line 25.
Breakpoint 2 at 0x562cb10658a7: task_state.rs:409. (2 locations)

Breakpoint 1, __GI___libc_read (fd=13, buf=0x7f995c015f20, nbytes=8192) at ../sysdeps/unix/sysv/linux/read.c:25
25	{
0x0000562cb133c9bd in std::fs::{impl#9}::read (buf=..., self=<optimized out>) at /rustc/48a229ceaefd4985c50990b14116b6d856af0985/library/std/src/fs.rs:1335
warning: 1335	/rustc/48a229ceaefd4985c50990b14116b6d856af0985/library/std/src/fs.rs: No such file or directory
Value returned is $1 = 18
0x7f995c015f20:	"line1\nline2\nline3\n"

Breakpoint 2.1, devenv_tasks::task_state::{impl#1}::run::{async_fn#0}::{async_block#0}::{async_block#0} () at devenv-tasks/src/task_state.rs:415
415	                            let expanded_paths = expand_glob_patterns(&self.task.exec_if_modified);
[Inferior 1 (process 1) detached]
```

The `directory` is the test package's copy of the crate's sources on the
host, for gdb's listing. There is one read of the pipe, and it returns all 18
bytes: `line1\nline2\nline3\n`, into the `BufReader` that `next_line` takes
lines from. The loop then took two lines from that buffer, which the log
shows it printing, and the next time round ran the `child.wait()` branch
with `line3` still in the buffer. The function returned, and the buffer was
dropped with it.

The same breakpoints on the passing run 87691911, from the step where it
opens the task's output file, hit `read` on standard output twice and on
standard error (descriptor 15) once before the `child.wait()` branch: there,
the loop reached the end of both pipes before it took the exit.

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
starts. Perturbing the schedule changes who runs when; most of the 257
schedules leave the shell to finish first.

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
$ rewind check --all --schedules 256 'github:fzakaria/rewindvm?dir=examples/case-studies#devenv-last-lines-2296'
schedule   0: exited:0             1262 steps  9ab388bedc43  run c5a142253612d475
schedule   1: exited:0             1323 steps  9ab388bedc43  run 2dfbb8c377d4eb1c
schedule   2: exited:0             1379 steps  9ab388bedc43  run 45e49358fd2ad37b
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
[case-studies release](https://github.com/fzakaria/rewindvm/releases/tag/case-studies):
[devenv-task-output-race-replayable.rwd](https://github.com/fzakaria/rewindvm/releases/download/case-studies/devenv-task-output-race-replayable.rwd),
with everything needed to replay the failure, and
[devenv-task-output-race.rwd](https://github.com/fzakaria/rewindvm/releases/download/case-studies/devenv-task-output-race.rwd),
the trace alone, which the desktop app opens:

```console
$ rewind import https://github.com/fzakaria/rewindvm/releases/download/case-studies/devenv-task-output-race-replayable.rwd
$ rewind replay d28e7411
$ rewind gdb d28e7411 1156
$ rewind-app https://github.com/fzakaria/rewindvm/releases/download/case-studies/devenv-task-output-race.rwd
```

How they were made:

```console
$ rewind replay d28e7411 --from 1150
identical from the keyframe at step 512 to the end (0.31s)
$ rewind export d28e7411 --replayable -o devenv-task-output-race-replayable.rwd
rewind: wrote devenv-task-output-race-replayable.rwd (179.8 MB)
$ rewind export d28e7411 -o devenv-task-output-race.rwd
rewind: wrote devenv-task-output-race.rwd (9.9 KB)
```
