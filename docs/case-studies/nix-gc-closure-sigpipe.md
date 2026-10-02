# Case study: a SIGPIPE in Nix's gc-closure test

Nix's functional test `gc-closure.sh` can fail with exit status 141 when the
machine schedules two processes in one particular order. Rewind VM hit that
order on the first run of the test suite; the host never did in 200 runs of
the test. The cause is a pipe into `head -n1` under `set -o pipefail`, and a
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
down to the tests named. The first run of the whole suite in the VM failed in
`gc-closure`. (Two other tests failed in that run because the derivation
skipped building a plugin and a test program they need.) On its own:

```console
$ rewind nix 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-git-gc-closure'
rewind: packing 217 store paths for nix-functional-gc-closure-2.36pre20260912_203f85b2
...
+(gc-closure.sh:47) nix_gc_closure false --also-referrers
...
++(gc-closure.sh:14) nix build -f dependencies2.nix input2_drv --no-link --print-out-paths
...
+(gc-closure.sh:14) input2=$'/build/nix-test/main/gc-closure/store/ij8qwx0m6dvpmggqmq8zmhbaiw6ym8pg-dependencies-input-2\n/build/nix-test/main/gc-closure/store/hw6dm6...
++(gc-closure.sh:15) printf %s $'/build/nix-test/main/gc-closure/store/ij8qwx0m6dvpmggqmq8zmhbaiw6ym8pg-dependencies-input-2\n/build/nix-test/main/gc-closure/store/hw...
++(gc-closure.sh:15) head -n1
+(gc-closure.sh:15) input2_out=/build/nix-test/main/gc-closure/store/ij8qwx0m6dvpmggqmq8zmhbaiw6ym8pg-dependencies-input-2
...
1/1 main - nix-functional-tests:gc-closure FAIL             0.92s   (exit status 141 or signal 13 SIGPIPE)
...
rewind: run 7ef07d3f2865a36e exited:1 after 70483 steps, 2.074s virtual, 6.148s wall (poweroff)

$ rewind replay 7ef07d3f
identical: 6893 events over 70483 steps
```

The test calls `nix_gc_closure` three times, and line 15 runs in each. The
first two calls passed; in the third, line 15 got the right value,
`input2_out` is set, and the test still died with signal 13. The events around
the failure:

```console
$ rewind events 7ef07d3f --from 70259 --to 70365
     70269   359/359   fork() = 534
     70272   534/534   fork() = 535
     70275   534/534   fork() = 536
     70305   536/536   execve("/nix/store/2gfxiwls9hbgwdwcy43mprchwsq36mg6-coreutils-9.11/bin/head", ["head", "-n1"])
     70328   536/536   exit_group(head) exited:0
     70334   535/535   SIGPIPE code=0 addr=0x0
     70335   535/535   exit_group(bash) killed:SIGPIPE
     70337   534/534   SIGCHLD code=1 addr=0x0
     70338   534/534   exit_group(bash) exited:141
     70341   359/359   SIGCHLD code=1 addr=0x0
     70362   359/359   exit_group(bash) exited:141

$ rewind ps 7ef07d3f --at 70299
...
   359       /nix/store/10dxp0qxqxxsyiljrh2kp0xqhz6arhcx-bash-5.3p15/bin/bash -x -e -u -o pipefail gc-closure.sh
   534         /nix/store/10dxp0qxqxxsyiljrh2kp0xqhz6arhcx-bash-5.3p15/bin/bash -x -e -u -o pipefail gc-closure.sh (fork)
   535           /nix/store/10dxp0qxqxxsyiljrh2kp0xqhz6arhcx-bash-5.3p15/bin/bash -x -e -u -o pipefail gc-closure.sh (fork)
   536           head -n1
```

Process 534 is the command substitution, 535 the subshell that runs `printf`
and 536 the one that becomes `head`. `head` exits successfully, then 535 dies
of SIGPIPE, the pipeline's status is 141, and the test script (359) exits 141.

The trace does not record writes into pipes, but the kernel counts them.
`rewind cat` reads `/proc/<pid>/io` in a fork of the run at any step:

```console
$ for step in 70324 70327 70333; do
>   echo "step $step"
>   rewind cat 7ef07d3f $step /proc/535/io | grep -E '^(wchar|syscw)' | sed 's/^/  535 /'
>   rewind cat 7ef07d3f $step /proc/536/io | grep -E '^(wchar|syscw)' | sed 's/^/  536 /'
> done
step 70324
  535 wchar: 224
  535 syscw: 1
  536 wchar: 30
  536 syscw: 1
step 70327
  535 wchar: 316
  535 syscw: 2
  536 wchar: 122
  536 syscw: 2
step 70333
  535 wchar: 316
  535 syscw: 3
rewind: /proc/536/io: no such file at this step
```

At step 70324 each subshell has written only its `bash -x` trace line to
standard error (224 and 30 bytes). By step 70327 the `printf` subshell has
written 92 more bytes, the first store path and its newline, and `head` has
read them and written the same 92 bytes. `head` exits at step 70328, and the
third write from 535, the second line, fails: `syscw` goes to 3 while `wchar`
stays at 316. Writing to a pipe with no reader raises SIGPIPE.

