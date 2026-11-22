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
$ rewind check --epoch 1793318400 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-git-gc-closure'
rewind: packing 217 store paths for nix-functional-gc-closure-2.36pre20260912_203f85b2
...
schedule  37: exited:1            64518 steps    run f7284fdbab2fd66f
...

schedule 37 ends differently from schedule 0; finding the step that decides it
schedule 37 limited to steps 723..64293 still ends differently
step 64292 decides it: a request to switch threads there makes the run fail

passing: run 0f230238b8df849f, schedule 37 over steps 723..64292
failing: run ca143a58325125d2, schedule 37 over steps 723..64293
the two are the same run until step 64292

where /nix/store/10dxp0qxqxxsyiljrh2kp0xqhz6arhcx-bash-5.3p15/bin/bash -x -e -u -o pipefail gc-closure.sh first behaves differently:
  both          64283   359/359   fork() = 478
  both          64287   478/478   fork() = 479
  both          64290   478/478   fork() = 480
  failing       64328   479/479   SIGPIPE code=0 addr=0x0
  failing       64329   479/479   exit_group(bash) killed:SIGPIPE
  failing       64332   478/478   SIGCHLD code=1 addr=0x0
  failing       64333   478/478   exit_group(bash) exited:141
  passing       64293   479/479   exit_group(bash) exited:0
  passing       64322   478/478   SIGCHLD code=1 addr=0x0
  passing       64323   478/478   exit_group(bash) exited:0
  passing       64328   359/359   SIGCHLD code=1 addr=0x0

open both in the desktop app: rewind open ca143a58325125d2 64328 --compare 0f230238b8df849f
```

Schedule 37 fails, and `check` narrows its perturbations to the one
step that decides it, 64292. The failing run's log has the test's last
lines:

```console
$ rewind log ca143a58
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
1/1 main - nix-functional-tests:gc-closure FAIL             0.69s   (exit status 141 or signal 13 SIGPIPE)
...
```

```console
$ rewind replay ca143a58
identical to the recording: 10281 events, 9915 context switches and 18611 read and write records over 64502 steps, and the whole machine at its 11 keyframes
```

`input2_out` got the right value, and the test still died of signal 13.

## What died

The events around the failure, and the processes just before it:

```console
$ rewind events ca143a58 --from 64253 --to 64353
     64254   470/470   close("/build/nix-test/main/gc-closure/store/ij8qwx0m6dvpmggqmq8zmhbaiw6ym8pg-dependencies-input-2.lock")
     64264   470/470   thread exit(nix) exited:0
     64268   470/471   exit_group(nix) exited:0
     64270   359/359   SIGCHLD code=1 addr=0x0
     64283   359/359   fork() = 478
     64287   478/478   fork() = 479
     64290   478/478   fork() = 480
     64295   480/480   execve("/nix/store/2gfxiwls9hbgwdwcy43mprchwsq36mg6-coreutils-9.11/bin/head", ["head", "-n1"])
     64309   480/480   exit_group(head) exited:0
     64328   479/479   SIGPIPE code=0 addr=0x0
     64329   479/479   exit_group(bash) killed:SIGPIPE
     64332   478/478   SIGCHLD code=1 addr=0x0
     64333   478/478   exit_group(bash) exited:141
     64337   359/359   SIGCHLD code=1 addr=0x0
     64351   359/359   exit_group(bash) exited:141

$ rewind ps ca143a58 64301
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
$ for step in 64306 64308 64328; do echo "step $step"; rewind cat ca143a58 $step /proc/479/io | grep -E '^(wchar|syscw)' | sed 's/^/  479 /'; rewind cat ca143a58 $step /proc/480/io | grep -E '^(wchar|syscw)' | sed 's/^/  480 /'; done
step 64306
  479 wchar: 0
  479 syscw: 0
  480 wchar: 30
  480 syscw: 1
step 64308
  479 wchar: 316
  479 syscw: 2
  480 wchar: 30
  480 syscw: 1
step 64328
  479 wchar: 316
  479 syscw: 3
rewind: /proc/480/io: no such file at this step
```

The first write of each is its `bash -x` trace line. At 64306 only
`head`'s has been made; `printf` has written nothing yet. By 64308
`printf` has made its trace line and written the first store path. In the
next step, 64309, `head` echoes that store path, the
92 bytes `input2_out` holds with its newline, and exits, so no
step shows its count after the echo. At 64328 `printf`'s third write
adds no bytes: the pipe has no reader left.

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
$ rewind threads ca143a58 --from 64295 --to 64333
     64295      64306       480/480  head
     64307      64307       479/479  bash
     64308      64309       480/480  head
     64310      64321       358/358  meson
     64322      64322     kernel 11  ksoftirqd/0
     64323      64324       359/359  bash
     64325      64325       478/478  bash
     64326      64326     kernel 11  ksoftirqd/0
     64327      64329       479/479  bash
     64330      64330     kernel 11  ksoftirqd/0
     64331      64333       478/478  bash
```

`head` blocks in `read` by 64306. `printf` writes its trace line
and the first store path at 64307, which wakes `head`. `head` runs at 64308 and exits,
and `printf` only gets the CPU back at 64327; its next write, at
64328, raises the SIGPIPE.

