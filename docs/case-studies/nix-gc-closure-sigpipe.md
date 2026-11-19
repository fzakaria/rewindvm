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
$ rewind check --epoch 1792627200 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-git-gc-closure'
rewind: packing 217 store paths for nix-functional-gc-closure-2.36pre20260912_203f85b2
...
schedule  14: exited:1            64602 steps    run 8679777fcdbe572d
...

schedule 14 ends differently from schedule 0; finding the step that decides it
schedule 14 limited to steps 712..64354 still ends differently
step 64353 decides it: a request to switch threads there makes the run fail

passing: run f1847c57e08b4a5d, schedule 14 over steps 712..64353
failing: run c150b2dfda1a5b28, schedule 14 over steps 712..64354
the two are the same run until step 64353

where /nix/store/10dxp0qxqxxsyiljrh2kp0xqhz6arhcx-bash-5.3p15/bin/bash -x -e -u -o pipefail gc-closure.sh first behaves differently:
  both          64334   359/359   fork() = 478
  both          64338   478/478   fork() = 479
  both          64352   478/478   fork() = 480
  failing       64392   479/479   SIGPIPE code=0 addr=0x0
  failing       64393   479/479   exit_group(bash) killed:SIGPIPE
  failing       64396   478/478   SIGCHLD code=1 addr=0x0
  failing       64397   478/478   exit_group(bash) exited:141
  passing       64355   479/479   exit_group(bash) exited:0
  passing       64399   478/478   SIGCHLD code=1 addr=0x0
  passing       64400   478/478   exit_group(bash) exited:0
  passing       64404   359/359   SIGCHLD code=1 addr=0x0

open both in the desktop app: rewind open c150b2dfda1a5b28 64392 --compare f1847c57e08b4a5d
```

Schedule 14 fails, and `check` narrows its perturbations to the one
step that decides it, 64353. The failing run's log has the test's last
lines:

```console
$ rewind log c150b2df
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
1/1 main - nix-functional-tests:gc-closure FAIL             0.70s   (exit status 141 or signal 13 SIGPIPE)
...
```

```console
$ rewind replay c150b2df
identical to the recording: 10279 events, 9708 context switches and 18611 read and write records over 64580 steps, and the whole machine at its 10 keyframes
```

`input2_out` got the right value, and the test still died of signal 13.

## What died

The events around the failure, and the processes just before it:

```console
$ rewind events c150b2df --from 64317 --to 64417
     64322   470/470   thread exit(nix) exited:0
     64325   470/471   exit_group(nix) exited:0
     64329   359/359   SIGCHLD code=1 addr=0x0
     64334   359/359   fork() = 478
     64338   478/478   fork() = 479
     64352   478/478   fork() = 480
     64357   480/480   execve("/nix/store/2gfxiwls9hbgwdwcy43mprchwsq36mg6-coreutils-9.11/bin/head", ["head", "-n1"])
     64386   480/480   exit_group(head) exited:0
     64392   479/479   SIGPIPE code=0 addr=0x0
     64393   479/479   exit_group(bash) killed:SIGPIPE
     64396   478/478   SIGCHLD code=1 addr=0x0
     64397   478/478   exit_group(bash) exited:141
     64401   359/359   SIGCHLD code=1 addr=0x0
     64415   359/359   exit_group(bash) exited:141

$ rewind ps c150b2df 64363
     1 /init
    34   bash -e /nix/store/l622p70vy8k5sh7y5wizi5f2mic6ynpg-source-stdenv.sh /nix/store/shkw4qm9qcw5sc5n1k5jznc83ny02r39-default-builder.sh
   358     /nix/store/lb41b0anx1f98y9y5s9mdv97gjgsq740-python3-3.14.7/bin/python3.14 /nix/store/r72868g8znkk4m8pmwsn3cf8v3h84mr0-meson-1.12.1/bin/meson test --no-rebuild --print-errorlogs gc-closure
   359       /nix/store/10dxp0qxqxxsyiljrh2kp0xqhz6arhcx-bash-5.3p15/bin/bash -x -e -u -o pipefail gc-closure.sh
   478         /nix/store/10dxp0qxqxxsyiljrh2kp0xqhz6arhcx-bash-5.3p15/bin/bash -x -e -u -o pipefail gc-closure.sh (fork)
   479           /nix/store/10dxp0qxqxxsyiljrh2kp0xqhz6arhcx-bash-5.3p15/bin/bash -x -e -u -o pipefail gc-closure.sh (fork)
   480           head -n1
