# Case study: a SIGPIPE in Nix's gc-closure test

<!-- let epoch = 1791936000 -->
<!-- let flake = github:fzakaria/rewindvm?dir=examples/case-studies -->

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

```console run name=first cut=170 keep=^schedule\s+\d+:\sexited:1\s|^(?!schedule\s+\d+:)
$ rewind check --epoch {{epoch}} '{{flake}}#nix-git-gc-closure'
```

<!-- capture schedule: schedule (\d+) ends differently from schedule 0 -->
<!-- capture run: failing: run (\w+), -->
<!-- capture deciding: step (\d+) decides it -->

Schedule {{schedule}} fails, and `check` narrows its perturbations to the one
step that decides it, {{deciding}}. The failing run's log has the test's last
lines:

```console run name=log cut=170 keep=^[+][(]gc-closure.sh:45[)]|^[+][+][(]gc-closure.sh:14[)]\snix|^[+]*[(]gc-closure.sh:1[45][)]\s(input2=|input2_out=|printf|head)|^1/1\smain\s-\s\S+\sFAIL
$ rewind log {{run|short}}
```

<!-- assert: grep -q 'gc-closure FAIL .*(exit status 141 or signal 13 SIGPIPE)' {{out:log}} -->
<!-- assert: grep -q '^+(gc-closure.sh:15) input2_out=/build/nix-test/main/gc-closure/store/[0-9a-z]*-dependencies-input-2$' {{out:log}} -->

```console run name=replay
$ rewind replay {{run|short}}
```

<!-- assert: grep -q '^identical to the recording: ' {{out:replay}} -->

`input2_out` got the right value, and the test still died of signal 13.

## What died

<!-- set sigpipe: rewind events {{run}} | grep -m1 SIGPIPE | awk '{print $1}' -->
<!-- set writer: rewind events {{run}} | grep -m1 SIGPIPE | awk '{print $2}' | cut -d/ -f1 -->
<!-- set fork_writer: rewind events {{run}} --to {{sigpipe}} | grep -E "fork\(\) = {{writer}}$" | tail -1 | awk '{print $1}' -->
<!-- set subst: rewind events {{run}} --to {{sigpipe}} | grep -E "fork\(\) = {{writer}}$" | tail -1 | awk '{print $2}' | cut -d/ -f1 -->
<!-- set head_exec: rewind events {{run}} --to {{sigpipe}} | grep 'execve(".*/bin/head", \["head", "-n1"\])' | tail -1 | awk '{print $1}' -->
<!-- set head: rewind events {{run}} --to {{sigpipe}} | grep 'execve(".*/bin/head", \["head", "-n1"\])' | tail -1 | awk '{print $2}' | cut -d/ -f1 -->
<!-- set head_exit: rewind events {{run}} --to {{sigpipe}} | grep -E " {{head}}/{{head}} +exit_group\(head\) exited:0$" | awk '{print $1}' -->
<!-- set events_from: echo $(( {{sigpipe}} - 75 )) -->
<!-- set events_to: echo $(( {{sigpipe}} + 25 )) -->
<!-- set ps_step: echo $(( {{head_exec}} + 6 )) -->

The events around the failure, and the processes just before it:

```console run name=events
$ rewind events {{run|short}} --from {{events_from}} --to {{events_to}}

$ rewind ps {{run|short}} {{ps_step}}
```

<!-- assert: grep -qE "fork\(\) = {{subst}}$" {{out:events}} && grep -qE " {{subst}}/{{subst}} +exit_group\(bash\) exited:141$" {{out:events}} && grep -qE " {{writer}}/{{writer}} +exit_group\(bash\) killed:SIGPIPE$" {{out:events}} -->
<!-- assert: grep -qE "^ +{{writer}} +/nix/store/.*bash -x -e -u -o pipefail gc-closure.sh \(fork\)$" {{out:events}} && grep -qE "^ +{{head}} +head -n1$" {{out:events}} -->

{{subst}} is the command substitution, {{writer}} runs `printf` and {{head}}
runs `head`. `head` exits 0, then {{writer}} dies of SIGPIPE and the test
exits 141.

## Count the writes

<!-- set before: rewind threads {{run}} --from {{fork_writer}} --to {{head_exit}} | awk '$3 == "{{writer}}/{{writer}}" {print $1}' | sed -n 2p | awk '{print $1 - 1}' -->
<!-- set wchar_before: rewind cat {{run}} {{before}} /proc/{{writer}}/io | awk '/^wchar/ {print $2}' -->
<!-- set wchar_after: rewind cat {{run}} {{head_exit}} /proc/{{writer}}/io | awk '/^wchar/ {print $2}' -->
<!-- set first_line: echo $(( {{wchar_after}} - {{wchar_before}} )) -->

