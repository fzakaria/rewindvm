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
$ rewind check --epoch 1791936000 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-git-gc-closure'
rewind: packing 217 store paths for nix-functional-gc-closure-2.36pre20260912_203f85b2
...
schedule  16: exited:1            64293 steps    run dea53806ff1f5edb

schedule 16 ends differently from schedule 0; finding the step that decides it
schedule 16 limited to steps 2037..64194 still ends differently
step 64193 decides it: a 40 µs pause of the running thread there makes the run fail

passing: run 7805d77074cadf07, schedule 16 over steps 2037..64193
failing: run 34297008746b10e9, schedule 16 over steps 2037..64194
the two are the same run until step 64193

where /nix/store/10dxp0qxqxxsyiljrh2kp0xqhz6arhcx-bash-5.3p15/bin/bash -x -e -u -o pipefail gc-closure.sh first behaves differently:
  both          81436   359/359   fork() = 534
  both          81440   534/534   fork() = 535
  both          81443   534/534   fork() = 536
  failing       81491   535/535   SIGPIPE code=0 addr=0x0
  failing       81492   535/535   exit_group(bash) killed:SIGPIPE
  failing       81495   534/534   SIGCHLD code=1 addr=0x0
  failing       81496   534/534   exit_group(bash) exited:141
  passing       81370   535/535   exit_group(bash) exited:0
  passing       81398   534/534   SIGCHLD code=1 addr=0x0
  passing       81399   534/534   exit_group(bash) exited:0
  passing       81403   359/359   SIGCHLD code=1 addr=0x0

open both in the desktop app: rewind open 34297008746b10e9 81491 --compare 7805d77074cadf07
```

Schedule 16 fails, and `check` narrows its perturbations to the one
step that decides it, 64193. The failing run's log has the test's last
lines:

```console
$ rewind log 34297008
...
++(gc-closure.sh:14) nix build -f dependencies2.nix input2_drv --no-link --print-out-paths
...
+(gc-closure.sh:14) input2=$'/build/nix-test/main/gc-closure/store/ij8qwx0m6dvpmggqmq8zmhbaiw6ym8pg-dependencies-input-2\n/build/nix-test/main/gc-closure/store/hw6dm6rpyn ...
++(gc-closure.sh:15) head -n1
++(gc-closure.sh:15) printf %s $'/build/nix-test/main/gc-closure/store/ij8qwx0m6dvpmggqmq8zmhbaiw6ym8pg-dependencies-input-2\n/build/nix-test/main/gc-closure/store/hw6dm6 ...
+(gc-closure.sh:15) input2_out=/build/nix-test/main/gc-closure/store/ij8qwx0m6dvpmggqmq8zmhbaiw6ym8pg-dependencies-input-2
...
++(gc-closure.sh:14) nix build -f dependencies2.nix input2_drv --no-link --print-out-paths
...
+(gc-closure.sh:14) input2=$'/build/nix-test/main/gc-closure/store/ij8qwx0m6dvpmggqmq8zmhbaiw6ym8pg-dependencies-input-2\n/build/nix-test/main/gc-closure/store/hw6dm6rpyn ...
++(gc-closure.sh:15) printf %s $'/build/nix-test/main/gc-closure/store/ij8qwx0m6dvpmggqmq8zmhbaiw6ym8pg-dependencies-input-2\n/build/nix-test/main/gc-closure/store/hw6dm6 ...
++(gc-closure.sh:15) head -n1
+(gc-closure.sh:15) input2_out=/build/nix-test/main/gc-closure/store/ij8qwx0m6dvpmggqmq8zmhbaiw6ym8pg-dependencies-input-2
...
1/1 main - nix-functional-tests:gc-closure FAIL             1.03s   (exit status 141 or signal 13 SIGPIPE)
...
```

```console
$ rewind replay 34297008
identical to the recording: 10713 events, 9765 context switches and 20150 read and write records over 81663 steps, and the whole machine at its 12 keyframes
```

`input2_out` got the right value, and the test still died of signal 13.

## What died

The events around the failure, and the processes just before it:

```console
$ rewind events 34297008 --from 81416 --to 81516
     81419   526/527   thread exit(nix) exited:0
     81421   359/359   SIGCHLD code=1 addr=0x0
     81436   359/359   fork() = 534
     81440   534/534   fork() = 535
     81443   534/534   fork() = 536
     81459   536/536   execve("/nix/store/2gfxiwls9hbgwdwcy43mprchwsq36mg6-coreutils-9.11/bin/head", ["head", "-n1"])
     81484   536/536   exit_group(head) exited:0
     81491   535/535   SIGPIPE code=0 addr=0x0
     81492   535/535   exit_group(bash) killed:SIGPIPE
     81495   534/534   SIGCHLD code=1 addr=0x0
     81496   534/534   exit_group(bash) exited:141
     81500   359/359   SIGCHLD code=1 addr=0x0
     81514   359/359   exit_group(bash) exited:141