## Root cause

`printf "%s" "$input2"` writes its output in two `write` calls, one per line.
On the host, with the same bash 5.3.15 and the two store paths from the run:

```console
$ export X=$'/build/nix-test/main/gc-closure/store/ij8qwx0m6dvpmggqmq8zmhbaiw6ym8pg-dependencies-input-2\n/build/nix-test/main/gc-closure/store/hw6dm6rpynd7hmhl1mnp0z4m28lfbx4b-dependencies-input-2-out2'
$ strace -f -e trace=write -o /dev/stdout bash -c 'set -o pipefail; y=$(printf "%s" "$X" | cat)' | grep write
3948546 write(1, "/build/nix-test/main/gc-closure/"..., 92) = 92
3948546 write(1, "/build/nix-test/main/gc-closure/"..., 96) = 96
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
schedule   0: exited:1            70483 steps    run 7ef07d3f2865a36e
schedule   1: exited:0            89238 steps  d5ede538f628  run 3d0626e492ff5609
...
schedule 0 failed; 256 of 256 perturbed schedules ended differently

schedule 1 passes where schedule 0 fails; narrowing the steps it perturbs
perturbing only steps 70324..70325 still ends differently

passing: run 60d84600e110130d
failing: run 7ef07d3f2865a36e

where /nix/store/10dxp0qxqxxsyiljrh2kp0xqhz6arhcx-bash-5.3p15/bin/bash -x -e -u -o pipefail gc-closure.sh first behaves differently:
  both       70269   359/359   fork() = 534
  both       70272   534/534   fork() = 535
  both       70275   534/534   fork() = 536
  left       70327   535/535   exit_group(bash) exited:0
  left       70334   534/534   SIGCHLD code=1 addr=0x0
  left       70335   534/534   exit_group(bash) exited:0
  left       70338   359/359   SIGCHLD code=1 addr=0x0
  right      70334   535/535   SIGPIPE code=0 addr=0x0
  right      70335   535/535   exit_group(bash) killed:SIGPIPE
  right      70337   534/534   SIGCHLD code=1 addr=0x0
  right      70338   534/534   exit_group(bash) exited:141
```

One of the 257 runs fails, schedule 0, at line 15 with SIGPIPE. Schedule 1
passes, and narrowing it finds that perturbing two steps, 70324 and 70325, the steps between `printf`'s first write and `head`'s exit, is enough
to make the pipeline pass. In the passing run the `printf` subshell gets the
CPU back for its second write before `head` exits, and exits 0 at step 70327.

The same test from nixpkgs' Nix 2.35.2, which has the same line, passed all
257 schedules of the same check. Under 1024 perturbed schedules it failed in
one, schedule 722, also with SIGPIPE at line 15:

```console
$ rewind check --all --schedules 1024 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-gc-closure'
...
schedule 722: exited:1            37777 steps    run 4c970e274b3a9c26
...
1 of 1024 perturbed schedules ended differently
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
schedule   0: exited:0            79588 steps  d5ede538f628  run 3b519ca44aae4ad9
schedule   1: exited:0            89126 steps  d5ede538f628  run db8731945a279ed2
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
[case-studies release](https://github.com/fzakaria/rewindvm/releases/tag/case-studies):
[nix-gc-closure-sigpipe-replayable.rwd](https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-gc-closure-sigpipe-replayable.rwd), with
everything needed to replay the failure on another AMD machine from Zen 2 on,
and [nix-gc-closure-sigpipe.rwd](https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-gc-closure-sigpipe.rwd), the trace alone, which the
desktop app opens. `rewind import` and the app both take the URL, and
unpack the file as it downloads:

```console
$ rewind import https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-gc-closure-sigpipe-replayable.rwd
$ rewind replay 7ef07d3f
$ rewind shell 7ef07d3f <step>
$ rewind-app https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-gc-closure-sigpipe.rwd
```

How they were made:

```console
$ rewind export 7ef07d3f --replayable -o nix-gc-closure-sigpipe-replayable.rwd
rewind: wrote nix-gc-closure-sigpipe-replayable.rwd (540.1 MB)
$ rewind export 7ef07d3f -o nix-gc-closure-sigpipe.rwd
rewind: wrote nix-gc-closure-sigpipe.rwd (77.2 KB)
```

The replayable file holds the input image, the kernel and the keyframes, so
another AMD machine from Zen 2 on can import it and replay the failure; the
view-only file holds the trace and opens in the app. Imported into an empty
`REWIND_HOME`, the replayable export replays identically from boot and from a
keyframe:

```console
$ rewind import nix-gc-closure-sigpipe-replayable.rwd
7ef07d3f2865a36e  exited:1         70483 steps  nix-functional-gc-closure-2.36pre20260912_203f85b2
$ rewind replay 7ef07d3f
identical: 6893 events over 70483 steps
$ rewind replay 7ef07d3f --from 70000
identical from the keyframe at step 67853 to the end (1.12s)
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
