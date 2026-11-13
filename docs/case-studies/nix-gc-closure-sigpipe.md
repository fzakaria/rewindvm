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
$ rewind check --epoch 1792454400 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-git-gc-closure'
rewind: packing 217 store paths for nix-functional-gc-closure-2.36pre20260912_203f85b2
...
schedule  49: exited:1            42548 steps    run d9548363e90c301e
...

schedule 49 ends differently from schedule 0; finding the step that decides it
schedule 49 limited to steps 709..42298 still ends differently
step 42297 decides it: a request to switch threads and a timer firing 5.2 µs late there makes the run fail

passing: run 51603debfbb1836d, schedule 49 over steps 709..42297
failing: run b428e28cbccac63e, schedule 49 over steps 709..42298
the two are the same run until step 42297

where /nix/store/10dxp0qxqxxsyiljrh2kp0xqhz6arhcx-bash-5.3p15/bin/bash -x -e -u -o pipefail gc-closure.sh first behaves differently:
  both          42281   359/359   fork() = 416
  both          42285   416/416   fork() = 417
  both          42288   416/416   fork() = 418
  failing       42350   417/417   SIGPIPE code=0 addr=0x0
  failing       42351   417/417   exit_group(bash) killed:SIGPIPE
  failing       42354   416/416   SIGCHLD code=1 addr=0x0
  failing       42355   416/416   exit_group(bash) exited:141
  passing       42312   417/417   exit_group(bash) exited:0
  passing       42345   416/416   SIGCHLD code=1 addr=0x0
  passing       42346   416/416   exit_group(bash) exited:0
  passing       42348   359/359   SIGCHLD code=1 addr=0x0

open both in the desktop app: rewind open b428e28cbccac63e 42350 --compare 51603debfbb1836d
```

Schedule 49 fails, and `check` narrows its perturbations to the one
step that decides it, 42297. The failing run's log has the test's last
lines:

```console
$ rewind log b428e28c
...
+(gc-closure.sh:45) nix_gc_closure false
...
++(gc-closure.sh:14) nix build -f dependencies2.nix input2_drv --no-link --print-out-paths
...
+(gc-closure.sh:14) input2=$'/build/nix-test/main/gc-closure/store/ij8qwx0m6dvpmggqmq8zmhbaiw6ym8pg-dependencies-input-2\n/build/nix-test/main/gc-closure/store/hw6dm6rpyn ...
++(gc-closure.sh:15) printf %s $'/build/nix-test/main/gc-closure/store/ij8qwx0m6dvpmggqmq8zmhbaiw6ym8pg-dependencies-input-2\n/build/nix-test/main/gc-closure/store/hw6dm6 ...
++(gc-closure.sh:15) head -n1
+(gc-closure.sh:15) input2_out=/build/nix-test/main/gc-closure/store/ij8qwx0m6dvpmggqmq8zmhbaiw6ym8pg-dependencies-input-2
...
1/1 main - nix-functional-tests:gc-closure FAIL             0.29s   (exit status 141 or signal 13 SIGPIPE)
...
```

```console
$ rewind replay b428e28c
identical to the recording: 9806 events, 8118 context switches and 16923 read and write records over 42531 steps, and the whole machine at its 8 keyframes
```

`input2_out` got the right value, and the test still died of signal 13.

## What died

The events around the failure, and the processes just before it:

```console
$ rewind events b428e28c --from 42275 --to 42375
     42281   359/359   fork() = 416
     42285   416/416   fork() = 417
     42288   416/416   fork() = 418
     42329   418/418   execve("/nix/store/2gfxiwls9hbgwdwcy43mprchwsq36mg6-coreutils-9.11/bin/head", ["head", "-n1"])
     42344   418/418   exit_group(head) exited:0
     42350   417/417   SIGPIPE code=0 addr=0x0
     42351   417/417   exit_group(bash) killed:SIGPIPE
     42354   416/416   SIGCHLD code=1 addr=0x0
     42355   416/416   exit_group(bash) exited:141
     42359   359/359   SIGCHLD code=1 addr=0x0
     42373   359/359   exit_group(bash) exited:141