$ rewind ps 34297008 81465
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
$ for step in 81470 81484 81491; do echo "step $step"; rewind cat 34297008 $step /proc/535/io | grep -E '^(wchar|syscw)' | sed 's/^/  535 /'; rewind cat 34297008 $step /proc/536/io | grep -E '^(wchar|syscw)' | sed 's/^/  536 /'; done
step 81470
  535 wchar: 224
  535 syscw: 1
  536 wchar: 30
  536 syscw: 1
step 81484
  535 wchar: 316
  535 syscw: 2
  536 wchar: 122
  536 syscw: 2
step 81491
  535 wchar: 316
  535 syscw: 3
rewind: /proc/536/io: no such file at this step
```

The first write of each is its `bash -x` trace line. By 81484 `printf`
has written the first store path (92 bytes) and `head` has echoed
it and exited. At 81491 `printf`'s third write adds no bytes: the pipe
has no reader left.

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
$ rewind threads 34297008 --from 81457 --to 81496
     81457      81457       358/358  meson
     81458      81458       536/536  bash
     81459      81470       536/536  head
     81471      81471       535/535  bash
     81472      81482       358/358  meson
     81483      81484       536/536  head
     81485      81485     kernel 11  ksoftirqd/0
     81486      81487       359/359  bash
     81488      81488       534/534  bash
     81489      81489     kernel 11  ksoftirqd/0
     81490      81492       535/535  bash
     81493      81493     kernel 11  ksoftirqd/0
     81494      81496       534/534  bash
```

`head` blocks in `read` by 81470. `printf` makes its first write at
81471, which wakes `head`. `head` runs at 81483 and exits,
and `printf` only gets the CPU back at 81490; its next write, at
81491, raises the SIGPIPE.

## How often it fails

```console
$ rewind check --epoch 1791936000 --all --schedules 256 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-git-gc-closure'
schedule   0: exited:0            79508 steps  a50a5ab6d992  run 2ff2577e9f49beec
schedule   1: exited:0            94084 steps  a50a5ab6d992  run 11937aaf0e24ef9e
...
schedule  16: exited:1            64293 steps    run dea53806ff1f5edb
...
schedule 187: exited:1            83651 steps    run 215d269298c4648e
...
2 of 256 other schedules ended differently

schedule 16 ends differently from schedule 0; finding the step that decides it
schedule 16 limited to steps 2037..64194 still ends differently
step 64193 decides it: a 40 µs pause of the running thread there makes the run fail

passing: run 7805d77074cadf07, schedule 16 over steps 2037..64193
failing: run 34297008746b10e9, schedule 16 over steps 2037..64194
the two are the same run until step 64193

where /nix/store/10dxp0qxqxxsyiljrh2kp0xqhz6arhcx-bash-5.3p15/bin/bash -x -e -u -o pipefail gc-closure.sh first behaves differently:
  both          81436   359/359   fork() = 534
  both          81440   534/534   fork() = 535
  both          81443   534/534   fork() = 536
  failing       81491   535/535   SIGPIPE code=0 addr=0x0
  failing       81492   535/535   exit_group(bash) killed:SIGPIPE
  failing       81495   534/534   SIGCHLD code=1 addr=0x0
  failing       81496   534/534   exit_group(bash) exited:141
  passing       81370   535/535   exit_group(bash) exited:0
  passing       81398   534/534   SIGCHLD code=1 addr=0x0
  passing       81399   534/534   exit_group(bash) exited:0
  passing       81403   359/359   SIGCHLD code=1 addr=0x0

open both in the desktop app: rewind open 34297008746b10e9 81491 --compare 7805d77074cadf07
```