`rewind cat` reads a file in a fork of the run at any step. `/proc/<pid>/io`
counts each process's writes:

```console run name=writes
$ for step in {{before}} {{head_exit}} {{sigpipe}}; do echo "step $step"; rewind cat {{run|short}} $step /proc/{{writer}}/io | grep -E '^(wchar|syscw)' | sed 's/^/  {{writer}} /'; rewind cat {{run|short}} $step /proc/{{head}}/io | grep -E '^(wchar|syscw)' | sed 's/^/  {{head}} /'; done
```

<!-- assert: test "$(grep -A4 '^step {{before}}$' {{out:writes}} | grep syscw | tr -s ' ' | cut -d' ' -f4 | tr '\n' ' ')" = '1 1 ' -->
<!-- assert: test "$(grep -A4 '^step {{head_exit}}$' {{out:writes}} | grep syscw | tr -s ' ' | cut -d' ' -f4 | tr '\n' ' ')" = '2 2 ' -->
<!-- assert: grep -A3 '^step {{sigpipe}}$' {{out:writes}} | grep -q '  {{writer}} syscw: 3' && grep -A3 '^step {{sigpipe}}$' {{out:writes}} | grep -q '  {{writer}} wchar: {{wchar_after}}' && grep -q 'rewind: /proc/{{head}}/io: no such file at this step' {{out:writes}} -->

The first write of each is its `bash -x` trace line. By {{head_exit}} `printf`
has written the first store path ({{first_line}} bytes) and `head` has echoed
it and exited. At {{sigpipe}} `printf`'s third write adds no bytes: the pipe
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

<!-- assert: test {{first_line}} = 92 -->