$ rewind ps b428e28c 42335
     1 /init
    34   bash -e /nix/store/l622p70vy8k5sh7y5wizi5f2mic6ynpg-source-stdenv.sh /nix/store/shkw4qm9qcw5sc5n1k5jznc83ny02r39-default-builder.sh
   358     /nix/store/lb41b0anx1f98y9y5s9mdv97gjgsq740-python3-3.14.7/bin/python3.14 /nix/store/r72868g8znkk4m8pmwsn3cf8v3h84mr0-meson-1.12.1/bin/meson test --no-rebuild --print-errorlogs gc-closure
   359       /nix/store/10dxp0qxqxxsyiljrh2kp0xqhz6arhcx-bash-5.3p15/bin/bash -x -e -u -o pipefail gc-closure.sh
   416         /nix/store/10dxp0qxqxxsyiljrh2kp0xqhz6arhcx-bash-5.3p15/bin/bash -x -e -u -o pipefail gc-closure.sh (fork)
   417           /nix/store/10dxp0qxqxxsyiljrh2kp0xqhz6arhcx-bash-5.3p15/bin/bash -x -e -u -o pipefail gc-closure.sh (fork)
   418           head -n1
```

416 is the command substitution, 417 runs `printf` and 418
runs `head`. `head` exits 0, then 417 dies of SIGPIPE and the test
exits 141.

## Count the writes

`rewind cat` reads a file in a fork of the run at any step. `/proc/<pid>/io`
counts each process's writes:

```console
$ for step in 42302 42344 42350; do echo "step $step"; rewind cat b428e28c $step /proc/417/io | grep -E '^(wchar|syscw)' | sed 's/^/  417 /'; rewind cat b428e28c $step /proc/418/io | grep -E '^(wchar|syscw)' | sed 's/^/  418 /'; done
step 42302
  417 wchar: 224
  417 syscw: 1
  418 wchar: 0
  418 syscw: 0
step 42344
  417 wchar: 316
  417 syscw: 2
  418 wchar: 122
  418 syscw: 2
step 42350
  417 wchar: 316
  417 syscw: 3
rewind: /proc/418/io: no such file at this step
```

The first write of each is its `bash -x` trace line. At 42302 only
`printf`'s has been made; `head` has written nothing yet. By 42344
`printf` has written the first store path too, 92 bytes, and
`head` has made its trace line, echoed that store path and exited. At 42350 `printf`'s third write adds no
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
$ rewind threads b428e28c --from 42329 --to 42355
     42329      42340       418/418  head
     42341      42341       417/417  bash
     42342      42342     kernel 11  ksoftirqd/0
     42343      42344       418/418  head
     42345      42345     kernel 11  ksoftirqd/0
     42346      42346       416/416  bash
     42347      42347     kernel 11  ksoftirqd/0
     42348      42348       359/359  bash
     42349      42351       417/417  bash
     42352      42352     kernel 11  ksoftirqd/0
     42353      42355       416/416  bash
```

`head` blocks in `read` by 42340. `printf` writes its trace line
and the first store path at 42341, which wakes `head`. `head` runs at 42343 and exits,
and `printf` only gets the CPU back at 42349; its next write, at
42350, raises the SIGPIPE.

## How often it fails