2 of the 257 schedules fail, 16 and 187; the other 255
pass, the unperturbed one among them.

```console
$ rewind check --run 34297008 --schedule-from 81440 --schedules 64 --all --no-narrow
schedule   0: exited:1            81663 steps    run 34297008746b10e9
schedule   1: exited:0            91641 steps  a50a5ab6d992  run e96f7599aef4adef
schedule   2: exited:0            91690 steps  a50a5ab6d992  run 1ba6817ef6bc1ead
...
schedule   7: exited:1            81694 steps    run 00184e012484739e
...
schedule  14: exited:1            81683 steps    run baa572771c8f0824
...
schedule  64: exited:0            91672 steps  a50a5ab6d992  run cdf29324195bf0f1
run 34297008746b10e9 failed; 62 of 64 other schedules ended differently
```

Perturbing only from step 81440, where the pipeline forks `printf`,
moves 62 of 64 schedules off the failing order. Schedule
7 fails the same way: `head` exits between `printf`'s two
writes, and `printf`, process 535, dies of SIGPIPE.

```console
$ rewind check --epoch 1791936000 --all --schedules 1024 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-gc-closure'
rewind: packing 217 store paths for nix-functional-gc-closure-2.35.2
...
schedule 149: exited:1            62982 steps    run e97af69ee8d2ae0c
...
schedule 680: exited:1            62639 steps    run 54c71199d0809773
...
schedule 735: exited:1            42024 steps    run 44bc927a671323e8
...
schedule 747: exited:1            62182 steps    run ba5be675e94a52fa
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
$ rewind check --epoch 1791936000 --all 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-git-gc-closure-fixed'
schedule   0: exited:0            79546 steps  a50a5ab6d992  run 31fbd37ade28c50c
schedule   1: exited:0            93763 steps  a50a5ab6d992  run f106bbfeb007a017
...
0 of 64 other schedules ended differently
same result under all 65 schedules
```

## The recording

Both files are in the
[case-studies release](https://github.com/fzakaria/rewindvm/releases/tag/case-studies):

- [nix-gc-closure-sigpipe.rwd](https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-gc-closure-sigpipe.rwd)
  (254.4 KB), the trace, for `rewind events`, `rewind log` and the app; the
  [viewer](https://rewindvm.dev/viewer/?url=examples/nix-gc-closure.rwd) opens it in a browser.
- [nix-gc-closure-sigpipe-replayable.rwd](https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-gc-closure-sigpipe-replayable.rwd)
  (533.0 MB), with the kernel, input image and keyframes, for `rewind replay`
  and `rewind shell` on an AMD Zen 2 or later.

```console
$ rewind import https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-gc-closure-sigpipe-replayable.rwd
$ rewind replay 34297008
$ rewind shell 34297008 <step>
$ rewind-app https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-gc-closure-sigpipe.rwd
```

The run ids above come from the derivations in the
[example flake](../../examples/case-studies/flake.nix), on nixpkgs
[b4fd65b1](https://github.com/NixOS/nixpkgs/commit/b4fd65b198c599cbe814fcb9f42d25d021595ec9),
at the epoch the commands pass. Any change to a derivation gives a different
set of runs.