The buffering is set in `shell_initialize` in bash 5.3's
[shell.c](https://git.savannah.gnu.org/cgit/bash.git/tree/shell.c?h=bash-5.3).
`head -n1` exits after the first line. If it does so between the two writes,
the second write raises SIGPIPE, `pipefail` makes the pipeline's status 141,
and `-e` ends the test.

## Who had the CPU

<!-- set threads_from: echo $(( {{head_exec}} - 2 )) -->
<!-- set threads_to: echo $(( {{sigpipe}} + 5 )) -->

`rewind threads` prints which process held the CPU through each stretch of
steps:

```console run name=threads
$ rewind threads {{run|short}} --from {{threads_from}} --to {{threads_to}}
```

<!-- set head_blocks: awk '$3 == "{{head}}/{{head}}" && $4 == "head" {print $2}' {{out:threads}} | sed -n 1p -->
<!-- set head_runs: awk '$3 == "{{head}}/{{head}}" && $4 == "head" {print $1}' {{out:threads}} | sed -n 2p -->
<!-- set printf_writes: awk '$3 == "{{writer}}/{{writer}}" {print $1}' {{out:threads}} | sed -n 1p -->
<!-- set printf_back: awk '$3 == "{{writer}}/{{writer}}" {print $1}' {{out:threads}} | sed -n 2p -->
<!-- assert: test {{printf_writes}} -eq $(( {{head_blocks}} + 1 )) && test {{head_runs}} -gt {{printf_writes}} && test {{head_exit}} -lt {{printf_back}} && test {{printf_back}} -eq $(( {{sigpipe}} - 1 )) -->
<!-- assert: test $(awk '$3 == "{{writer}}/{{writer}}"' {{out:threads}} | wc -l) -eq 2 -->

`head` blocks in `read` by {{head_blocks}}. `printf` makes its first write at
{{printf_writes}}, which wakes `head`. `head` runs at {{head_runs}} and exits,
and `printf` only gets the CPU back at {{printf_back}}; its next write, at
{{sigpipe}}, raises the SIGPIPE.

## How often it fails

```console run name=check keep=^schedule\s+[01]:|^schedule\s+\d+:\sexited:1\s|^(?!schedule\s+\d+:)
$ rewind check --epoch {{epoch}} --all --schedules 256 '{{flake}}#nix-git-gc-closure'
```

<!-- assert: grep -qE '^schedule +0: exited:0 ' {{out:check}} && grep -qE '^schedule +{{schedule}}: exited:1 ' {{out:check}} -->
<!-- set fails: grep -E '^schedule +[0-9]+: exited:1 ' {{out:check}} | awk '{print $2}' | tr -d : | tr '\n' ' ' -->
<!-- set nfails: grep -cE '^schedule +[0-9]+: exited:1 ' {{out:check}} -->
<!-- set npass: grep -cE '^schedule +[0-9]+: exited:0 ' {{out:check}} -->
<!-- assert: test $(( {{nfails}} + {{npass}} )) -eq 257 -->

{{nfails}} of the 257 schedules fail, {{fails|and}}; the other {{npass}}
pass, the unperturbed one among them.

```console run name=fork keep=^schedule\s+(0|1|2|64):|^schedule\s+\d+:\sexited:1\s|^run\s
$ rewind check --run {{run|short}} --schedule-from {{fork_writer}} --schedules 64 --all --no-narrow
```

<!-- capture fork_left: failed; (\d+) of 64 other schedules ended differently -->
<!-- set fork_fails: grep -E '^schedule +[0-9]+: exited:1 ' {{out:fork}} | awk '{print $2}' | tr -d : | grep -vx 0 | head -1 -->
<!-- set fork_fail_run: grep -E '^schedule +{{fork_fails}}: exited:1 ' {{out:fork}} | awk '{print $NF}' -->
<!-- set fork_fail_pid: rewind events {{fork_fail_run}} | grep -m1 SIGPIPE | awk '{print $2}' | cut -d/ -f1 -->
<!-- assert: test {{fork_left}} -gt 0 && test -n "{{fork_fail_pid}}" -->

Perturbing only from step {{fork_writer}}, where the pipeline forks `printf`,
moves {{fork_left}} of 64 schedules off the failing order. Schedule
{{fork_fails}} fails the same way: `head` exits between `printf`'s two
writes, and `printf`, process {{fork_fail_pid}}, dies of SIGPIPE.

```console run name=released keep=^rewind:|^schedule\s+\d+:\sexited:1\s|\sof\s1024\sother
$ rewind check --epoch {{epoch}} --all --schedules 1024 '{{flake}}#nix-gc-closure'
```

<!-- set released_fails: grep -cE '^schedule +[0-9]+: exited:1 ' {{out:released}} -->
<!-- assert: test {{released_fails}} -gt 0 -->
<!-- assert: for r in $(grep -E '^schedule +[0-9]+: exited:1 ' {{out:released}} | awk '{print $NF}'); do rewind events $r | grep -q SIGPIPE || exit 1; done -->

Nix 2.35.2 from nixpkgs has the same line, and {{released_fails}} of its
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

```console run name=fixed keep=^schedule\s+[01]:|^0\sof\s64|^same\sresult
$ rewind check --epoch {{epoch}} --all '{{flake}}#nix-git-gc-closure-fixed'
```

<!-- assert: grep -q '^same result under all 65 schedules$' {{out:fixed}} && grep -qE '^schedule +0: exited:0 ' {{out:fixed}} -->

## The recording

<!-- set trace_size: rewind export {{run}} -o {{work}}/nix-gc-closure-sigpipe.rwd 2>&1 | grep -o '([^)]*)' | tr -d '()' -->
<!-- set replayable_size: rewind export {{run}} --replayable -o {{work}}/nix-gc-closure-sigpipe-replayable.rwd 2>&1 | grep -o '([^)]*)' | tr -d '()' -->
<!-- run: cp {{work}}/nix-gc-closure-sigpipe.rwd {{site}}/viewer/examples/nix-gc-closure.rwd -->

Both files are in the
[case-studies release](https://github.com/fzakaria/rewindvm/releases/tag/case-studies):

- [nix-gc-closure-sigpipe.rwd](https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-gc-closure-sigpipe.rwd)
  ({{trace_size}}), the trace, for `rewind events`, `rewind log` and the app; the
  [viewer](https://rewindvm.dev/viewer/?url=examples/nix-gc-closure.rwd) opens it in a browser.
- [nix-gc-closure-sigpipe-replayable.rwd](https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-gc-closure-sigpipe-replayable.rwd)
  ({{replayable_size}}), with the kernel, input image and keyframes, for `rewind replay`
  and `rewind shell` on an AMD Zen 2 or later.

```console
$ rewind import https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-gc-closure-sigpipe-replayable.rwd
$ rewind replay {{run|short}}
$ rewind shell {{run|short}} <step>
$ rewind-app https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-gc-closure-sigpipe.rwd
```

The run ids above come from the derivations in the
[example flake](../../examples/case-studies/flake.nix), on nixpkgs
[b4fd65b1](https://github.com/NixOS/nixpkgs/commit/b4fd65b198c599cbe814fcb9f42d25d021595ec9),
at the epoch the commands pass. Any change to a derivation gives a different
set of runs.