```console
$ rewind check --epoch 1792454400 --all --schedules 256 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-git-gc-closure'
schedule   0: exited:0            79764 steps  a50a5ab6d992  run 756d76215d5f5237
schedule   1: exited:0            93519 steps  a50a5ab6d992  run c7b783e5b40a716d
...
schedule  49: exited:1            42548 steps    run d9548363e90c301e
...
schedule 158: exited:1            82969 steps    run aa10d41cf5b28cdf
...
2 of 256 other schedules ended differently

schedule 49 ends differently from schedule 0; finding the step that decides it
schedule 49 limited to steps 709..42298 still ends differently
step 42297 decides it: a request to switch threads and a timer firing 5.2 µs late there makes the run fail

passing: run 51603debfbb1836d, schedule 49 over steps 709..42297
failing: run b428e28cbccac63e, schedule 49 over steps 709..42298
the two are the same run until step 42297

where /nix/store/10dxp0qxqxxsyiljrh2kp0xqhz6arhcx-bash-5.3p15/bin/bash -x -e -u -o pipefail gc-closure.sh first behaves differently:
  both          42281   359/359   fork() = 416
  both          42285   416/416   fork() = 417
  both          42288   416/416   fork() = 418
  failing       42350   417/417   SIGPIPE code=0 addr=0x0
  failing       42351   417/417   exit_group(bash) killed:SIGPIPE
  failing       42354   416/416   SIGCHLD code=1 addr=0x0
  failing       42355   416/416   exit_group(bash) exited:141
  passing       42312   417/417   exit_group(bash) exited:0
  passing       42345   416/416   SIGCHLD code=1 addr=0x0
  passing       42346   416/416   exit_group(bash) exited:0
  passing       42348   359/359   SIGCHLD code=1 addr=0x0

open both in the desktop app: rewind open b428e28cbccac63e 42350 --compare 51603debfbb1836d
```

2 of the 257 schedules fail, 49 and 158; the other 255 pass, the unperturbed one among them.

```console
$ rewind check --run b428e28c --schedule-from 42285 --schedules 64 --all --no-narrow
schedule   0: exited:1            42531 steps    run b428e28cbccac63e
schedule   1: exited:0            93492 steps  a50a5ab6d992  run b32f31ea4a99c448
schedule   2: exited:0            92922 steps  a50a5ab6d992  run d20de1822829e9ff
...
schedule  49: exited:1            42548 steps    run c6bfa70b7a6ab575
...
schedule  58: exited:1            42529 steps    run 95da07a2b0939e87
...
schedule  64: exited:0            93043 steps  a50a5ab6d992  run aeed2674885abe28
run b428e28cbccac63e failed; 62 of 64 other schedules ended differently
```

Perturbing only from step 42285, where the pipeline forks `printf`,
moves 62 of 64 schedules off the failing order. Schedule
49 fails the same way: `head` exits between `printf`'s two
writes, and `printf`, process 417, dies of SIGPIPE.

```console
$ rewind check --epoch 1792454400 --all --schedules 1024 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-gc-closure'
rewind: packing 217 store paths for nix-functional-gc-closure-2.35.2
...
schedule 943: exited:1            41956 steps    run 760c5d6410167584
...
1 of 1024 other schedules ended differently
...
```

Nix 2.35.2 from nixpkgs has the same line, and 1 of its
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
$ rewind check --epoch 1792454400 --all 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-git-gc-closure-fixed'
schedule   0: exited:0            79525 steps  a50a5ab6d992  run 546b69bb0fe82fe2
schedule   1: exited:0            93833 steps  a50a5ab6d992  run e725fbc88de3b0b5
...
0 of 64 other schedules ended differently
same result under all 65 schedules
```

## The recording

Both files are in the
[case-studies release](https://github.com/fzakaria/rewindvm/releases/tag/case-studies):

- [nix-gc-closure-sigpipe.rwd](https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-gc-closure-sigpipe.rwd)
  (260.5 KB), the trace, for `rewind events`, `rewind log` and the app; the
  [viewer](https://rewindvm.dev/viewer/?url=examples/nix-gc-closure.rwd) opens it in a browser.
- [nix-gc-closure-sigpipe-replayable.rwd](https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-gc-closure-sigpipe-replayable.rwd)
  (513.5 MB), with the kernel, input image and keyframes, for `rewind replay`
  and `rewind shell` on an AMD Zen 2 or later.

```console
$ rewind import https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-gc-closure-sigpipe-replayable.rwd
$ rewind replay b428e28c
$ rewind shell b428e28c <step>
$ rewind-app https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-gc-closure-sigpipe.rwd
```

The run ids above come from the derivations in the
[example flake](../../examples/case-studies/flake.nix), on nixpkgs
[b4fd65b1](https://github.com/NixOS/nixpkgs/commit/b4fd65b198c599cbe814fcb9f42d25d021595ec9),
at the epoch the commands pass. Any change to a derivation gives a different
set of runs.