```

478 is the command substitution, 479 runs `printf` and 480
runs `head`. `head` exits 0, then 479 dies of SIGPIPE and the test
exits 141.

## Count the writes

`rewind cat` reads a file in a fork of the run at any step. `/proc/<pid>/io`
counts each process's writes:

```console
$ for step in 64368 64386 64392; do echo "step $step"; rewind cat c150b2df $step /proc/479/io | grep -E '^(wchar|syscw)' | sed 's/^/  479 /'; rewind cat c150b2df $step /proc/480/io | grep -E '^(wchar|syscw)' | sed 's/^/  480 /'; done
step 64368
  479 wchar: 0
  479 syscw: 0
  480 wchar: 30
  480 syscw: 1
step 64386
  479 wchar: 316
  479 syscw: 2
  480 wchar: 122
  480 syscw: 2
step 64392
  479 wchar: 316
  479 syscw: 3
rewind: /proc/480/io: no such file at this step
```

The first write of each is its `bash -x` trace line. At 64368 only
`head`'s has been made; `printf` has written nothing yet. By 64386
`printf` has made its trace line and written the first store path, and
`head` has echoed that store path, 92 bytes, and exited. At
64392 `printf`'s third write adds no bytes: the pipe has no reader
left.

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
$ rewind threads c150b2df --from 64357 --to 64397
     64357      64368       480/480  head
     64369      64369       479/479  bash
     64370      64382       358/358  meson
     64383      64384     kernel 11  ksoftirqd/0
     64385      64386       480/480  head
     64387      64387     kernel 11  ksoftirqd/0
     64388      64388       359/359  bash
     64389      64389       478/478  bash
     64390      64390     kernel 11  ksoftirqd/0
     64391      64393       479/479  bash
     64394      64394     kernel 11  ksoftirqd/0
     64395      64397       478/478  bash
```

`head` blocks in `read` by 64368. `printf` writes its trace line
and the first store path at 64369, which wakes `head`. `head` runs at 64385 and exits,
and `printf` only gets the CPU back at 64391; its next write, at
64392, raises the SIGPIPE.

## How often it fails

```console
$ rewind check --epoch 1792627200 --all --schedules 256 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-git-gc-closure'
schedule   0: exited:0            79679 steps  a50a5ab6d992  run 5bdb3ab0bc90c8c0
schedule   1: exited:0            93501 steps  a50a5ab6d992  run 157806c2025329bf
...
schedule  14: exited:1            64602 steps    run 8679777fcdbe572d
...
schedule  18: exited:1            43013 steps    run 7520644cccb59c7a
...
2 of 256 other schedules ended differently

schedule 14 ends differently from schedule 0; finding the step that decides it
schedule 14 limited to steps 712..64354 still ends differently
step 64353 decides it: a request to switch threads there makes the run fail

passing: run f1847c57e08b4a5d, schedule 14 over steps 712..64353
failing: run c150b2dfda1a5b28, schedule 14 over steps 712..64354
the two are the same run until step 64353

where /nix/store/10dxp0qxqxxsyiljrh2kp0xqhz6arhcx-bash-5.3p15/bin/bash -x -e -u -o pipefail gc-closure.sh first behaves differently:
  both          64334   359/359   fork() = 478
  both          64338   478/478   fork() = 479
  both          64352   478/478   fork() = 480
  failing       64392   479/479   SIGPIPE code=0 addr=0x0
  failing       64393   479/479   exit_group(bash) killed:SIGPIPE
  failing       64396   478/478   SIGCHLD code=1 addr=0x0
  failing       64397   478/478   exit_group(bash) exited:141
  passing       64355   479/479   exit_group(bash) exited:0
  passing       64399   478/478   SIGCHLD code=1 addr=0x0
  passing       64400   478/478   exit_group(bash) exited:0
  passing       64404   359/359   SIGCHLD code=1 addr=0x0

open both in the desktop app: rewind open c150b2dfda1a5b28 64392 --compare f1847c57e08b4a5d
```