## How often it fails

```console
$ rewind check --epoch 1793318400 --all --schedules 256 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-git-gc-closure'
schedule   0: exited:0            79875 steps  a50a5ab6d992  run 5658330d0941ceae
schedule   1: exited:0            94435 steps  a50a5ab6d992  run 5a1439405f1b0ce2
...
schedule  37: exited:1            64518 steps    run f7284fdbab2fd66f
...
1 of 256 other schedules ended differently

schedule 37 ends differently from schedule 0; finding the step that decides it
schedule 37 limited to steps 723..64293 still ends differently
step 64292 decides it: a request to switch threads there makes the run fail

passing: run 0f230238b8df849f, schedule 37 over steps 723..64292
failing: run ca143a58325125d2, schedule 37 over steps 723..64293
the two are the same run until step 64292

where /nix/store/10dxp0qxqxxsyiljrh2kp0xqhz6arhcx-bash-5.3p15/bin/bash -x -e -u -o pipefail gc-closure.sh first behaves differently:
  both          64283   359/359   fork() = 478
  both          64287   478/478   fork() = 479
  both          64290   478/478   fork() = 480
  failing       64328   479/479   SIGPIPE code=0 addr=0x0
  failing       64329   479/479   exit_group(bash) killed:SIGPIPE
  failing       64332   478/478   SIGCHLD code=1 addr=0x0
  failing       64333   478/478   exit_group(bash) exited:141
  passing       64293   479/479   exit_group(bash) exited:0
  passing       64322   478/478   SIGCHLD code=1 addr=0x0
  passing       64323   478/478   exit_group(bash) exited:0
  passing       64328   359/359   SIGCHLD code=1 addr=0x0

open both in the desktop app: rewind open ca143a58325125d2 64328 --compare 0f230238b8df849f
```

Only schedule 37 of the 257 fails; the other 256 pass, the unperturbed one among them.

```console
$ rewind check --run ca143a58 --schedule-from 64287 --schedules 64 --all --no-narrow
schedule   0: exited:1            64502 steps    run ca143a58325125d2
schedule   1: exited:0            93684 steps  a50a5ab6d992  run 05a809a96a687c11
schedule   2: exited:0            93718 steps  a50a5ab6d992  run 2adbc7531d27f545
...
schedule  37: exited:1            64518 steps    run 4e9e0537fc2ed3b9
...
schedule  64: exited:0            94086 steps  a50a5ab6d992  run dba32f4c5a48242a
run ca143a58325125d2 failed; 63 of 64 other schedules ended differently
```

Perturbing only from step 64287, where the pipeline forks `printf`,
moves 63 of 64 schedules off the failing order. Schedule
37 fails the same way: `head` exits between `printf`'s two
writes, and `printf`, process 479, dies of SIGPIPE.

```console
$ rewind check --epoch 1793318400 --all --schedules 1024 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-gc-closure'
rewind: packing 217 store paths for nix-functional-gc-closure-2.35.2
...
schedule 196: exited:1            82660 steps    run a03d37e0a716aefa
...
schedule 650: exited:1            62884 steps    run 2a4579dc8e14fd70
...
schedule 741: exited:1            82209 steps    run 7e7ff2067ad9d371
...
schedule 904: exited:1            82121 steps    run cb695778aee3c5ad
...
schedule 935: exited:1            82500 steps    run 87be2b02f02e12a6
...
5 of 1024 other schedules ended differently
...
```

Nix 2.35.2 from nixpkgs has the same line, and 5 of its
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
$ rewind check --epoch 1793318400 --all 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-git-gc-closure-fixed'
schedule   0: exited:0            79680 steps  a50a5ab6d992  run f015240c164ff130
schedule   1: exited:0            94146 steps  a50a5ab6d992  run 28db6f952bb16e0a
...
0 of 64 other schedules ended differently
same result under all 65 schedules
```

## The recording

Both files are in the
[case-studies release](https://github.com/fzakaria/rewindvm/releases/tag/case-studies):

- [nix-gc-closure-sigpipe.rwd](https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-gc-closure-sigpipe.rwd)
  (320.8 KB), the trace, for `rewind events`, `rewind log` and the app; the
  [viewer](https://rewindvm.dev/viewer/?url=examples/nix-gc-closure.rwd) opens it in a browser.
- [nix-gc-closure-sigpipe-replayable.rwd](https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-gc-closure-sigpipe-replayable.rwd)
  (608.2 MB), with the kernel, input image and keyframes, for `rewind replay`
  and `rewind shell` on an AMD Zen 2 or later.

```console
$ rewind import https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-gc-closure-sigpipe-replayable.rwd
$ rewind replay ca143a58
$ rewind shell ca143a58 <step>
$ rewind-app https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-gc-closure-sigpipe.rwd
```

The run ids above come from the derivations in the
[example flake](../../examples/case-studies/flake.nix), on nixpkgs
[b4fd65b1](https://github.com/NixOS/nixpkgs/commit/b4fd65b198c599cbe814fcb9f42d25d021595ec9),
at the epoch the commands pass. Any change to a derivation gives a different
set of runs.
