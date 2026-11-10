# Case study: a SIGPIPE in Nix's gc-closure test

Nix's functional test `gc-closure.sh` can fail with exit status 141. Rewind VM
failed it on the first run; the host passed it 200 times. We reported it as
[NixOS/nix#16546](https://github.com/NixOS/nix/issues/16546), and the fix
below was merged as [NixOS/nix#16547](https://github.com/NixOS/nix/pull/16547).

Scrub the recording in your browser: [open it in the viewer](https://rewindvm.dev/viewer/?url=examples/nix-gc-closure.rwd).

## The test

Lines 14 to 16 of
[gc-closure.sh](https://github.com/NixOS/nix/blob/203f85b2e851fc52e253e8e33eff5fb92936736a/tests/functional/gc-closure.sh#L14-L16)
split a derivation's two outputs:

```bash
    input2=$(nix build -f dependencies2.nix input2_drv --no-link --print-out-paths)
    input2_out=$(printf "%s" "$input2" | head -n1)
    input2_out2=$(printf "%s" "$input2" | tail -n1)
```

Meson runs each functional test as `bash -x -e -u -o pipefail`, so if `printf`
dies the pipeline fails and the test ends.

## Run it

The derivation is the functional tests of
[Nix master 203f85b2](https://github.com/NixOS/nix/commit/203f85b2e851fc52e253e8e33eff5fb92936736a),
narrowed to this one test
([flake](../../examples/case-studies/flake.nix)). `--epoch` fixes the VM's
clock, which seeds its randomness, so these are the runs you get. The
unperturbed run passes. `rewind check` runs the test under other thread
schedules and stops at the first that ends differently:

```console
$ rewind check --epoch 1792368000 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-git-gc-closure'
rewind: packing 217 store paths for nix-functional-gc-closure-2.36pre20260912_203f85b2
...
schedule  23: exited:1            83618 steps    run 2daf27a69c19dcf7
...

schedule 23 ends differently from schedule 0; finding the step that decides it
schedule 23 limited to steps 709..83369 still ends differently
step 83368 decides it: a request to switch threads there makes the run fail

passing: run 802ec1c550b13d47, schedule 23 over steps 709..83368
failing: run 2b0c95958340658f, schedule 23 over steps 709..83369
the two are the same run until step 83368

where /nix/store/10dxp0qxqxxsyiljrh2kp0xqhz6arhcx-bash-5.3p15/bin/bash -x -e -u -o pipefail gc-closure.sh first behaves differently:
  both          83344   359/359   fork() = 534
  both          83361   534/534   fork() = 535
  both          83366   534/534   fork() = 536
  failing       83404   535/535   SIGPIPE code=0 addr=0x0
  failing       83405   535/535   exit_group(bash) killed:SIGPIPE
  failing       83408   534/534   SIGCHLD code=1 addr=0x0
  failing       83409   534/534   exit_group(bash) exited:141
  passing       83369   535/535   exit_group(bash) exited:0
  passing       83398   534/534   SIGCHLD code=1 addr=0x0
  passing       83399   534/534   exit_group(bash) exited:0
  passing       83402   359/359   SIGCHLD code=1 addr=0x0

open both in the desktop app: rewind open 2b0c95958340658f 83404 --compare 802ec1c550b13d47
```

Schedule 23 fails, and `check` narrows its perturbations to the one
step that decides it, 83368. The failing run's log has the test's last
lines:

```console
$ rewind log 2b0c9595
...
++(gc-closure.sh:14) nix build -f dependencies2.nix input2_drv --no-link --print-out-paths
...
+(gc-closure.sh:14) input2=$'/build/nix-test/main/gc-closure/store/ij8qwx0m6dvpmggqmq8zmhbaiw6ym8pg-dependencies-input-2\n/build/nix-test/main/gc-closure/store/hw6dm6rpyn ...
++(gc-closure.sh:15) printf %s $'/build/nix-test/main/gc-closure/store/ij8qwx0m6dvpmggqmq8zmhbaiw6ym8pg-dependencies-input-2\n/build/nix-test/main/gc-closure/store/hw6dm6 ...
++(gc-closure.sh:15) head -n1
+(gc-closure.sh:15) input2_out=/build/nix-test/main/gc-closure/store/ij8qwx0m6dvpmggqmq8zmhbaiw6ym8pg-dependencies-input-2
...
++(gc-closure.sh:14) nix build -f dependencies2.nix input2_drv --no-link --print-out-paths
...
+(gc-closure.sh:14) input2=$'/build/nix-test/main/gc-closure/store/ij8qwx0m6dvpmggqmq8zmhbaiw6ym8pg-dependencies-input-2\n/build/nix-test/main/gc-closure/store/hw6dm6rpyn ...
++(gc-closure.sh:15) head -n1
++(gc-closure.sh:15) printf %s $'/build/nix-test/main/gc-closure/store/ij8qwx0m6dvpmggqmq8zmhbaiw6ym8pg-dependencies-input-2\n/build/nix-test/main/gc-closure/store/hw6dm6 ...
+(gc-closure.sh:15) input2_out=/build/nix-test/main/gc-closure/store/ij8qwx0m6dvpmggqmq8zmhbaiw6ym8pg-dependencies-input-2
...
1/1 main - nix-functional-tests:gc-closure FAIL             1.06s   (exit status 141 or signal 13 SIGPIPE)
...
```

```console
$ rewind replay 2b0c9595
identical to the recording: 10719 events, 10543 context switches and 20150 read and write records over 83593 steps, and the whole machine at its 12 keyframes
```

`input2_out` got the right value, and the test still died of signal 13.

## What died

The events around the failure, and the processes just before it:

```console
$ rewind events 2b0c9595 --from 83329 --to 83429
     83333   526/527   exit_group(nix) exited:0
     83337   526/526   thread exit(nix) exited:0
     83339   359/359   SIGCHLD code=1 addr=0x0
     83344   359/359   fork() = 534
     83361   534/534   fork() = 535
     83366   534/534   fork() = 536
     83371   536/536   execve("/nix/store/2gfxiwls9hbgwdwcy43mprchwsq36mg6-coreutils-9.11/bin/head", ["head", "-n1"])
     83398   536/536   exit_group(head) exited:0
     83404   535/535   SIGPIPE code=0 addr=0x0
     83405   535/535   exit_group(bash) killed:SIGPIPE
     83408   534/534   SIGCHLD code=1 addr=0x0
     83409   534/534   exit_group(bash) exited:141
     83413   359/359   SIGCHLD code=1 addr=0x0
     83427   359/359   exit_group(bash) exited:141

$ rewind ps 2b0c9595 83377
     1 /init
    34   bash -e /nix/store/l622p70vy8k5sh7y5wizi5f2mic6ynpg-source-stdenv.sh /nix/store/shkw4qm9qcw5sc5n1k5jznc83ny02r39-default-builder.sh
   358     /nix/store/lb41b0anx1f98y9y5s9mdv97gjgsq740-python3-3.14.7/bin/python3.14 /nix/store/r72868g8znkk4m8pmwsn3cf8v3h84mr0-meson-1.12.1/bin/meson test --no-rebuild --print-errorlogs gc-closure
   359       /nix/store/10dxp0qxqxxsyiljrh2kp0xqhz6arhcx-bash-5.3p15/bin/bash -x -e -u -o pipefail gc-closure.sh
   534         /nix/store/10dxp0qxqxxsyiljrh2kp0xqhz6arhcx-bash-5.3p15/bin/bash -x -e -u -o pipefail gc-closure.sh (fork)
   535           /nix/store/10dxp0qxqxxsyiljrh2kp0xqhz6arhcx-bash-5.3p15/bin/bash -x -e -u -o pipefail gc-closure.sh (fork)
   536           head -n1
```

534 is the command substitution, 535 runs `printf` and 536
runs `head`. `head` exits 0, then 535 dies of SIGPIPE and the test
exits 141.

## Count the writes

`rewind cat` reads a file in a fork of the run at any step. `/proc/<pid>/io`
counts each process's writes:

```console
$ for step in 83382 83398 83404; do echo "step $step"; rewind cat 2b0c9595 $step /proc/535/io | grep -E '^(wchar|syscw)' | sed 's/^/  535 /'; rewind cat 2b0c9595 $step /proc/536/io | grep -E '^(wchar|syscw)' | sed 's/^/  536 /'; done
step 83382
  535 wchar: 0
  535 syscw: 0
  536 wchar: 30
  536 syscw: 1
step 83398
  535 wchar: 316
  535 syscw: 2
  536 wchar: 122
  536 syscw: 2
step 83404
  535 wchar: 316
  535 syscw: 3
rewind: /proc/536/io: no such file at this step
```

The first write of each is its `bash -x` trace line. At 83382 only `head`
has made it, and waits for input. By 83398 `printf` has made its
trace line and written the first store path, and `head` has echoed that line,
92 bytes, and exited. At 83404 `printf`'s third write adds no
bytes: the pipe has no reader left.

## Root cause

bash line-buffers its own standard output, so `printf` writes one line per
`write` call. On the host, with the same bash and the two paths from the run:

```console
$ export X=$'/build/nix-test/main/gc-closure/store/ij8qwx0m6dvpmggqmq8zmhbaiw6ym8pg-dependencies-input-2\n/build/nix-test/main/gc-closure/store/hw6dm6rpynd7hmhl1mnp0z4m28lfbx4b-dependencies-input-2-out2'
$ strace -f -e trace=write -o /dev/stdout bash -c 'set -o pipefail; y=$(printf "%s" "$X" | cat)' | grep write
1281845 write(1, "/build/nix-test/main/gc-closure/"..., 92) = 92
1281845 write(1, "/build/nix-test/main/gc-closure/"..., 96) = 96
```

The buffering is set in `shell_initialize` in bash 5.3's
[shell.c](https://git.savannah.gnu.org/cgit/bash.git/tree/shell.c?h=bash-5.3).
`head -n1` exits after the first line. If it does so between the two writes,
the second write raises SIGPIPE, `pipefail` makes the pipeline's status 141,
and `-e` ends the test.

## Who had the CPU

`rewind threads` prints which process held the CPU through each stretch of
steps:

```console
$ rewind threads 2b0c9595 --from 83371 --to 83409
     83371      83382       536/536  head
     83383      83383       535/535  bash
     83384      83394       358/358  meson
     83395      83396     kernel 11  ksoftirqd/0
     83397      83398       536/536  head
     83399      83399     kernel 11  ksoftirqd/0
     83400      83400       359/359  bash
     83401      83401       534/534  bash
     83402      83402     kernel 11  ksoftirqd/0
     83403      83405       535/535  bash
     83406      83406     kernel 11  ksoftirqd/0
     83407      83409       534/534  bash
```

`head` blocks in `read` by 83382. `printf` writes its trace line
and the first store path at 83383, which wakes `head`. `head` runs at 83397 and exits,
and `printf` only gets the CPU back at 83403; its next write, at
83404, raises the SIGPIPE.

## How often it fails

```console
$ rewind check --epoch 1792368000 --all --schedules 256 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-git-gc-closure'
schedule   0: exited:0            79614 steps  a50a5ab6d992  run cf9cb4f1d293bd85
schedule   1: exited:0            94200 steps  a50a5ab6d992  run 7416be5a177784a7
...
schedule  23: exited:1            83618 steps    run 2daf27a69c19dcf7
...
1 of 256 other schedules ended differently

schedule 23 ends differently from schedule 0; finding the step that decides it
schedule 23 limited to steps 709..83369 still ends differently
step 83368 decides it: a request to switch threads there makes the run fail

passing: run 802ec1c550b13d47, schedule 23 over steps 709..83368
failing: run 2b0c95958340658f, schedule 23 over steps 709..83369
the two are the same run until step 83368

where /nix/store/10dxp0qxqxxsyiljrh2kp0xqhz6arhcx-bash-5.3p15/bin/bash -x -e -u -o pipefail gc-closure.sh first behaves differently:
  both          83344   359/359   fork() = 534
  both          83361   534/534   fork() = 535
  both          83366   534/534   fork() = 536
  failing       83404   535/535   SIGPIPE code=0 addr=0x0
  failing       83405   535/535   exit_group(bash) killed:SIGPIPE
  failing       83408   534/534   SIGCHLD code=1 addr=0x0
  failing       83409   534/534   exit_group(bash) exited:141
  passing       83369   535/535   exit_group(bash) exited:0
  passing       83398   534/534   SIGCHLD code=1 addr=0x0
  passing       83399   534/534   exit_group(bash) exited:0
  passing       83402   359/359   SIGCHLD code=1 addr=0x0

open both in the desktop app: rewind open 2b0c95958340658f 83404 --compare 802ec1c550b13d47
```

Only schedule 23 of the 257 fails; the other 256 pass, the unperturbed one among them.

```console
$ rewind check --run 2b0c9595 --schedule-from 83361 --schedules 64 --all --no-narrow
schedule   0: exited:1            83593 steps    run 2b0c95958340658f
schedule   1: exited:0            93494 steps  a50a5ab6d992  run 0ebaf973f86d1fe5
schedule   2: exited:0            93556 steps  a50a5ab6d992  run 2ac066176599a077
...
schedule  23: exited:1            83618 steps    run 7b8f67f62d848812
...
schedule  64: exited:0            93628 steps  a50a5ab6d992  run d747e099c056f303
run 2b0c95958340658f failed; 63 of 64 other schedules ended differently
```

Perturbing only from step 83361, where the pipeline forks `printf`,
moves 63 of 64 schedules off the failing order. Schedule
23 fails the same way: `head` exits between `printf`'s two
writes, and `printf`, process 535, dies of SIGPIPE.

```console
$ rewind check --epoch 1792368000 --all --schedules 1024 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-gc-closure'
rewind: packing 217 store paths for nix-functional-gc-closure-2.35.2
...
schedule 287: exited:1            62711 steps    run 394b90901880995f
...
schedule 622: exited:1            62600 steps    run e3f519d2c8386267
...
schedule 626: exited:1            63235 steps    run 2552b9beb2e3c9e2
...
schedule 813: exited:1            82048 steps    run 275af0ee7efcf94b
...
4 of 1024 other schedules ended differently
...
```

Nix 2.35.2 from nixpkgs has the same line, and 4 of its
1025 runs fail, all with SIGPIPE.

On the host it fails only when the writer is made the low-priority task on its
CPU:

| What ran on the host                                       | Failed       |
| ---------------------------------------------------------- | ------------ |
| `gc-closure` from Nix master, 200 times in one build       | 0 of 200     |
| line 15 in a loop, 16 CPUs                                 | 0 of 5000    |
| line 15 in a loop, pinned to one CPU                       | 0 of 5000    |
| the same, with a busy loop sharing that CPU                | 0 of 20000   |
| the same, with the `printf` subshell reniced to 19, pinned | 1802 of 2000 |
| the same, reniced, not pinned                              | 1 of 2000    |

## The fix

Give `head` its input without a pipe, so there is no writer to kill:

```diff
-    input2_out=$(printf "%s" "$input2" | head -n1)
+    input2_out=$(head -n1 <<< "$input2")
```

Line 16 needs no change: `tail` reads to the end. With the fix, all 65
schedules pass:

```console
$ rewind check --epoch 1792368000 --all 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-git-gc-closure-fixed'
schedule   0: exited:0            79476 steps  a50a5ab6d992  run 93e1e61c11d74ce0
schedule   1: exited:0            93907 steps  a50a5ab6d992  run 35f60294114bde59
...
0 of 64 other schedules ended differently
same result under all 65 schedules
```

## The recording

Both files are in the
[case-studies release](https://github.com/fzakaria/rewindvm/releases/tag/case-studies):

- [nix-gc-closure-sigpipe.rwd](https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-gc-closure-sigpipe.rwd)
  (256.2 KB), the trace, for `rewind events`, `rewind log` and the app; the
  [viewer](https://rewindvm.dev/viewer/?url=examples/nix-gc-closure.rwd) opens it in a browser.
- [nix-gc-closure-sigpipe-replayable.rwd](https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-gc-closure-sigpipe-replayable.rwd)
  (522.6 MB), with the kernel, input image and keyframes, for `rewind replay`
  and `rewind shell` on an AMD Zen 2 or later.

```console
$ rewind import https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-gc-closure-sigpipe-replayable.rwd
$ rewind replay 2b0c9595
$ rewind shell 2b0c9595 <step>
$ rewind-app https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-gc-closure-sigpipe.rwd
```

The run ids above come from the derivations in the
[example flake](../../examples/case-studies/flake.nix), on nixpkgs
[b4fd65b1](https://github.com/NixOS/nixpkgs/commit/b4fd65b198c599cbe814fcb9f42d25d021595ec9),
at the epoch the commands pass. Any change to a derivation gives a different
set of runs.