2 of the 257 schedules fail, 14 and 18; the other 255 pass, the unperturbed one among them.

```console
$ rewind check --run c150b2df --schedule-from 64338 --schedules 64 --all --no-narrow
schedule   0: exited:1            64580 steps    run c150b2dfda1a5b28
schedule   1: exited:0            93945 steps  a50a5ab6d992  run 191e55c8584e563c
schedule   2: exited:0            93809 steps  a50a5ab6d992  run 93387cf771f66f65
...
schedule  14: exited:1            64602 steps    run c7ca73646ac0f681
...
schedule  64: exited:0            93917 steps  a50a5ab6d992  run 980dd0bce166f950
run c150b2dfda1a5b28 failed; 63 of 64 other schedules ended differently
```

Perturbing only from step 64338, where the pipeline forks `printf`,
moves 63 of 64 schedules off the failing order. Schedule
14 fails the same way: `head` exits between `printf`'s two
writes, and `printf`, process 479, dies of SIGPIPE.

```console
$ rewind check --epoch 1792627200 --all --schedules 1024 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-gc-closure'
rewind: packing 217 store paths for nix-functional-gc-closure-2.35.2
...
schedule 154: exited:1            81977 steps    run 7f4c63eefdb79b69
...
schedule 229: exited:1            42284 steps    run ea68ef404c97a3a7
...
schedule 524: exited:1            62925 steps    run 5fcbaba85fe7ff61
...
schedule 540: exited:1            41817 steps    run d4b55fbbf7e653f2
...
schedule 597: exited:1            82017 steps    run 4749fa9a8079583f
...
schedule 879: exited:1            63288 steps    run 14366dc173354094
...
schedule 975: exited:1            81916 steps    run 4fe36df4fd20b7b9
...
7 of 1024 other schedules ended differently
...
```

Nix 2.35.2 from nixpkgs has the same line, and 7 of its
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
$ rewind check --epoch 1792627200 --all 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-git-gc-closure-fixed'
schedule   0: exited:0            79660 steps  a50a5ab6d992  run c0dca173b49d423e
schedule   1: exited:0            94278 steps  a50a5ab6d992  run 20811ad52ab2d244
...
0 of 64 other schedules ended differently
same result under all 65 schedules
```

## The recording

Both files are in the
[case-studies release](https://github.com/fzakaria/rewindvm/releases/tag/case-studies):

- [nix-gc-closure-sigpipe.rwd](https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-gc-closure-sigpipe.rwd)
  (320.0 KB), the trace, for `rewind events`, `rewind log` and the app; the
  [viewer](https://rewindvm.dev/viewer/?url=examples/nix-gc-closure.rwd) opens it in a browser.
- [nix-gc-closure-sigpipe-replayable.rwd](https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-gc-closure-sigpipe-replayable.rwd)
  (516.7 MB), with the kernel, input image and keyframes, for `rewind replay`
  and `rewind shell` on an AMD Zen 2 or later.

```console
$ rewind import https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-gc-closure-sigpipe-replayable.rwd
$ rewind replay c150b2df
$ rewind shell c150b2df <step>
$ rewind-app https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-gc-closure-sigpipe.rwd
```

The run ids above come from the derivations in the
[example flake](../../examples/case-studies/flake.nix), on nixpkgs
[b4fd65b1](https://github.com/NixOS/nixpkgs/commit/b4fd65b198c599cbe814fcb9f42d25d021595ec9),
at the epoch the commands pass. Any change to a derivation gives a different
set of runs.
