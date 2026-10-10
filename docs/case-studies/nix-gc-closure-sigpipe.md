# Case study: a SIGPIPE in Nix's gc-closure test

Nix's functional test `gc-closure.sh` can fail with exit status 141. Rewind VM
failed it on the first run; the host passed it 200 times. We reported it as
[NixOS/nix#16546](https://github.com/NixOS/nix/issues/16546), and the fix
below was merged as [NixOS/nix#16547](https://github.com/NixOS/nix/pull/16547).

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
([flake](../../examples/case-studies/flake.nix)). It fails on schedule 0:

```console
$ rewind nix 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-git-gc-closure'
rewind: packing 217 store paths for nix-functional-gc-closure-2.36pre20260912_203f85b2
...
+(gc-closure.sh:45) nix_gc_closure false
...
++(gc-closure.sh:14) nix build -f dependencies2.nix input2_drv --no-link --print-out-paths
...
+(gc-closure.sh:14) input2=$'/build/nix-test/main/gc-closure/store/ij8qwx0m6dvpmggqmq8zmhbaiw6ym8pg-dependencies-input-2\n/build/nix-test/main/gc-closure/store/hw6dm6...
++(gc-closure.sh:15) printf %s $'/build/nix-test/main/gc-closure/store/ij8qwx0m6dvpmggqmq8zmhbaiw6ym8pg-dependencies-input-2\n/build/nix-test/main/gc-closure/store/hw...
++(gc-closure.sh:15) head -n1
+(gc-closure.sh:15) input2_out=/build/nix-test/main/gc-closure/store/ij8qwx0m6dvpmggqmq8zmhbaiw6ym8pg-dependencies-input-2
...
1/1 main - nix-functional-tests:gc-closure FAIL             0.26s   (exit status 141 or signal 13 SIGPIPE)
...
rewind: run f1584358a5e13e59 exited:1 after 33576 steps, 1.416s virtual, 5.997s wall (poweroff)

$ rewind replay f1584358
identical: 6180 events over 33576 steps
```

`input2_out` got the right value, and the test still died of signal 13.

## What died

The events around the failure, and the processes just before it:

```console
$ rewind events f1584358 --from 33313 --to 33413
     33323   359/359   fork() = 416
     33326   416/416   fork() = 417
     33329   416/416   fork() = 418
     33359   418/418   execve("/nix/store/2gfxiwls9hbgwdwcy43mprchwsq36mg6-coreutils-9.11/bin/head", ["head", "-n1"])
     33382   418/418   exit_group(head) exited:0
     33388   417/417   SIGPIPE code=0 addr=0x0
     33389   417/417   exit_group(bash) killed:SIGPIPE
     33391   416/416   SIGCHLD code=1 addr=0x0
     33392   416/416   exit_group(bash) exited:141
     33395   359/359   SIGCHLD code=1 addr=0x0
     33408   359/359   exit_group(bash) exited:141

$ rewind ps f1584358 33365
...
   359       /nix/store/10dxp0qxqxxsyiljrh2kp0xqhz6arhcx-bash-5.3p15/bin/bash -x -e -u -o pipefail gc-closure.sh
   416         /nix/store/10dxp0qxqxxsyiljrh2kp0xqhz6arhcx-bash-5.3p15/bin/bash -x -e -u -o pipefail gc-closure.sh (fork)
   417           /nix/store/10dxp0qxqxxsyiljrh2kp0xqhz6arhcx-bash-5.3p15/bin/bash -x -e -u -o pipefail gc-closure.sh (fork)
   418           head -n1
```

416 is the command substitution, 417 runs `printf` and 418 runs `head`. `head`
exits 0, then 417 dies of SIGPIPE and the test exits 141.

## Count the writes

`rewind cat` reads a file in a fork of the run at any step. `/proc/<pid>/io`
counts each process's writes:

```console
$ for step in 33378 33382 33388; do
>   echo "step $step"
>   rewind cat f1584358 $step /proc/417/io | grep -E '^(wchar|syscw)' | sed 's/^/  417 /'
>   rewind cat f1584358 $step /proc/418/io | grep -E '^(wchar|syscw)' | sed 's/^/  418 /'
> done
step 33378
  417 wchar: 224
  417 syscw: 1
  418 wchar: 30
  418 syscw: 1
step 33382
  417 wchar: 316
  417 syscw: 2
  418 wchar: 122
  418 syscw: 2
step 33388
  417 wchar: 316
  417 syscw: 3
rewind: /proc/418/io: no such file at this step
```

The first write of each is its `bash -x` trace line. By 33382 `printf` has
written the first store path (92 bytes) and `head` has echoed it and exited.
At 33388 `printf`'s third write adds no bytes: the pipe has no reader left.

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
$ rewind threads f1584358 --from 33357 --to 33393
     33357      33357       358/358  meson
     33358      33358       418/418  bash
     33359      33374       418/418  head
     33375      33379       417/417  bash
     33380      33380     kernel 11  ksoftirqd/0
     33381      33382       418/418  head
     33383      33383     kernel 11  ksoftirqd/0
     33384      33385       359/359  bash
     33386      33386       416/416  bash
     33387      33387     kernel 11  ksoftirqd/0
     33388      33389       417/417  bash
     33390      33390     kernel 11  ksoftirqd/0
     33391      33392       416/416  bash
     33393      33393     kernel 11  ksoftirqd/0
```

`head` blocks in `read` by 33374. `printf` makes its first write at 33375,
which wakes `head`. `head` runs at 33381 and exits, and `printf` only gets the
CPU back at 33388.

## How often it fails

```console
$ rewind check --all --schedules 256 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-git-gc-closure'
schedule   0: exited:1      33576 steps    run f1584358a5e13e59
schedule   1: exited:0      89128 steps  a50a5ab6d992  run 382fb99cb0bac3e9
...
schedule  51: exited:1      78929 steps    run 83f4f7129a70792d
...
schedule 130: exited:1      38958 steps    run 90d4b6513ee1a5da
...
schedule 0 failed; 254 of 256 perturbed schedules ended differently

schedule 1 passes where schedule 0 fails; narrowing the steps it perturbs
perturbing only steps 33377..33820 still ends differently
step 33819 decides it: a 80 µs stall there makes the run pass

passing: run c50d1724631c3213, schedule 1 over steps 33377..33820
failing: run 2c347a1c2bcc5dbd, schedule 1 over steps 33377..33819
the two are the same run until step 33819

where /nix/store/10dxp0qxqxxsyiljrh2kp0xqhz6arhcx-bash-5.3p15/bin/bash -x -e -u -o pipefail gc-closure.sh first behaves differently:
  both          70479   359/359   fork() = 534
  both          70482   534/534   fork() = 535
  both          70485   534/534   fork() = 536
  failing       70544   535/535   SIGPIPE code=0 addr=0x0
  failing       70545   535/535   exit_group(bash) killed:SIGPIPE
  failing       70547   534/534   SIGCHLD code=1 addr=0x0
  failing       70548   534/534   exit_group(bash) exited:141
  passing       70541   535/535   exit_group(bash) exited:0
  passing       70582   534/534   SIGCHLD code=1 addr=0x0
  passing       70583   534/534   exit_group(bash) exited:0
  passing       70585   359/359   SIGCHLD code=1 addr=0x0

open both in the desktop app: rewind open 2c347a1c2bcc5dbd 70544 --compare c50d1724631c3213
```

Schedules 0, 51 and 130 fail; the other 254 pass. Here the unperturbed
schedule happens to be the failing order. The test runs the pipeline three
times, so the narrowed pair parts at a later call, step 70544.

Perturbing only from step 33326, where the pipeline forks `printf`, moves
every schedule off the failing order:

```console
$ rewind check --run f1584358 --schedule-from 33326 --schedules 64 --all --no-narrow
schedule   0: exited:1      33576 steps    run f1584358a5e13e59
schedule   1: exited:0      84277 steps  a50a5ab6d992  run df96737be58721b5
schedule   2: exited:0      84078 steps  a50a5ab6d992  run 2077f6a462ff1a94
...
schedule  64: exited:0      84170 steps  a50a5ab6d992  run b357175957408f04
run f1584358a5e13e59 failed; 64 of 64 perturbed schedules ended differently
```

Nix 2.35.2 from nixpkgs has the same line. It passes schedule 0 and fails 6 of
1024 others:

```console
$ rewind check --all --schedules 1024 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-gc-closure'
...
schedule  56: exited:1      38326 steps    run ad4747353113f0a3
...
schedule 247: exited:1      78119 steps    run 123fbc351cd90351
...
schedule 374: exited:1      58491 steps    run 3232d0d7c38c2471
...
schedule 785: exited:1      78226 steps    run 8d9b2e1d58327204
...
schedule 801: exited:1      37936 steps    run 9a0622457d90b486
...
schedule 930: exited:1      77942 steps    run 2f337a4cd5ab9669
...
6 of 1024 perturbed schedules ended differently
```

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
$ rewind check --all 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-git-gc-closure-fixed'
schedule   0: exited:0      79839 steps  a50a5ab6d992  run 185bad3553dd45dd
schedule   1: exited:0      89344 steps  a50a5ab6d992  run f8e21cdc537a2573
...
0 of 64 perturbed schedules ended differently
same result under all 65 schedules
```

## The recording

Both files are in the
[case-studies release](https://github.com/fzakaria/rewindvm/releases/tag/case-studies):

- [nix-gc-closure-sigpipe.rwd](https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-gc-closure-sigpipe.rwd)
  (71.3 KB), the trace, for `rewind events`, `rewind log` and the app.
- [nix-gc-closure-sigpipe-replayable.rwd](https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-gc-closure-sigpipe-replayable.rwd)
  (535.5 MB), with the kernel, input image and keyframes, for `rewind replay`
  and `rewind shell` on an AMD Zen 2 or later.

```console
$ rewind import https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-gc-closure-sigpipe-replayable.rwd
$ rewind replay f1584358
$ rewind shell f1584358 <step>
$ rewind-app https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-gc-closure-sigpipe.rwd
```

The run ids above come from the derivations in the
[example flake](../../examples/case-studies/flake.nix), on nixpkgs
[b4fd65b1](https://github.com/NixOS/nixpkgs/commit/b4fd65b198c599cbe814fcb9f42d25d021595ec9).
Any change to a derivation gives a different set of runs.
