# Tutorial: property-based testing

<!-- let epoch = 1790985600 -->
<!-- let repo = github:fzakaria/rewindvm -->

By default `rewind check` judges a run by how it ends. It runs the job
under many thread schedules and calls a schedule failing when it ends
differently from schedule 0, the run with nothing perturbed: another exit
status, a signal, or an output with another hash. A test that exits 0 on
schedule 0 and 1 on schedule 5 is the bug the [Nix tutorial](tutorial-nix.md)
finds.

You can also state properties that every run must have, and check judges
each schedule by them. A property can be one of Rewind's built-in checks on
the files and processes a run touched, an assertion the program makes about
itself with the Rewind SDK, or a script of your own. Check can also vary the
program's randomness with seeds, as well as its schedule.

The examples are three small programs from
[examples](https://github.com/fzakaria/rewindvm/tree/main/examples):
`config-reload`, `bank` and `hash-order`. Install Rewind as the
[Nix tutorial](tutorial-nix.md) does.

## Catch a bug on the first run

`config-reload` rewrites its config file in place: it truncates the file,
then writes the two lines one at a time. Another process rereads the file
meanwhile. The program's own test fails only when a read lands between the
truncate and the last write, which few schedules do.

The `atomic-replace` property fails a run that truncates a file in place,
where it should write a new file and rename it over the old one. The
property looks at what the program did, not at how the run ended, so
schedule 0 already violates it and check finds the bug with no search:

```console run name=atomic show=:3,-3:
$ rewind check --epoch {{epoch}} --schedules 8 --property 'atomic-replace=*.conf' {{repo}}#config-reload
```

<!-- assert: grep -qE '^  atomic-replace: step [0-9]+: process [0-9]+ \(config\) truncated /build/config-reload/app.conf in place' {{out:atomic}} && grep -q '^every schedule violated a property, as schedule 0 did$' {{out:atomic}} -->

`atomic-replace` flags the unsafe write whether or not a reader saw the
half-written file. `no-read-while-written` flags the reader: it fails a run
in which a process opens the file for reading while another process has it
open for writing. Only some schedules do that, so check searches for one and
narrows it to the step that decides it:

```console run name=read
$ rewind check --epoch {{epoch}} --property 'no-read-while-written=*.conf' {{repo}}#config-reload | grep -E 'decides it|failing run'
```

<!-- assert: grep -qE 'failing run violates no-read-while-written: step [0-9]+: process [0-9]+ \(config\) opened /build/config-reload/app.conf for reading while process [0-9]+ \(config\)' {{out:read}} -->

`--property` replaces the default, and you can repeat it to judge runs by
several properties. Rewind includes these:

| Property                     | A run violates it when                                                 |
| ---------------------------- | ---------------------------------------------------------------------- |
| `same-outcome`               | it ends differently from schedule 0; the default                       |
| `exit-zero`                  | the job does not exit 0 with every output made                         |
| `no-crash`                   | a process dies of a signal such as SIGSEGV, even if a retry hid it     |
| `no-orphans`                 | a process the job started is still running when the job exits          |
| `single-writer=GLOB`         | two processes have a matching file open for writing at once            |
| `atomic-replace=GLOB`        | a matching file the run wrote is truncated or recreated in place       |
| `no-read-while-written=GLOB` | a matching file is opened for reading while another process writes it  |
| `writes-within=DIR:DIR`      | the job writes outside these directories                               |
| `sdk`                        | an assertion made with the Rewind SDK fails                            |
| `junit=GLOB`                 | a test that passed on schedule 0 fails, by the job's JUnit XML reports |
| `script=PATH`                | a program of yours exits 1                                             |

`rewind check --help` describes each in full.

## Assert from inside the program

`bank` has two threads deposit into one balance without a lock, so one
deposit can overwrite another. It includes `rewind.h`, the C header of the
Rewind SDK, and makes two assertions:

```c
/* Another teller is mid-deposit: the case a lock around the read and
 * store has to handle, and the only way to lose a deposit without one. */
int others = atomic_fetch_add(&depositing, 1);
REWIND_SOMETIMES(others > 0, "deposits-concurrent");
...
REWIND_ALWAYS(balance == expected, "balance-adds-up", "balance %ld, expected %ld", balance,
	      expected);
```

`REWIND_ALWAYS` must be true every time the program reaches it. A run where
it is false violates the `sdk` property, and check prints the message.

`REWIND_SOMETIMES` tests the search, not the program: its condition must be
true in at least one of the runs check tries. It never fails a run. Here it
asks whether any run had both tellers mid-deposit at once.

Under schedule 0 alone, `bank` passes. That is one run, one teller always
finished its deposit before the other started, and check reports the
`sometimes` that never came true:

```console run name=alone show=-3:
$ rewind check --epoch {{epoch}} --schedules 0 {{repo}}#bank
```

<!-- assert: grep -q '^never true in any run: sometimes "deposits-concurrent"$' {{out:alone}} && grep -q '^every property held under the one schedule tried$' {{out:alone}} -->

With its default 64 schedules, check puts both tellers mid-deposit and finds
a run that loses a deposit:

```console run name=bank
$ rewind check --epoch {{epoch}} {{repo}}#bank | grep -E 'decides it|passing:|failing:|failing run violates sdk|^true in'
```

<!-- capture bank_passing: passing: run (\w+) -->
<!-- capture bank_failing: failing: run (\w+) -->
<!-- capture assert_step: failing run violates sdk: step (\d+): -->
<!-- assert: grep -qE 'failing run violates sdk: step [0-9]+: always "balance-adds-up" failed: balance [0-9]+, expected 400' {{out:bank}} -->
<!-- assert: grep -qE '^true in [1-9][0-9]* of [0-9]+ runs that checked it: sometimes "deposits-concurrent"$' {{out:bank}} -->

Outside Rewind the SDK's calls do nothing, so they can stay in the program.
The [SDK](https://github.com/fzakaria/rewindvm/tree/main/sdk) has a small
library for C, Rust, Go, Python, Java and shell, and a pytest plugin and a
JUnit 5 extension that mark each test's result.

## Check the fix

Clone the repository to edit `bank`:

```console run hide
$ git clone https://github.com/fzakaria/rewindvm
$ cd rewindvm
```

In `examples/bank/bank.c`, hold a mutex from each deposit's read of the
balance to its store:

<!-- diff: bank-fix.patch -->
<!-- run: patch -p1 < {{dir}}/bank-fix.patch -->

```console run name=fixed
$ rewind check --epoch {{epoch}} .#bank | grep -E '^true in|^every property'
```

<!-- capture fixed_hits: true in (\d+) of -->
<!-- assert: grep -qE '^true in [1-9][0-9]* of 65 runs that checked it: sometimes "deposits-concurrent"$' {{out:fixed}} && grep -q '^every property held under all 65 schedules$' {{out:fixed}} -->

The balance adds up in all 65 runs, and in {{fixed_hits}} of them both
tellers were mid-deposit at once, so the search made them contend for the
lock and the lock held. Without the `sometimes`, a clean search could also
mean the tellers never overlapped.

The `sometimes` marks the risky state rather than the bug. One true only
when a deposit is lost would never come true once the lock is in, and
check would report it as never reached on a correct program.

## Vary the randomness

`hash-order` tests a function that collects tags in a Rust `HashSet` and
expects them back in a fixed order. A `HashSet`'s order depends on a random
key that Rust draws from the system when it makes the set. In the VM, the
system's randomness comes from the run's `--seed`, so the key is the same on
every schedule, every schedule passes, and no number of schedules finds the
bug. `--seeds 8` runs the job under 8 seeds:

```console run name=seeds
$ rewind check --epoch {{epoch}} --seeds 8 --schedules 0 {{repo}}#hash-order | grep -E '^seed|decides it|passing:|failing:'
```

<!-- capture deciding_seed: seed (\d+) decides it -->
<!-- capture hash_passing: passing: run (\w+) -->
<!-- set hash_start: rewind events {{hash_passing}} --json | jq -r 'select(.kind.type == "mark" and .kind.text == "rewind-start") | .step' -->
<!-- assert: grep -qE '^seed [0-9]+ decides it: under schedule 0, seed [0-9]+ passes and seed [0-9]+ fails$' {{out:seeds}} -->

Seed {{deciding_seed}} puts the tags in the other order and the test fails.
With `--schedules` above 0, check also perturbs the schedule under each
seed, so one search varies the randomness and the timing together.

## In the desktop app

Open the failing `bank` run beside the passing one, at the assertion:

```console
$ rewind open {{bank_failing|short}} {{assert_step}} --compare {{bank_passing|short}}
```

<!-- screenshot site/img/app-assertion: {{home}}/runs/{{bank_failing}} --compare {{home}}/runs/{{bank_passing}} --step {{assert_step}} ;; key plus ;; key plus ;; key plus ;; key plus ;; key plus ;; key plus ;; key plus ;; key plus ;; wait 2 -->

![The app zoomed in around step {{assert_step}} of the failing bank run: a red mark above the track where always "balance-adds-up" failed, the test's span under the track in red, and the event card naming it](../site/img/app-assertion.png)

The red mark above the track is the failed assertion; hover over it to see
its message. The event card names it too, and Jump to failure goes to it.
`bank` also marks its test's start and result with the SDK, as the pytest
and JUnit plugins do for each test. The strip under the track has a span per
test: green if it passed, red if it failed.

The chevron beside Fork from here also offers Check from here with 8 seeds
of 8 schedules. It tries 8 schedules under the run's own seed, and 8 under
each of 7 other seeds, as forks reseeded at the playhead. At the start of
`hash-order`'s passing run it shows a row of cells per seed, blue where the
test failed:

<!-- screenshot site/img/app-seeds: {{home}}/runs/{{hash_passing}} --step {{hash_start}} ;; click 1408 189 ;; wait 1 ;; click 1300 397 ;; wait 1 ;; click 1300 189 ;; wait 120 -->

![The app after Check from here with 8 seeds of 8 schedules at the start of hash-order's passing run: a card with a row of cells per seed, the seeds that made the test fail in blue](../site/img/app-seeds.png)

## What to read next

- [The advanced tutorial](tutorial-advanced.md): a property as a script, a
  file of properties, and seeds from a step of a recorded run.
- [The SDK](https://github.com/fzakaria/rewindvm/tree/main/sdk): what each
  call is for, in every language.
- [Design](design.md#exploring-interleavings): how the search and narrowing
  work.
