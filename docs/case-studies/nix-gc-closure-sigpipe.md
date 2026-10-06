# Case study: a SIGPIPE in Nix's gc-closure test

Nix's functional test `gc-closure.sh` can fail with exit status 141 when the
machine schedules two processes in one particular order. Rewind VM hit that
order on the first run of the test on its own; the host never did in 200 runs
of the test. The cause is a pipe into `head -n1` under `set -o pipefail`, and a
detail of bash: it line-buffers its own standard output, so a two-line
`printf` is two `write` calls. As of 2026-10-01 nothing about this is reported
in the [Nix issue tracker](https://github.com/NixOS/nix/issues).

## The software

The test came in with
[NixOS/nix#15727](https://github.com/NixOS/nix/pull/15727), merged on
2026-05-02, and checks `nix store delete --recursive`. It is in Nix 2.35.0
through 2.35.2 and on master. Lines 14 to 16 of
[tests/functional/gc-closure.sh](https://github.com/NixOS/nix/blob/203f85b2e851fc52e253e8e33eff5fb92936736a/tests/functional/gc-closure.sh#L14-L16)
split the two outputs of a derivation:

```bash
    input2=$(nix build -f dependencies2.nix input2_drv --no-link --print-out-paths)
    input2_out=$(printf "%s" "$input2" | head -n1)
    input2_out2=$(printf "%s" "$input2" | tail -n1)
```

Meson runs every functional test as `bash -x -e -u -o pipefail <script>`, so a
pipeline fails if any command in it fails, and a failed assignment ends the
test.

## How Rewind found it

The derivation is nixpkgs' `nixVersions.nixComponents_git.nix-functional-tests`
(Nix master at 203f85b2, packaged as 2.36pre20260912) with its check phase cut
down to the tests named. In a run of the whole suite in the VM, d6e9b3e5,
`gc-closure` passed: other tests ran before it, so its processes met a
different schedule. (Two other tests failed in that run because the
derivation skips building a plugin and a test program they need.) On its own,
the test fails on its first run, schedule 0:

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
rewind: run b34ebac24f05c690 exited:1 after 33640 steps, 1.417s virtual, 5.359s wall (poweroff)

$ rewind replay b34ebac2
identical: 6180 events over 33640 steps
```

The test calls `nix_gc_closure` three times, and line 15 runs in each. In the
first call, line 15 got the right value,
`input2_out` is set, and the test still died with signal 13. The events around
the failure:

```console
$ rewind events b34ebac2 --from 33376 --to 33476
     33386   359/359   fork() = 416
     33389   416/416   fork() = 417
     33392   416/416   fork() = 418
     33422   418/418   execve("/nix/store/2gfxiwls9hbgwdwcy43mprchwsq36mg6-coreutils-9.11/bin/head", ["head", "-n1"])
     33445   418/418   exit_group(head) exited:0
     33451   417/417   SIGPIPE code=0 addr=0x0
     33452   417/417   exit_group(bash) killed:SIGPIPE
     33454   416/416   SIGCHLD code=1 addr=0x0
     33455   416/416   exit_group(bash) exited:141
     33458   359/359   SIGCHLD code=1 addr=0x0
     33471   359/359   exit_group(bash) exited:141

$ rewind ps b34ebac2 33414
...
   359       /nix/store/10dxp0qxqxxsyiljrh2kp0xqhz6arhcx-bash-5.3p15/bin/bash -x -e -u -o pipefail gc-closure.sh
   416         /nix/store/10dxp0qxqxxsyiljrh2kp0xqhz6arhcx-bash-5.3p15/bin/bash -x -e -u -o pipefail gc-closure.sh (fork)
   417           /nix/store/10dxp0qxqxxsyiljrh2kp0xqhz6arhcx-bash-5.3p15/bin/bash -x -e -u -o pipefail gc-closure.sh (fork)
   418           head -n1
```

Process 416 is the command substitution, 417 the subshell that runs `printf`
and 418 the one that becomes `head`. `head` exits successfully, then 417 dies
of SIGPIPE, the pipeline's status is 141, and the test script (359) exits 141.

The trace does not record writes into pipes, but the kernel counts them.
`rewind cat` reads `/proc/<pid>/io` in a fork of the run at any step:

```console
$ for step in 33441 33445 33451; do
>   echo "step $step"
>   rewind cat b34ebac2 $step /proc/417/io | grep -E '^(wchar|syscw)' | sed 's/^/  417 /'
>   rewind cat b34ebac2 $step /proc/418/io | grep -E '^(wchar|syscw)' | sed 's/^/  418 /'
> done
step 33441
  417 wchar: 224
  417 syscw: 1
  418 wchar: 30
  418 syscw: 1
step 33445
  417 wchar: 316
  417 syscw: 2
  418 wchar: 122
  418 syscw: 2
step 33451
  417 wchar: 316
  417 syscw: 3
rewind: /proc/418/io: no such file at this step
```

At step 33441 each subshell has written only its `bash -x` trace line to
standard error (224 and 30 bytes). By step 33445 the `printf` subshell has
written 92 more bytes, the first store path and its newline, and `head` has
read them and written the same 92 bytes. `head` exits at step 33445, and the
third write from 417, the second line, fails: by step 33451 `syscw` has gone
to 3 while `wchar` stays at 316. Writing to a pipe with no reader raises
SIGPIPE.

## Root cause

`printf "%s" "$input2"` writes its output in two `write` calls, one per line.
On the host, with the same bash 5.3.15 and the two store paths from the run:

```console
$ export X=$'/build/nix-test/main/gc-closure/store/ij8qwx0m6dvpmggqmq8zmhbaiw6ym8pg-dependencies-input-2\n/build/nix-test/main/gc-closure/store/hw6dm6rpynd7hmhl1mnp0z4m28lfbx4b-dependencies-input-2-out2'
$ strace -f -e trace=write -o /dev/stdout bash -c 'set -o pipefail; y=$(printf "%s" "$X" | cat)' | grep write
153665 write(1, "/build/nix-test/main/gc-closure/"..., 92) = 92
153665 write(1, "/build/nix-test/main/gc-closure/"..., 96) = 96
```

That is because bash makes its own standard output line-buffered when it
starts, in `shell_initialize` in bash 5.3's
[shell.c](https://git.savannah.gnu.org/cgit/bash.git/tree/shell.c?h=bash-5.3):

```c
  /* Line buffer output for stderr and stdout. */
  if (shell_initialized == 0)
    {
      sh_setlinebuf (stderr);
      sh_setlinebuf (stdout);
    }
```

`head -n1` stops reading after the first newline and exits. If it does so
between the two writes, the second write gets EPIPE and the subshell is killed
by SIGPIPE. Without `pipefail` the pipeline's status would be `head`'s, 0, and
nobody would notice. With it, the status is 141, and `-e` ends the test.

The window is the time between the two writes, a few microseconds. `head`
has to be blocked in `read` already, be woken by the first write, and get the
CPU and run to `exit` before the writer gets it back. On one CPU the scheduler
is free to run the woken reader first, and in this run the VM's scheduler did.

## How often it fails

Under Rewind, on the build of Nix master:

```console
$ rewind check --all --schedules 256 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-git-gc-closure'
schedule   0: exited:1      33640 steps    run b34ebac24f05c690
schedule   1: exited:0      89507 steps  a50a5ab6d992  run cd500b3a6372553f
...
schedule 0 failed; 256 of 256 perturbed schedules ended differently

schedule 1 passes where schedule 0 fails; narrowing the steps it perturbs
perturbing only steps 31713..36075 still ends differently
step 36074 decides it: a reschedule there makes the run pass

passing: run da7e3e7b2369f89c, schedule 1 over steps 31713..36075
failing: run eba86a81a195ddc6, schedule 1 over steps 31713..36074
the two are the same run until step 36074

where /nix/store/10dxp0qxqxxsyiljrh2kp0xqhz6arhcx-bash-5.3p15/bin/bash -x -e -u -o pipefail gc-closure.sh first behaves differently:
  both          70820   359/359   fork() = 534
  both          70823   534/534   fork() = 535
  both          70826   534/534   fork() = 536
  failing       70885   535/535   SIGPIPE code=0 addr=0x0
  failing       70886   535/535   exit_group(bash) killed:SIGPIPE
  failing       70888   534/534   SIGCHLD code=1 addr=0x0
  failing       70889   534/534   exit_group(bash) exited:141
  passing       70859   535/535   exit_group(bash) exited:0
  passing       70890   534/534   SIGCHLD code=1 addr=0x0
  passing       70891   534/534   exit_group(bash) exited:0
  passing       70893   359/359   SIGCHLD code=1 addr=0x0

open both in the desktop app: rewind open eba86a81a195ddc6 70885 --compare da7e3e7b2369f89c
```

Only schedule 0 fails. All 256 perturbed schedules pass: the unperturbed
schedule is the failing order here, and every perturbation the check tried
moved the run off it. Schedule 1 passes, and narrowing it finds that
perturbing steps 31713..36075 is enough to make the pipeline pass. That window
opens 1673 steps before schedule 0's pipeline first forks, while the `nix build`
of line 14 (process 408) is still running. In the passing run the `printf`
subshell writes both lines and exits 0 at step 33551, before 418 has become
`head`.

The script calls `nix_gc_closure` three times, so the pipeline runs again
later. Without the reschedule at step 36074, the window's last step, the same
schedule gets through the first call and fails a later one with the same
SIGPIPE, at step 70885. That run and the passing one are the same until step
36074, so they are the pair `check` reports, and they part at the later
call's pipeline.

The same test from nixpkgs' Nix 2.35.2, which has the same line, passed
schedule 0 and the first 256 perturbed schedules, the ones the check above
tries. Under 1024 perturbed schedules it failed in two, schedules 829 and 938,
each with SIGPIPE:

```console
$ rewind check --all --schedules 1024 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-gc-closure'
...
schedule 829: exited:1      58704 steps    run 17d2961bb4852861
...
schedule 938: exited:1      37996 steps    run 3ba52f9fc50ffb4a
...
2 of 1024 perturbed schedules ended differently
```

On the host the test has not failed:

| What ran on the host                                                             | Failed       |
| -------------------------------------------------------------------------------- | ------------ |
| `gc-closure` from Nix master, 200 times in one build (line 15 runs 3 times each) | 0 of 200     |
| line 15 in a loop, 16 CPUs                                                       | 0 of 5000    |
| line 15 in a loop, pinned to one CPU with `taskset -c 3`                         | 0 of 5000    |
| the same, with a busy loop sharing that CPU                                      | 0 of 20000   |
| the same, with the `printf` subshell reniced to 19 first, pinned to one CPU      | 1802 of 2000 |
| the same, reniced, not pinned                                                    | 1 of 2000    |

The reniced rows confirm the window exists on real hardware: when the writer
is the low priority task on its CPU, the woken `head` runs first and the
pipeline fails nine times in ten. A writer descheduled between two writes is
what a loaded CI machine does to some process now and then, which makes this
a rare flake there and a near impossible one on an idle machine.

## The fix

Give `head` its input without a pipe, so there is no writer to kill:

```diff
-    input2_out=$(printf "%s" "$input2" | head -n1)
+    input2_out=$(head -n1 <<< "$input2")
```

`input2_out=${input2%%$'\n'*}` does the same without a process. Line 16
needs no change: `tail` reads to the end, so its writer never sees a closed
pipe.

With the change applied in the derivation's `postPatch`, all 65 schedules
pass, including schedule 0:

```console
$ rewind check --all 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-git-gc-closure-fixed'
schedule   0: exited:0      79733 steps  a50a5ab6d992  run f5d0c0e4fe8946cc
schedule   1: exited:0      89241 steps  a50a5ab6d992  run 6e68b2180ef1ab5a
...
0 of 64 perturbed schedules ended differently
same result under all 65 schedules
```

The same pattern appears twice more in Nix's functional tests,
`find ... | head -n1` in `binary-cache.sh` and `sed ... | head -1` in
`check.sh`. Both writers are stdio programs writing to a pipe, which buffer
fully and write once at exit for output this small, so they are not exposed in
the same way.

## The recording

Both files are in the
[case-studies release](https://github.com/fzakaria/rewindvm/releases/tag/case-studies). `rewind import` and the
desktop app (`rewind-app`) take either one, by path or URL, and unpack it as
it downloads:

- [nix-gc-closure-sigpipe.rwd](https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-gc-closure-sigpipe.rwd)
  (71.4 KB) is the trace alone, enough for `rewind events`, `rewind log` and
  the app.
- [nix-gc-closure-sigpipe-replayable.rwd](https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-gc-closure-sigpipe-replayable.rwd)
  (530.9 MB) adds the kernel, the input image and the keyframes, so
  another AMD machine from Zen 2 on can `rewind replay` and `rewind shell` it.

```console
$ rewind import https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-gc-closure-sigpipe-replayable.rwd
$ rewind replay b34ebac2
$ rewind shell b34ebac2 <step>
$ rewind-app https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-gc-closure-sigpipe.rwd
```

How they were made:

```console
$ rewind export b34ebac2 --replayable -o nix-gc-closure-sigpipe-replayable.rwd
rewind: wrote nix-gc-closure-sigpipe-replayable.rwd (530.9 MB)
$ rewind export b34ebac2 -o nix-gc-closure-sigpipe.rwd
rewind: wrote nix-gc-closure-sigpipe.rwd (71.4 KB)
```

Imported into an empty `REWIND_HOME`, the replayable export replays
identically from boot and from a keyframe:

```console
$ rewind import nix-gc-closure-sigpipe-replayable.rwd
b34ebac24f05c690  exited:1         33640 steps  nix-functional-gc-closure-2.36pre20260912_203f85b2
$ rewind replay b34ebac2
identical: 6180 events over 33640 steps
$ rewind replay b34ebac2 --from 33000
identical from the keyframe at step 29162 to the end (1.64s)
```

## Reproducing it

The derivation is nixpkgs' functional test package with the check phase
narrowed to one test. With `pkgs` from nixpkgs b4fd65b1, the attribute
`nix-git-gc-closure` in the example flake is in essence:

```nix
pkgs.nixVersions.nixComponents_git.nix-functional-tests.overrideAttrs (old: {
  dontBuild = true;
  checkPhase = ''
    runHook preCheck
    meson test --no-rebuild --print-errorlogs gc-closure
    runHook postCheck
  '';
})
```

The run ids above come from the exact derivation in the flake, which has a
few more attributes than this sketch. Any change to the derivation, or the boot
date, is a change of inputs and so a different set of runs: one that differs
may pass in schedule 0 and fail in others, and
`rewind check --all --schedules 256` is the way to find them.
