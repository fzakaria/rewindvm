---
date: 2026-11-13
kind: audit
revision: bf1cfa9f
---

# Release audit of Rewind 0.16.0

Hold the release's critical-path and shortening claims until A1 is resolved.
A recorded program joins a worker that computes for 100 ms, but `rewind time`
reports a 2.2 ms critical path. The worker finishes before its join, so the join
creates no wakeup edge and the graph loses a required dependency.

All seven findings below were open at revision `bf1cfa9f`. This audit changed no
production code.

## Scope and evidence

Reviewed the 28 commits from v0.15.0 (`c3a7c769`) through `bf1cfa9f`, with a clean
working copy. The review covered the guest's switch and wake records, critical
path construction, timing reports, Perfetto output, the Time tab's model and CLI
source navigation. It also covered the changed keyframe export and app layout
code, and checked the release's recorded benchmarks and guest identity.

The review compared the feature with
[Worshipping at the altar of Brendan Gregg](https://fzakaria.com/2026/11/12/worshipping-at-the-altar-of-brendan-gregg),
published on 2026-11-12. Code locations below refer to the reviewed revision.

Validation on nyx:

| Check | Result |
| --- | --- |
| `nix develop -c cargo test --workspace -q` | 770 passed, including one doc test |
| `nix develop .#app -c cargo test --manifest-path crates/rewind-app/Cargo.toml --lib -q` | 424 passed, two ignored |
| `nix flake check --no-build` | Passed evaluation; did not build the integration checks |
| KVM recording and replay of `sleep 0.1` | Identical: 189 events, 423 context switches and three read/write records over 779 steps |
| KVM recording of the joined-worker reproduction | Confirmed A1; gdb inspection confirmed A3 |
| Synthetic graph probes | A2 and A4 failed their assertions |
| Decoding exported Perfetto track events | Both A5 interval assertions failed |
| Deserializing a 0.15 layout with the current type definitions | Confirmed A7 |
| Optimized keyframe traversal probe | Measured A6's scaling |

The VM probes used an isolated `REWIND_HOME` under
`/tmp/rewind-release-review/home`. The joined-worker run used the AMD retired
conditional branch clock. Scratch sources, logs and recordings were left under
`/tmp/rewind-release-review`; those paths are session artifacts. The procedures
and input schedules below preserve the reproduction details after that directory
is gone.

The full Nix integration build matrix and an interactive desktop/browser review
were not run. App findings are based on code, model tests and the CLI/gdb probes
described below.

## Repair order

P1 blocks the release's stated timing behavior. P2 covers incorrect answers or
scaling problems to repair before relying on the feature for larger recordings.
P3 is a smaller upgrade regression.

| ID | Priority | Finding | Evidence |
| --- | --- | --- | --- |
| A1 | P1 | Completed joins disappear from the critical path | KVM recording and graph assertion |
| A2 | P2 | Reused thread IDs connect unrelated lifetimes | Synthetic trace assertion |
| A3 | P2 | The suggested source command selects the wrong thread | CLI output and gdb JSON |
| A4 | P2 | Phase totals drop work across phase boundaries | Synthetic trace assertion |
| A5 | P2 | Perfetto merges CPU gaps and shifts sleep intervals | Exported track events |
| A6 | P2 | Replayable export repeatedly walks complete ancestry | Optimized timing probe and code |
| A7 | P3 | Upgrading resets saved panel layouts | Deserialization assertion |

## A1. Preserve dependencies on completed joins

Location: [Graph::new](../../crates/rewind-trace/src/critical.rs#L224), with the
start constraints evaluated by
[Graph::earliest](../../crates/rewind-trace/src/critical.rs#L456).

The graph learns dependencies from forks and from wakes after a thread blocks.
A join of an already-completed thread can return in userspace without blocking,
so the recording supplies no wakeup edge for that dependency. Following the
job's final thread backward through the graph can omit work the job required.

The reproduction starts two workers. Thread 35 performs 100,000,000 iterations
of a volatile accumulation loop. Thread 36 sleeps for 1 ms and performs
1,000,000 iterations. Main joins thread 36, then thread 35. The long worker has
already exited when main reaches its join.

The recorded run `f899f0c55e1dabf7`, named `review-joins-sleep`, produced:

| Quantity | Observed value |
| --- | --- |
| Job elapsed time on the VM clock | 102,341,916 ns |
| Long worker's total computing | 100,070,112 ns |
| Reported critical path | 2,233,842 ns |

The CLI rounds the last number to 2.2 ms. Its per-thread shortening table names
thread 36's computing and sleeps, while the required 100 ms worker is absent.
An assertion that this job's path must be at least the long worker's computing
time fails.

The reported number can be a loose lower bound, but the graph does not establish
the full critical path or the savings advertised by "What would shorten it".
The same limitation matters for other dependencies that complete without a
recorded block and wake.

Repair: represent completion dependencies, including joins that never block. If
the recording cannot supply those edges, describe the result as a bound derived
from recorded wakeups and qualify or withhold the shortening predictions. A
change to the output wording alone does not recover the missing work.

Acceptance test: use the program in the reproduction section below and verify
that its critical path includes the required long worker. Cover both join
orders and a worker that exits before main calls either join. Exercise the
corresponding shortening estimates too.

## A2. Separate each lifetime of a reused thread ID

Location: [Graph::new](../../crates/rewind-trace/src/critical.rs#L196).

`by_thread` groups runs by numeric TID and links each run to the previous run
with that number. The fork map uses `or_insert`, retaining only the first fork
for a TID. When Linux reuses an ID, the new thread continues the old thread's
chain, and its own fork dependency is skipped.

The synthetic reproduction starts two children in sequence. With child IDs 2
and 3, the path is 57 ns. Reusing ID 2 for the second child changes the path to
53 ns. The second child's start changes from `Forked` to `Ready`, although its
work and dependencies are otherwise identical.

The fixture uses these switches; each switch's clock is `step + 1`:

| Step | Incoming TID | Syscall the outgoing thread blocked in |
| --- | --- | --- |
| 0 | 1 | None |
| 10 | 2 | `wait4` |
| 20 | 1 | None |
| 50 | 3, or reused 2 | `wait4` |
| 60 | 1 | None |

Init's job marks are at steps 1 and 69, and the final clock is 71 ns. Init forks
the first child at step 5 and the second at step 45. The children exit at steps
19 and 59. Their task wakes of init occur at step/time pairs `(20, 20)` and
`(60, 60)`. Only the second child's numeric ID differs between the two traces.

Repair: resolve runs, forks and wakes against thread lifetimes. Reset the
previous-run relationship at each new lifetime; the process `Life` machinery
elsewhere in the trace reader already treats reused IDs explicitly.

Acceptance test: require the two traces above to have equal path lengths and
shortening results. Cover a reused non-main thread ID within one process as
well as a reused process ID.

## A3. Carry the selected thread into the source command

Location: [print_inside](../../crates/rewind/src/cmd/time.rs#L260).

The "Look closer" command includes `--pid` but omits `--tid`, even when its row
describes one thread. At a switch step with no event, `rewind where --pid PID`
selects that process's main thread.

For A1's run, `rewind time review-joins-sleep --in 34` names thread 36's computing
in the first row and suggests:

```sh
rewind where f899f0c5 748 --pid 34
```

The JSON result has `pid: 34, tid: 34` and shows main waiting in
`__pthread_clockjoin_ex`, called from `main`. Adding `--tid 36` produces
`pid: 34, tid: 36`, with the new worker at `__GI___clone3` at that step. The
suggested command therefore inspects a different thread from the row.

Repair: retain the TID of the piece a row refers to and include it in the
generated command. Thread and thread-sleep rows already identify a TID in
`RowKind`; aggregated rows need the TID corresponding to their chosen step.

Acceptance test: require the command generated for a worker's computing or
sleep row to contain both `--pid` and `--tid`. Run the command against a recording
and check the returned thread identity.

## A4. Split path pieces at phase boundaries

Locations: [Timing::report](../../crates/rewind-trace/src/time_report.rs#L563)
and [Timing::path_tree](../../crates/rewind-trace/src/time_report.rs#L971).

Phase accounting assigns an entire piece using its CPU run's start time. The
app's phase tree similarly filters by the run's start step. A phase marker can
occur inside a CPU run, so work after the marker can be attributed to the
preceding phase or excluded from every phase. A sleep is also associated with
the run that resumes afterward, which is not the interval the sleep occupied.

The synthetic reproduction has init on the CPU from step/time `(0, 1)` until
the idle switch at `(100, 101)`. Its job marks are at steps 1 and 90. It prints
`Running phase: buildPhase` at step 10 and `Running phase: checkPhase` at step
50. Both phase durations are 40 ns under the graph's own clock interpolation,
but both report `on_chain_ns: 0` because the CPU run started before either
phase. The assertion expecting `[40, 40]` receives `[0, 0]`.

Repair: intersect each piece's actual interval with the phase boundaries and
use the resulting portions in the CLI totals, folded stacks and app phase
trees. Computing and sleeps need their respective interval endpoints.

Acceptance test: cover multiple phase markers within one CPU run, a sleep that
crosses a marker and work before the first marker. Require phase totals and
scoped trees to agree, with time outside the phases accounted for separately.

## A5. Preserve the recorded intervals in Perfetto

Location:
[chain_on_the_timeline](../../crates/rewind-trace/src/perfetto.rs#L361).

Two interval errors affect the critical-path track:

- Consecutive pieces belonging to the same process and state are merged without
  checking whether their intervals touch. In the preemption fixture, 29 ns of
  computing in two separated CPU runs becomes one slice from 1 to 71 ns. The
  slice spans work another thread performed while the first was off the CPU.
- A sleep is placed at `run.start_ns - piece.ns` through `run.start_ns`.
  `run.start_ns` is dispatch time, so a delay between wakeup and dispatch shifts
  the sleep. A fixture that sleeps from 11 to 21 ns and resumes at 51 ns exports
  the sleep as 41 to 51 ns.

Both errors were checked by decoding the protobuf track events emitted by
`perfetto::trace`. The separate-computing-slices assertion received one slice
instead of two; the sleep assertion received `[(41, 51)]` instead of
`[(11, 21)]`.

Repair: retain the piece's actual start and end, clip computing at the job
boundaries, and merge only contiguous intervals with the same identity and
state. Anchor timer sleeps at the preceding switch-out and the recorded wake.

Acceptance test: decode the exported trace and assert these intervals. Include
preemption, delayed dispatch after a timer wake and a job mark inside a CPU run.
Checking protobuf field encoding alone does not cover interval placement.

## A6. Visit each keyframe ancestor once during export

Locations:
[Layers::recording](../../crates/rewind-core/src/keyframes.rs#L394),
[Layers::chain_loaded](../../crates/rewind-core/src/keyframes.rs#L468) and
[write_export](../../crates/rewind-core/src/export.rs#L163).

`recording` calls `chain_loaded` for every recorded keyframe. Caching avoids
reading a file twice, but each call still follows the entire parent chain and
builds a vector of its ancestors. A linear chain of N recorded keyframes causes
N(N + 1)/2 ancestor visits.

The probe created one full keyframe followed by deltas, with empty page lists,
then populated `Loaded` with one call to `recording`. The measurements below
are the mean of three further calls using that cache, with optimized release
libraries and thin LTO. They exclude file creation and initial loading.

| Recorded keyframes | Time per cached scan |
| --- | --- |
| 1,000 | 21 ms |
| 2,000 | 88 ms |
| 4,000 | 380 ms |

Doubling the chain produces about four times the traversal cost. The export
also retains every decoded keyframe, including its page vector, until the
export finishes. That memory retention follows from the code; the empty-page
probe did not measure peak memory for a large guest.

Repair: stop following parents once the traversal reaches a previously visited
keyframe. Keep only the ancestry information needed to select the export's
keyframes, and release page vectors after collecting their hashes.

Acceptance test: preserve the recorded/kept/shared-keyframe selection tests and
add a long-chain probe that counts ancestor visits. Require linear growth in
visits. Measure peak memory separately with populated page lists; avoid a test
that asserts a machine-specific timing threshold.

## A7. Default the new layout field when reading older settings

Locations: [Splits](../../crates/rewind-app/src/ui/splits.rs#L30) and
[Splits::parse](../../crates/rewind-app/src/ui/splits.rs#L106).

The new `lists: ListSizing` field has no serde default. Deserializing a layout
saved by 0.15 fails with `missing field lists`; `parse` then discards the entire
saved layout and returns `Splits::default()`. Existing column proportions and
list heights are reset on upgrade.

The probe extracted the current `Splits` and `ListSizing` definitions and tried
to deserialize this valid old layout:

```json
{"log":2,"middle":1,"right":1,"procs":1,"files":1,"terminal":0.3,"variables":200,"frames":140}
```

Repair: add `#[serde(default)]` to `lists`, using `ListSizing::Automatic` when
the old layout supplies no value.

Acceptance test: load the old JSON through `Splits::parse` and retain its
existing dimensions. Also round-trip a new layout with `ListSizing::Dragged`.

## Recording performance and release communication

The checked-in [benchmark history](../../site/bench/history.json) has five
rounds for each release on leviathan, an AMD EPYC 7763 host with 256 logical
CPUs, 251 GiB of memory and kernel 6.18.44. These are existing measurements,
separate from the keyframe probe on nyx.

| Workload | v0.15.0 median wall time | v0.16.0 median wall time |
| --- | --- | --- |
| GNU hello build | 22.89 s | 22.93 s |
| Replay | 21.80 s | 21.80 s |
| Schedule exploration (`check`) | 1.30 s | 1.44 s |

The build and replay medians change little. The exploration median rises by
about 10.8%; repeat that measurement before attributing the change to wakeup
recording. Host load varied across the stored rounds. The existing workloads
also do not exercise A6's export traversal.

The post and release notes should explain that the timing figures use the VM's
branch-counted clock and timer waits. Work in the kernel contributes little to
that clock. A1 requires qualifying the claim that the reported path is the
least time possible on any number of CPUs and the corresponding savings claims.

Document the recording compatibility boundary. Guest interface 7 and switch
batch format 2 replace the previous formats. Extracting v0.15.0's bundled
`mylib-fail.rwd` and reading its trace with the new CLI fails with:

```text
the switches at step 4096 are in format 1; this build reads 2
```

That affects trace inspection as well as replay. The version is already 0.16.0
in `VERSION` and the Cargo manifests, and `docs/templates/recorded-with` matches
the guest used by the development shell at the reviewed revision.

## Lessons from perf's top-down analysis

The [perf top-down guide](https://perfwiki.github.io/main/top-down-analysis/)
measures a broad group, drills into the dominant metric's subgroup, then samples
an event to locate the code responsible. Its retiring, frontend-bound,
backend-bound and bad-speculation categories describe CPU pipeline behavior.
The recommendations below apply that workflow to Rewind; they are design
feedback, separate from the reproduced defects above.

1. State what the computing number measures beside the result. Rewind's
   [counter clock](../pmu.md#what-the-two-clocks-do) adds one virtual nanosecond
   per counted conditional branch, plus exit charges and timer advances. A
   change that reduces cache misses or misprediction penalties can improve
   native runtime without reducing that branch count. The current trace cannot
   assign computing time to perf's hardware categories. Describe the path and
   shortening estimates as results under the recorded VM clock and dependency
   model, even after A1 is repaired. Native performance claims need native
   measurements.

2. Make each breakdown reconcile with its stated denominator. Keep elapsed VM
   time and modeled path length distinct. Within a selected path scope,
   computing, timer sleep and any unattributed remainder should sum to that
   scope; child contributions plus the parent's own contribution should do the
   same. Show work outside named phases explicitly. These are useful acceptance
   conditions for A4 and A5. The "What would shorten it" rows are overlapping
   counterfactuals, so their savings must not be presented as an additive
   breakdown.

3. Finish each drill-down with the evidence needed for the next measurement.
   Time already leads from the job to a process, thread and computing or sleep
   interval. Preserve the selected interval's process lifetime, TID and step
   range when opening source or exporting it. A3 loses the TID. The app's
   [computing source lookup](../../crates/rewind-app/src/ui/time.rs#L197) shows
   where the longest interval began; that location does not establish which
   function consumed the interval. Keep that distinction visible and use
   sampling for a future hotspot view.

4. Treat hardware profiling as an optional follow-up. Start with a native
   reproduction of the selected work and discover the machine's metrics with
   `perf list`. Where supported, the guide's starting command is
   `perf stat -M TopdownL1 -- ./workload`. Do not assume its Intel metric names
   apply to the AMD hosts used here. Any future measurements attached to a
   replay need guest/process attribution and their own host/CPU metadata;
   ordinary host-wide counts cannot be assigned to a guest interval. Replay
   fixes the program's execution, but does not restore the host's caches or
   branch predictor.

Counter capacity also constrains that last proposal. The guide warns that
multiplexing lowers metric accuracy. Rewind's [PMU reader](../../crates/rewind-vmm/src/pmu.rs#L802)
requires full counter coverage, and [open_event](../../crates/rewind-vmm/src/pmu.rs#L900)
pins the replay counter. Extra events must preserve that counter's coverage;
collect smaller groups separately and record their enabled/running fractions.
Never feed scaled or sampled profiling counts into the deterministic clock.
The release work is the clock explanation and accounting repairs. Integrated
hardware profiling needs a separate design and has not been tested in this audit.

## Joined-worker reproduction

Save this program as `joins.c` and compile with
`nix develop -c cc -O0 -g -pthread joins.c -o joins`. Put the binary at
`root/bin/joins` and copy its dynamic loader and shared libraries into `root`
at their absolute paths. `ldd ./joins` lists the paths; preserve the ELF
interpreter path as well as any resolved library paths.

```c
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>

static const uint64_t LONG_WORK = 100000000;
static const uint64_t SHORT_WORK = 1000000;
static const struct timespec SHORT_SLEEP = {.tv_sec = 0, .tv_nsec = 1000000};
static const char COMPLETED[] = "both workers joined";

// Run a branch-counted loop, delaying the short worker first.
static void *compute(void *input) {
    uint64_t iterations = *(const uint64_t *)input;
    volatile uint64_t value = 0;

    // Let the long worker finish before the short worker is joined.
    if (iterations == SHORT_WORK) {
        nanosleep(&SHORT_SLEEP, NULL);
    }

    // Keep the computing visible to the branch counter.
    for (uint64_t i = 0; i < iterations; i++) {
        value += i;
    }

    return NULL;
}

int main(void) {
    pthread_t long_thread;
    pthread_t short_thread;

    // Start both workers before joining either worker.
    if (pthread_create(&long_thread, NULL, compute, (void *)&LONG_WORK) != 0) {
        return EXIT_FAILURE;
    }
    if (pthread_create(&short_thread, NULL, compute, (void *)&SHORT_WORK) != 0) {
        return EXIT_FAILURE;
    }

    // Join the short worker before checking the completed long worker.
    if (pthread_join(short_thread, NULL) != 0) {
        return EXIT_FAILURE;
    }
    if (pthread_join(long_thread, NULL) != 0) {
        return EXIT_FAILURE;
    }

    puts(COMPLETED);
    return EXIT_SUCCESS;
}
```

With the root assembled, run from the repository in a development shell:

```sh
export REWIND_HOME="$(mktemp -d /tmp/rewind-join-audit.XXXXXX)"
cargo run -q -p rewind -- run -q --clock branches --no-keyframes \
  --name completed-join --root /absolute/path/to/root -- /bin/joins
cargo run -q -p rewind -- time completed-join
cargo run -q -p rewind -- events completed-join
```

Check the worker exit order in the events: the long worker must exit before the
short worker, whose completion releases main's first join. The saved review
run had that order at steps 761 and 773. Run IDs and steps can change with the
compiler, guest or executable paths; use the new recording's IDs when checking
source navigation.

For a regression test, decode the trace, identify the long worker, sum its
computing intervals and require the job's critical path to be at least that
sum. This workload explicitly joins both workers, so both computations are
required for completion. Apply the same test after reversing the join order.
