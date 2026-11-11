---
date: 2026-11-04
kind: review
revision: 7481c985
---

# Memory blame review and handoff

Reviewed the 15 pushed commits after `329ded7c` through
`7481c9851fd66c50f58af0a7f6f922780c08bd3f`. Two new P1 regressions are
reproduced on the exact final binary: forward watch coalescing can lose a
write and skip another breakpoint (R13), and retained GDB symbols can blame
code that never executed (R14). Fix both before treating this revision as
ready. The syscall reverse-step WIP also exposes an existing failure (R15).

The previous R12 executable-lifetime failure is fixed. The new test assertion
repairs matter: the old negative assertions could pass after the command
failed. The performance measurements below found no return of quadratic
reverse-watch work, but one reverse-stepi workload costs about 15% more wall
time and CPU. No recording-format expansion was introduced by this range.

The source changed during this review. Results are pinned to the commits
named here; later changes are outside this signoff. The supplied in-flight
assertion and source branches were examined separately and together, then
checked at their pushed revisions. No production code was changed by this
review. The original findings and product roadmap remain below as history.

| Finding                                   | Current status                | Evidence and limits                                                                                                                     |
| ----------------------------------------- | ----------------------------- | --------------------------------------------------------------------------------------------------------------------------------------- |
| R1-R3, access completeness and retention  | Addressed on prior evidence   | Original independent probes passed at `02649a9e`; current blame-memory check passes. The original probes were not all repeated.         |
| R4-R7, app identity and request lifetime  | Addressed on prior evidence   | App tests passed at `86d99df1`; the current app has 317 passing unit tests and five integration tests. No GUI interaction in this pass. |
| R8, huge-page advice                      | Addressed on prior evidence   | Seccomp refusal probe passed at `02649a9e`; that implementation is unchanged in the new range.                                          |
| R9, reverse interruption                  | Addressed                     | Repeated-command SIGINT probes still stop; the current reverse Nix check passes. See timing limits below.                               |
| R10, mmap contract                        | Addressed                     | Mapping tests and compile-fail doctest pass; no new mapping-contract change in this range.                                              |
| R11, quadratic position copying           | Addressed                     | New 80k-write benchmark stays flat against `329ded7c`; sampled VM memory stays near 120 MiB.                                            |
| R12, watch before executable lifetime     | Addressed                     | Both clocks return all five writes, then an inspectable history boundary with value zero.                                               |
| R13, forward watch loses stops            | Open, P1                      | Both signal-handler reproductions fail on pushed `7481c985`.                                                                            |
| R14, retained symbols name the wrong code | Open, P1                      | A dlclose/dlopen address-reuse fixture blames a function never called.                                                                  |
| R15, reverse-stepi across syscall entry   | Open, P2; predates this range | Independent old/new probes fail; the supplied WIP regression check fails too.                                                           |

**R13, P1: forward watch coalescing can silently lose a write**

Introduced by `8b249f90`, the pushed version of `09547f75`.
At `7481c985`, [debug.rs](../../crates/rewind-core/src/debug.rs), lines 1748-1753,
replaces the debugger's traps with a single breakpoint at the syscall's saved
return PC. Lines 1720-1724 accept that PC in the calling task as the return.
The process can enter a signal handler before reaching the saved PC. The
handler's breakpoints have been removed, and the handler can exit or call
another syscall through the same wrapper address.

Independent fixture: a static musl program receives eight bytes from a Unix
socket with `MSG_WAITALL`. A child writes five bytes, waits, then sends
SIGUSR1. The kernel copies `hello` into the global watched buffer before the
signal. Start GDB at the earlier stdout `start` event, set `watch -l buffer`
and `break handle`, then continue once.

| Handler                                      | At `41b52124`, before forward coalescing | At `7481c985`                                                                                                               |
| -------------------------------------------- | ---------------------------------------- | --------------------------------------------------------------------------------------------------------------------------- |
| Immediately calls `_exit(0)`                 | Watch stops on the first copied byte     | Inferior exits normally. Neither watch nor handler breakpoint stops.                                                        |
| Calls `write(1, "handler\n", 8)`, then exits | Watch stops on the first copied byte     | Handler breakpoint is skipped. Watch stops at the handler's write return, while the interrupted recv is still on the stack. |

Without the watch, the same handler breakpoint works and the buffer contains
`hello`. The nested-write case has the same task and musl return PC as the
interrupted recv, but is a different syscall invocation. The immediate-exit
case is the more serious failure: a real write disappears from debugging.
Both commands exit successfully, so checking exit status alone misses this.

Preserve active breakpoints and watches during coalescing. Recognize the
actual call's completion, including signal delivery and nonlocal exit; a
return address plus task identity is insufficient. If completion cannot be
established, report the pending watch at a valid earlier stop. Avoid resuming
past unrelated debugger stops merely to improve presentation.

Regression acceptance: retain the normal single stop after a completed
multi-byte read; cover the two handlers above, an unrelated breakpoint,
another thread using the same syscall wrapper, interruption, and disconnect.
Assert stop identity and stack/value, not merely the presence of a watchpoint
message. Preserve both clocks and forward/backward inspection after the stop.

Local reproductions use `REWIND_HOME=/tmp/rewind-review-home`:

```sh
rewind gdb recheck4-signal-exit 302 -- -batch \
  -ex 'watch -l buffer' -ex 'break handle' -ex continue \
  -ex 'print buffer' -ex bt
rewind gdb recheck4-signal-return 302 -- -batch \
  -ex 'watch -l buffer' -ex 'break handle' -ex continue \
  -ex 'print buffer' -ex bt
```

To recreate the inputs: use a global eight-byte buffer, install a SIGUSR1
handler without `SA_RESTART`, create a `SOCK_STREAM` socketpair, and emit
`start`. Fork a child that sleeps 20 ms, writes five bytes to its socket,
sleeps another 20 ms, signals its parent, and exits. The parent calls
`recv(socket, buffer, 8, MSG_WAITALL)`. Build each handler variant statically
with musl and debug information, then record with the exit clock. Find the
start event in that recording instead of assuming step 302.

Sources: `/tmp/rewind-review-root/src/recheck-signal-{exit,return}.c`.
Recordings: `8604a24f629ebae0` and `d8b40e23085f3c18` respectively.
Scripts: `/tmp/rewind-recheck4-signal-exit-watch.py` and
`/tmp/rewind-recheck4-signal-watch.py`. Exact-final logs:
`/tmp/rewind-recheck4-signal-exit-final-True.log` and
`/tmp/rewind-recheck4-signal-final-True.log`.

**R14, P1: symbol reuse can falsely identify the historical writer**

Introduced by `7481c985`, reviewed first as `650b042e`.
At [blame_memory.rs](../../crates/rewind/src/cmd/blame_memory.rs), lines 392-415,
GDB reuse checks only PID and process start/exec. The code explicitly omits
library loads and unloads. A process can replace its executable mappings
without exec, so those checks do not establish symbol validity.

Independent fixture: a dynamically linked program has `volatile long total`.
It dlopens `first.so`, emits `loaded`, and calls `actual_writer(&total)`.
That function stores 7 and emits `inside`. Main dlcloses the library, then
dlopens `second.so` at the reused address and emits `done`. The replacement
library contains a function `never_called` at the same offset, with the
statement `*value = 9;`. Main never calls that function.

Blame `total` at the done event. Both revisions find the same real write:
step 313, PC `0x7fc1c0ee011f`, before 0, after 7. Their source answers differ:

| Revision   | Reported writer                           |
| ---------- | ----------------------------------------- |
| `41b52124` | `actual_writer`, first.c:6, `*value = 7;` |
| `7481c985` | `never_called`, second.c:6, `*value = 9;` |

The final command exits zero with plausible JSON. This is a false answer,
not a missing-symbol fallback. It directly misleads the bug investigation.
The `loaded` event deliberately creates a recorded step while first.so is
mapped, so the earlier implementation has the correct historical symbols.

Compare executable mappings, image identities, and load addresses at the
historical position before reusing symbols. Reusing the GDB process can still
save work, but its loaded objects must match the inspected memory. Fall back
to loading the historical symbols when that cannot be established. Test
unload without replacement, replacement at the same address, dlopen, exec,
and PID reuse. An unknown source is preferable to a source answer from the
wrong binary. Include a check that every reported writer actually belongs
to the image mapped at the access.

Local reproduction, with the same review home:

```sh
rewind blame recheck4-dlopen-mapped --at 315 --pid 34 --tid 34 \
  --writes 1 --json total
```

To recreate: build two shared libraries with identical function layout,
`-O0 -g -shared -fPIC`, and a main linked with `-ldl`. Each library's function
takes `volatile long *`, stores its distinct constant, then writes `inside`
to stdout. Main loads first.so, emits `loaded`, calls its function, unloads
it, loads second.so, and emits `done`. Include the dynamic loader and libc
in the guest root. Confirm the loader reuses the code address before using
the fixture to test invalidation. Record and query the done event.

Sources are in `/tmp/rewind-review-dlopen-root/src/{main,first,second}.c`;
recording `3dde7d747a29cfed`. Responses:
`/tmp/rewind-recheck4-dlopen-{prior,kept,final}.json`. The final response was
produced by the exact pushed binary. All source lines quoted here are at
`7481c985`.

**R15, P2: the syscall reverse-step test exposes an existing gap**

The WIP check `8a4d62c8` expects raw CPU reverse-stepi at a recorded syscall
event to stop at the immediately preceding kernel `out` instruction. It
instead stops at the user-space `syscall`, skipping the kernel work between.
The Nix check fails at its first exit-clock case. Keep that failure visible
until the implementation is repaired.

An independent probe gives the same result on `329ded7c` and `41b52124`:
start at the burst program's done write, select thread 2, reverse-stepi, then
select CPU thread 1 and inspect the instruction. Both stop at
`0x401951 <__syscall_cp_c+26>: syscall`. This predates the new pushed range.
The final passing reverse check does not include the WIP assertion.

`6cb529ce` re-arms KVM stepping after each single-step exit. That does not
cover a transition which clears stepping before another debug exit arrives.
Handle syscall entry as well as return, and assert the real CPU instruction
on both sides of the boundary. Preserve user-thread display semantics,
four-register cases, signal paths, and replay-position reconstruction.

Probe logs: `/tmp/rewind-recheck4-syscall-{prior,main}.log`.
Failing WIP check: `/tmp/rewind-recheck4-syscall-check.log`, derivation
`/nix/store/8nc8cki5zfwsdkdq3xh9m25x91p1k8rx-rewind-gdb-reverse.drv`.
This is separate from the successfully resolved final reverse check.

**Repairs that held up**

The backward exec bound in `c08d7e92` closes R12. Both clocks return
`15 -> 10 -> 6 -> 3 -> 1 -> 0` for five writes within a step, then stop at
recorded-history end. The value remains readable as zero. The forward exec
bound in `41b52124`, reverse kernel-watch coalescing in `b49b715a`, and the
four-register reverse-next case pass the final checks. R13 is a separate
forward coalescing regression.

The repeated-interrupt probes still respond after a six-second interrupted
search. On the combined source/assertion snapshot, the second reverse-stepi
stopped 33.7 ms after SIGINT; reverse-continue took 20.6 ms. Disconnect was
reported within 315.8 ms under host load. The probe then cleaned up, so that
last number does not prove complete process exit. These are single probes
with 10 ms polling, not tail-latency bounds. The final Nix reverse check also
passes the repeat-interrupt cases.

The debuginfod lifetime repair in `fab69b2b` addresses a real problem: the
server's parent-death signal was tied to a worker thread that could finish
while GDB still needed the server. Its lifetime test passes. The assertion
repairs in `1135deff`, `26a2cd8c`, and `749d01e1` remove false passes from
negated shell commands, failed commands with convenient output, and timeouts.
The longer state-replay fixture in `bc34c281` obtains three keyframes in the
check. Keyframes are wall-time spaced; a deterministic fixture control would
be a better future test interface than tuning workload length indefinitely.

The app's separate write/read rows in `09f2579b` have passing regression
coverage. No additional app behavior change was found in the later range.
No new blocker was found in listener cleanup or the temporary debug-register
loan, within the exercised cases. That is a scoped review result, not proof
that every debugger transition is covered.

Logs: `/tmp/rewind-recheck4-burst-main-{exits,branches}.log`,
`/tmp/rewind-recheck4-repeat-interrupt-{int,int-continue,kill}.{json,log}`,
and `/tmp/rewind-recheck4-app-tests.log`.

**Measured performance and storage cost**

Compare `329ded7c` with `41b52124` on leviathan, on identical newly recorded
inputs using `--cpu v3-compat --clock exits --mem 64`. The host has 256
logical CPUs; benchmark processes and children were pinned to CPUs 240-241.
Other host jobs were active, with load around 48. Three trials per revision
and case alternated order. GDB was already attached before the timed reverse
command. CPU and peak RSS below are sampled for the VM process during that
command; they exclude GDB memory and startup. All 18 results had the expected
value; all reverse-step results stopped at the same instruction.

| Command and workload                     | Wall, 329ded7c | Wall, 41b52124 | VM CPU, old -> new | VM peak RSS, old -> new |
| ---------------------------------------- | -------------- | -------------- | ------------------ | ----------------------- |
| Reverse-continue, 20k writes in one step | 1.277 s        | 1.236 s        | 1.30 -> 1.22 s     | 119.31 -> 119.98 MiB    |
| Reverse-continue, 80k writes in one step | 4.261 s        | 4.278 s        | 4.25 -> 4.21 s     | 119.61 -> 121.43 MiB    |
| Reverse-stepi after a 20k-iteration loop | 2.044 s        | 2.349 s        | 2.06 -> 2.38 s     | 121.59 -> 121.59 MiB    |

Numbers are medians. Sampling every 10 ms misses short peaks. One new step
trial took 3.76 s; the other two took 2.35 and 2.33 s. This is too few trials
to characterize tail latency. The watch results show no material regression
on these workloads and no return to the earlier quadratic prefix copies.
The step case is about 15% slower in both wall time and CPU. The added
per-step KVM debug rearming is a plausible contributor; no per-commit
bisect or CPU profile established attribution. Keep the correctness repair
and measure any replacement against its behavior. The table does not claim
a full performance comparison of the later GDB/session changes at `7481c985`.

Script: `/tmp/rewind-recheck4-remote-bench.py`; measurements:
`/tmp/rewind-recheck4-remote-bench.jsonl`; copied logs:
`/tmp/rewind-recheck4-remote-logs/`. Remote recordings are
`07e1473e3be2adde` (20k) and `058fe8086356753e` (80k), under
`/tmp/rewind-review4-nyx-home`. Local trials during concurrent Nix builds were
too variable for a useful comparison and are excluded from these claims.

The reviewed range changes no trace, page, or keyframe encoding. The new
trace helper computes process lifetime from existing records. Format tests
pass. The extra debugger state and retained GDB are runtime memory costs;
there is no new per-access disk index or larger recording payload here.
Existing keyframes, cached sources, and retained history still consume space.
The workload sample is not a general bound on retained debugger memory.

**Performance priorities and design recommendations**

`7481c985` reuses GDB between expression evaluation and writer naming within
one CLI request. It does not implement a persistent app worker. The app
calls blame with `--address --size --class --pid --tid`
([engine.rs](../../crates/rewind-app/src/engine.rs), lines 826-845). That path
returns no retained GDB in `name_values` (lines 221-245), so the new reuse
cannot eliminate an expression-evaluation session from an app blame request.

The earlier warm mylib measurements at `329ded7c` remain the useful baseline:
expression query 1.808 s and two GDB sessions; app-style address query 0.988 s
and one GDB session. Those are three-trial medians with caches warm and
network debuginfo disabled. Applying the TODO's roughly 1.4 s for two GDB
setups to every app click was incorrect. Local comparisons of the new reuse
ran under changing host load and do not support a reliable speedup estimate.
Fix R14 before accepting a performance result for retained symbols.

1. E1: measure a bounded serial worker across actual app clicks. Reuse
   symbol/object work only with historical mapping validation. Budget retained
   VM and GDB memory, idle eviction, cancellation, exec invalidation, and store
   lifetime. Measure median and tail click latency plus total process-tree
   memory. A raw writer/value result can appear before source and surrounding
   thread naming if the UI represents the partial answer explicitly.
2. E2: test one extra checkpoint within a long step. Reuse it only at or before
   the requested target on a compatible execution prefix. A checkpoint after
   the target cannot accelerate a backwards move. Measure repeated steps
   after an interrupted search, checkpoint creation cost, and retained bytes.
   This targets the repeated full-prefix replay visible in the long-step
   probes; a general instruction index is premature.
3. Keep debugger stop collection separate from user-facing coalescing.
   Coalescing needs a pending watch with explicit invocation identity and
   competing-stop handling. Keep symbol-cache identity tied to mapped images
   and load addresses. R13 and R14 show why task/PC and PID/exec alone are
   insufficient identities. These are correctness conditions for reuse.
4. Defer shared trace storage, decoding rewrites, batched page reads, and broad
   data-layout changes until a profile identifies material cost there. The
   measured high-value work is repeated setup and replay. Neither a bounded
   worker nor an in-memory checkpoint requires format growth.

The earlier R11 gain remains worth keeping: the 80k-write probe fell from
11.588 s at `02649a9e` to 3.501 s at `86d99df1` on nyx. The new remote table
uses different recordings and hardware, so compare within each table rather
than across hosts. Restore and LZ4 measurements in the historical review
below were not repeated in this pass.

**Validation and commit coverage**

The exact pushed package and seven targeted Nix checks resolved successfully:
`gdb-reverse`, `gdb-watch`, `gdb-int3`, `blame-memory`, `state-replay`,
`determinism`, and `where`. Available successful build outputs were reused.
The source-identical predecessor `650b042e` passed 529 engine/CLI unit and
format tests plus one compile-fail doctest. Its Rust tree is identical to
`7481c985`; the intervening differences are in checks. The unchanged app
source passed 317 unit and five integration tests. The complete flake check,
a GUI session, and a new restore benchmark were not run.

The two P1 probes were repeated against the final executable after these
checks. Both still fail. The WIP syscall check is an additional failing check
and is excluded from the passing list.

| Commit     | Reviewed change                                            |
| ---------- | ---------------------------------------------------------- |
| `c08d7e92` | Backward exec boundary                                     |
| `b49b715a` | Reverse kernel-watch coalescing                            |
| `09f2579b` | Separate write/read rows                                   |
| `d37422e7` | Four-trap reverse-next check                               |
| `0686e5e7` | GDB listener cleanup on serve errors                       |
| `963e43ed` | Debug-register loan at kernel return                       |
| `6cb529ce` | Re-arm single stepping after each step                     |
| `41b52124` | Forward exec boundary                                      |
| `1135deff` | Negative assertion repair, originally `b70feeaa`           |
| `26a2cd8c` | Command-failure assertion repair, originally `40ef92b1`    |
| `749d01e1` | Fail timed-out reverse sessions                            |
| `bc34c281` | Longer state-replay fixture                                |
| `fab69b2b` | Debuginfod spawning-thread lifetime, originally `15b82fe2` |
| `8b249f90` | Forward kernel-watch coalescing, originally `09547f75`     |
| `7481c985` | Retained GDB per expression blame                          |

Final executable:
`/nix/store/axmy62vzy0jr3z4aayxan7far2zx9152-rewind/bin/rewind`.
Build outputs: `/tmp/rewind-recheck4-final-build-paths`.
Suite logs: `/tmp/rewind-recheck4-kept-engine-tests.log`,
`/tmp/rewind-recheck4-app-tests.log`, and
`/tmp/rewind-recheck4-combined-reverse-tests.log`.
Scratch files are local review artifacts; the recipes above allow the failing
cases to be recreated after those files are removed.

**Original review and roadmap**

The findings below describe the implementation before the repair commits. Use
the current status above when assigning work. Keep the restore optimizations:
the measured wall-clock gains justify them. The next performance work should
remove repeated setup and replay, with explicit memory budgets. The app should
reduce the number of questions a developer must ask to explain a failure.

This file is a repair backlog and a ranked set of experiments. Proposed thresholds
below are acceptance targets, not measured results or delivery estimates. No
engine or app fixes were implemented by this review.

The review compared main `7556c9e950013d00efe70fbb91dfe325c207d6c8` with
`b79e1fb759e0a54b1079556ea51576225a52ea88`, then inspected the popover and access
position commits through `924b1a7b5aa873c91266b2aa6ffb968dc0b34502`. Findings R1-R3
were reproduced again at `924b1a7b`. Later commits through `970860db` landed before
this handoff was written; a targeted read still finds the reported implementation
patterns, but the full suite and probes were not rerun at that tip. Start repairs
by checking the current revision and reproducing each applicable failure.

The implementation brief was `~/Downloads/Memory Blame Walkthrough.pdf`.
`TODO-polish.md` supplies the existing profiling history.
The older [audit](2026-10-09-code-audit.md) is separate work; this file does not
change its status. Follow the repository's AGENTS.md instructions, use jj, and
leave unrelated working changes alone. Write each regression test first and
observe the failure before implementing the repair.

**Repair findings**

1. **R1, P1: retain every value hit by one instruction.** Reproduced.

   Locations: `crates/rewind-core/src/accesses.rs`, `Window::replay` and
   `Window::hit`; `crates/rewind-vmm/src/debug.rs`, `db_exit`.

   The VMM returns the first watchpoint whose DR6 bit is set. The search converts
   that one piece into one value using `position`. A single 64-bit store into two
   watched 32-bit fields therefore produces only the first field's history.
   Watching the second field alone finds its write. Its cached previous bytes can
   also remain stale after an omitted hit.

   The pair probe below writes low=1 and high=2. Blaming both fields returns only
   low's 0-to-1 write; blaming high alone returns its 0-to-2 write. Preserve all
   relevant DR6 hits, map ranges to every overlapping watched value, and
   deduplicate pieces belonging to the same value for one dynamic access.

   Acceptance: both fields are reported with correct before/after bytes and the
   same access position. Add overlapping aliases, an unaligned value split
   across debug registers, a same-value write, and a later write to the field
   that used to be omitted. Keep existing GDB watchpoint behavior covered.

2. **R2, P1: identify dynamic accesses when classifying reads and writes.**
   Reproduced.

   Location: `crates/rewind-core/src/accesses.rs`, `classify`.

   The two replay streams are matched using only `(step, after_pc)`. A repeated
   instruction can read a watched byte on one iteration and write it on another.
   The REP probe below reads buffer[2] as 3, then writes 4. Blame reports a write
   from 3 to 4 followed by a read of 4. Both events share the step and RIP. The
   per-stream `hit` counters added in `924b1a7b` do not resolve that ambiguity.

   Use an execution position that distinguishes occurrences, or directly retain
   read/write trap information where hardware permits. A design using separate
   write and read/write traps must account for the extra debug-register slots.
   Matching values or thread IDs alone is insufficient for same-value stores.

   Acceptance: preserve READ(3), WRITE(3,4) order in the REP probe. Cover repeated
   same-value stores, several watched values at the same PC, and read-modify-write
   instructions with explicitly defined reporting semantics. Jumping to each
   returned access must reproduce its actual bytes and instruction position.

3. **R3, P1: bound retention without losing requested per-value writes.**
   Reproduced.

   Locations: `accesses.rs`, `search_keeping` and `Window::replay`;
   `crates/rewind/src/cmd/blame_memory.rs`, `last_writes` and text rendering.

   The global cap runs before per-value last-write selection. In the stress probe,
   one write to high is followed by 20,000 counter writes in the same VM step.
   A default query for both values incorrectly prints:

   ```text
   pair.fields.high = 2
   not written since step 301, where the search began; it held 0 there
   ```

   The default cap is documented for `--reads`, but affects write queries too.
   Meanwhile `--max 1` still constructs a vector of all 20,000 accesses before
   discarding entries. A cap on returned rows does not bound search memory.

   Retain the last requested writes per value during replay. For read histories,
   use bounded retention with explicit truncation semantics. Do not render a
   truncated history as proof that no write occurred. The classifier must also
   avoid retaining an unbounded second stream after R2 is repaired.

   Acceptance: the default two-value query returns high's write and counter's
   last write. Test caps below the number of values and define the result or
   reject the request. Increase loop length with a fixed cap and verify retained
   access memory remains bounded; replay time may still increase.

4. **R4, P2: include process identity in the app's blame cache.** Code inspection.

   Location: `crates/rewind-app/src/ui/blame.rs`, `BlameKey`, `blame`, and
   `last_write_search`.

   The key contains run, step, address, size, and query kind, but no PID. Two
   processes can have the same address at the same step, especially after fork.
   The second query then reuses the first process's value, writer, and thread.
   Include address-space/process identity. If formatted answers remain cached,
   also account for the display class or cache raw bytes separately.

   Acceptance: query the same address/size/step in parent and child with different
   values and writers. Both the popover and underline must show the right process.
   Repeating a query in one process must still reuse its result.

5. **R5, P2: clear or validate displayed blame when changing runs.** Code inspection.

   Locations: `ui/scrubber.rs`, `Scrubber::show`; `ui/blame.rs`, `Blames::shown`,
   `blame_marks`, and actions that jump to an access.

   A run switch clears source and other panel state but leaves the shown blame
   and popover. Rendering does not check the active run. Old markers and actions
   can use a previous run's step and thread against the new session. Retaining a
   cache by run is fine; displaying and executing a result requires active-run
   validation.

   Acceptance: blame run A, open B with overlapping step/thread numbers, and
   verify that A's marks, card, and jump actions are inactive. Cover a pending
   query completing after the switch and reopening A.

6. **R6, P2: reject late updates from replaced requests.** Code inspection.

   Location: `ui/blame.rs`, the asynchronous progress/completion handlers and
   `close_blame`.

   Escape removes a cancelled entry. An immediate retry inserts a new entry with
   the same cache key. The old task can then write progress or its cancellation
   error into the replacement and clear the replacement's cancellation handle.
   Add a request generation/token and require both key and generation to match.

   Acceptance: use a controllable fake engine to delay A's completion, cancel A,
   start B with the same key, then deliver A's progress and completion. B must
   remain intact and cancellable. Avoid timing-dependent sleeps in this test.

7. **R7, P2: bound speculative work and cancel work with its owner.** Code inspection.

   Locations: `ui/blame.rs`, `blame_ahead`, `BlameEntry`, `Blames`; `ui/scrubber.rs`,
   the quit handler; `engine.rs`, `Cancel`.

   Each eligible expression starts an independent engine thread/process. Each
   query can restore a full VM with up to 16 page readers. The expression parser
   permits up to 64 expressions per frame; distinct eligible addresses are not
   subject to a search concurrency budget. Blame entries have no cancellation
   on disposal, and the quit handler does not cancel them. The result cache is
   also unbounded. An OOM was not needed or attempted to establish these paths.

   Start with one speculative query at a time and a bounded queue. Prioritize
   explicit clicks, coalesce identical work, cancel obsolete jobs, and cap cache
   memory. Treat worker count and total guest/checkpoint memory as separate limits.
   Cancellation must reach the engine process group on window close and app quit.

   Acceptance: queue many distinct expressions against a fake engine and assert
   the limit, priority, and cancellation behavior. Exercise a large real guest
   with several clicks and record peak anonymous/PSS memory and input latency.

8. **R8, P2: allow VM creation when huge-page advice is unsupported.** Reproduced.

   Locations: `crates/rewind-vmm/src/memory.rs`, `prefer_huge_pages`;
   `crates/rewind-vmm/src/lib.rs`, `Machine::create`.

   Any `madvise(MADV_HUGEPAGE)` error aborts VM creation. A seccomp probe returning
   EINVAL only for that advice made the branch fail while main restored the same
   run successfully. This advice depends on host transparent-huge-page support;
   see [madvise(2)](https://man7.org/linux/man-pages/man2/madvise.2.html).
   Unsupported optional advice should leave the ordinary mapping usable.

   Acceptance: inject an unsupported-advice result and restore/replay successfully;
   retain the supported-host test. Do not change host THP settings globally to
   run the regression. The original filter is `/tmp/rewind-review-no-thp.c`.

9. **R9, P2: service interruption during reverse execution.** Reproduced.

   Locations: `crates/rewind-core/src/debug.rs`, `wait_for_stop_reason` and
   `run_back`; `crates/rewind-core/src/debug/reverse.rs`, replay/search loops.

   Reverse dispatch enters a synchronous function before the socket-polling
   forward loop. The reverse loops use `Ignore` and do not poll for Ctrl-C.
   With a long compute-only step under `--clock exits`, GDB sent the interrupt
   packet during `reverse-stepi` but received no stop response during the probe.
   The review terminated its own process group afterward.

   Pass cancellation through replay and single-stepping, including reverse
   continue. Preserve a valid, accurately reported position when interrupted.
   Polling only between VM exits is insufficient for a long guest compute region;
   integrate the VMM's host-side interruption mechanism as needed.

   Acceptance: a deliberately long reverse command acknowledges interruption
   within a defined deadline, remains inspectable, and can resume. Cover socket
   disconnect as well as Ctrl-C. A proposed local target is 250 ms outside an
   uninterruptible host syscall; document and measure the actual bound.

10. **R10, P2: make the mmap safety contract enforceable.** Reproduced API failure.

    Location: `crates/rewind-mapped/src/lib.rs`, `Mapped::file` and `bytes`.

    Safe callers can map a file, shorten the same file using safe `File::set_len`,
    then read the returned slice. The review's safe Rust probe died with SIGBUS.
    The store's atomic replacement convention protects the intended current use,
    but the constructor exposes that unchecked invariant as a safe public API.
    Put the obligation at an unsafe boundary or enforce immutable ownership.
    Consider an established mapping abstraction. Keeping unsafe syntax out of the
    store crate does not remove the caller's safety obligation. Compare
    [memmap2's contract](https://docs.rs/memmap2/latest/memmap2/struct.MmapOptions.html#safety).

    Acceptance: an ordinary safe caller cannot obtain a slice whose required
    immutability depends on an unchecked caller promise. Preserve the atomic
    replacement test. Use a subprocess with core dumps disabled for a crash
    probe; do not crash the test runner.

**Validation and measured performance**

At the original review tip, 508 workspace tests, 305 app tests, and these nine
KVM-backed checks passed: blame-memory, gdb-reverse, state-replay, determinism,
where, gdb-watch, gdb-int3, gdb-cold, and inspect-stops-threads. The updated
blame-memory check also passed at `924b1a7b`. Passing these checks did not detect
R1-R3. The four app findings R4-R7 were established by code inspection rather
than an end-to-end GUI reproduction.

Run the existing workspace/app tests and relevant KVM checks after each repair.
For changes to in-place restore or checkpoints, add a differential test: compare
all RAM page hashes, CPU state, and device state against an independently restored
machine after backward/forward moves, checkpoint branches, zeroed pages, and
debugger writes. Matching printed output alone is too weak for this path.

The following restore measurements are medians of three interleaved warm trials
on nyx, with 16 logical CPUs and about 60 GiB RAM. CPU means user plus system time.
The command was `rewind threads RUN --from STEP --to STEP --json`; this path uses
`Keep::Nothing`, so the measurements did not create keyframes. Outputs matched.

| Run and step                                   | Main wall | Branch wall | Main CPU | Branch CPU | Main peak RSS | Branch peak RSS |
| ---------------------------------------------- | --------: | ----------: | -------: | ---------: | ------------: | --------------: |
| bcachefs 5fc8b1c9ff9ddf12, 199115, 3 GiB guest |    9.71 s |      1.34 s |   9.35 s |     8.57 s |      3.28 GiB |        3.88 GiB |
| mylib 11820c42, 4782                           |   0.490 s |     0.219 s |  0.470 s |    0.436 s |       184 MiB |         815 MiB |

The branch in this table is `b79e1fb7`. The RSS increase mostly comes from mapping
the existing 636 MiB sorted index. Separate `/proc` samples showed roughly 3.25 GiB
of anonymous memory in both large-guest cases; the branch had about 640 MiB of
file-backed PSS. File-backed pages can be shared and reclaimed. RSS alone would
overstate the increase in private memory. Warm results do not establish cold-cache
performance, behavior under memory pressure, or scaling across simultaneous jobs.

Keyframes and the page store retain their version-1 formats, compression, and
hashes. Reverse checkpoints are in RAM. Source-list caching adds metadata files.
Blame/debug operations can still save ordinary keyframes through `Keep::Keyframe`,
so they can grow disk usage. No per-memory-access recording stream was added.
The profiling decision in TODO-polish.md to reject LZ4's measured 53% page-size
increase for an estimated 0.15 s wall saving is reasonable. Retain integrity
verification; its reported 0.09 s potential saving is too small to accept the
loss of integrity checks.

**Ranked engineering experiments**

Resolve the open correctness findings in the current status above. E1 is the
first performance experiment; E2 should start with one extra checkpoint.
E3-E5 are deferred until a profile shows their costs are material. Establish
an interaction benchmark with phase timing, CPU, allocations, anonymous/PSS
memory, bytes read, and disk growth. Measure time
to a usable answer, including symbol naming and teardown. Use both a small crash
and the large late restore. The initial three trials above are a baseline, not a
p95 estimate; use enough repetitions for tail measurements and report warm and
cold inputs separately. Do not drop the host's global page cache on a working
machine. Use isolated data or a dedicated benchmark host for cold tests.

| Order | Experiment                                               | Expected scope of benefit                                             | Main cost or risk                                | Acceptance gate proposed for a prototype                                                            |
| ----- | -------------------------------------------------------- | --------------------------------------------------------------------- | ------------------------------------------------ | --------------------------------------------------------------------------------------------------- |
| E1    | Bounded replay worker for the active investigation       | Related clicks avoid VM, trace, and symbol setup                      | Retained VM RAM; lifetime and invalidation       | At least 2x faster median for a fixed multi-click workflow, bounded memory, correct cancellation    |
| E2    | Checkpoints within a VM step for reverse execution       | Repeated reverse steps avoid replaying a long prefix                  | Checkpoint CPU/RAM and exact position semantics  | At least 5x fewer KVM single-step exits for 100 reverse steps in a long interval, with exact states |
| E3    | Shared trace bytes, cursors, and state-only I/O decoding | Lower allocations, copying, and teardown CPU                          | API changes across trace consumers               | At least 20% less CPU or private memory on a large trace, with an end-to-end gain                   |
| E4    | Batched page read planning                               | Fewer locks/syscalls; better cold-read locality                       | Sorting, read amplification, temporary buffers   | At least 20% restore improvement on a named workload without a material warm regression             |
| E5    | Shared bounded access-query service                      | Fewer replay passes for related values; cheaper earlier-write queries | Hardware watch slots, query identity, truncation | Match separate-query answers while reducing total replay/restore work                               |

E1: Keep one bounded engine worker for the active investigation, initially with
serial requests. Reuse the restored machine, checkpoint cache, immutable trace,
and process symbols across inspect, blame, earlier-write, and jump operations.
Start with an app-owned worker that exits with the window; a system-wide daemon
adds a larger ownership problem. Keep the CLI as a client or standalone path.
Explicit requests outrank prefetch. All requests need generation IDs, cancellation,
progress, and resource accounting. A warm worker that retains a Store handle must
not make garbage collection permanently unavailable: define idle eviction and
cooperate with maintenance.

Measure GDB reuse separately from VM reuse. The current app request uses one
GDB session and took a median 0.988 s in the trials above; the expression CLI
query uses two sessions and took 1.808 s. Reusing the remaining naming session
is a candidate, with no measured prototype speedup yet. A persistent GDB can
retain object/symbol work;
[GDB documents BFD sharing](https://www.sourceware.org/gdb/current/onlinedocs/gdb.html/File-Caching.html)
and provides [MI for programmatic clients](https://sourceware.org/gdb/current/onlinedocs/gdb.html/GDB_002fMI.html).
Those interfaces establish feasibility, not a Rewind performance result. Reuse
the existing Python/JSON logic where possible. Invalidate process state on exec
and address-space changes. Never turn a stack lookup into a long-lived mutable
debug session without restoring the required state before each query.

E2: `reverse.rs::before` and `go_to` replay from the step's start. Under exit
time, a step can contain millions of instructions. After finding a position by
single-stepping, repeated reverse commands can replay most of that prefix again.
Checkpoints keyed only by VM step cannot distinguish useful positions inside it.
Start with one extra checkpoint at an exact execution position during an
expensive search. Reuse that checkpoint only for a target at or after the
checkpoint on a compatible execution prefix. Choose placement using measured
replay cost and checkpoint bytes. Validate the simple policy before considering
a denser history, and enforce a byte budget. One snapshot per instruction would
trade the problem for unacceptable memory use.

The current `Machine::checkpoint` contract describes a step's start. Extending
checkpoints to instruction boundaries requires tests for pending exits, REP
state, debug flags, and branch-counter state; do not assume the existing API
already supports every position inside a step.

A position must distinguish loop occurrences, REP iterations, process/thread
context, and interrupted operations. RIP or branch count alone is insufficient.
Preserve checkpoint validity across trap changes and reject reverse navigation
after debugger mutation unless the new history is explicitly tracked. The first
reverse operation in an unindexed exit-clock interval may still require a long
scan; checkpoints primarily amortize later operations. Benchmark first-command
latency and the next 100 commands separately, under exit and branch clocks.
Retain branch-count acceleration and test its exactness at syscalls and returns.
TODO-polish.md already records a user `syscall` stepping issue; do not hide that
correctness work inside a performance patch. rr's
[ReplayTimeline](https://github.com/rr-debugger/rr/blob/master/src/ReplayTimeline.cc)
is useful prior implementation work for intermediate replay points, reverse
checkpoints, and interruption; Rewind still needs its own KVM-state guarantees.

E3, deferred: if a profile justifies data-layout work, inspect these costs.
`rewind_trace::records_from` allocates a `Vec<u8>` per record; `Run::records_after`
and replay setup clone record collections. `Encoder::resume` reconstructs tables
by decoding into an `IoLog` whose accumulated output is discarded. Checkpoints
clone the encoder's tables. These costs recur across queries and grow with trace
length, even when the user asks about a short interval.

Prototype an immutable owned byte arena with validated `(step, offset, length)`
entries and cheap range cursors. Share that storage across replay observers;
preserve trace ordering and the existing corruption/truncated-tail checks.
Add an I/O decoder mode that updates encoder state without materializing an
unused event log. Then profile table cloning; if it remains material, separate
shared definitions from mutable per-file positions and snapshot only changes.
An Arc around a whole mutable map can still copy the whole map after every
checkpoint; measure the copy-on-write behavior. None of these steps requires
changing the recorded trace format or mapping externally mutable files.

For checkpoint metadata, dense guest page numbers make bitmaps and flat arrays
worth testing against repeated BTreeSet/HashMap operations. At 3 GiB with 4 KiB
pages, a full dirty bitmap is 96 KiB and a u32 entry per page is 3 MiB. Sparse
checkpoints can be cheaper with sorted vectors. Select by measured density and
operation cost. The contiguous checkpoint byte buffer already landed; do not
claim that existing change as a new optimization. Change one representation at
a time and compare full restored state with the original implementation.

E4, deferred: `Store::get` acquires the index and pack-reader locks for each page,
then performs one compressed read, decompression, and hash verification. A late
restore names about 721,100 pages. Resolve page locations against an immutable
index view, retain pack handles per batch/worker, and produce compact work items
for disjoint guest destinations. Keep lookup parallelism where a serial planning
pass would become the new bottleneck. For cold reads, test grouping nearby pack
offsets and coalescing adjacent ranges, with caps on extra bytes and buffers.

Measure duplicate page hashes before adding a decompressed-page cache. If many
destinations use the same hash, verify/decompress once per restore and copy the
verified page to those destinations. This saves only work corresponding to actual
duplicates; do not assume a high hit rate. Prefer a bounded per-restore cache
before a global cache. Preserve hash verification, store/GC locking, and atomic
index replacement. Do not start with io_uring, a new compressor, or a new format
unless profiles show an unresolved cost that those changes would remove.

E5, deferred: expose one access-query model to the CLI, app, and agent clients
if repeated overlapping searches account for material workflow cost. A query
identifies the run, process/address space, byte range, exact bound, access kinds,
and requested retention. Batch related values where the hardware watch slots
allow it, after R1 and R2 are fixed. The existing core already supports multiple
values; the app currently creates separate jobs. Avoid restarting the same
search for every underline. Track coverage and truncation explicitly so an
earlier-value request can extend a search or reuse sufficient retained history.
Do not answer a full read-history query from a last-write-only cache.

**App and product work**

The product hypothesis to test is a short, repeatable path from a failing run to
an explanation another engineer can verify. Faster individual commands support
that path; fewer investigative commands can save more time. Reverse execution
and value history are already offered by [rr](https://rr-project.org/) and
[Pernosco](https://pernos.co/). Those capabilities alone do not establish demand
for another paid app. Rewind can test a workflow built around its existing VM,
Nix inputs, schedule experiments, and locally replayable evidence. Willingness
to pay remains unmeasured.

1. Make the failure view answer a concrete question immediately. Show the crash
   expression and its value, then let a click reveal the last writer with bytes,
   source, and the relevant threads at the exact access. Keep an investigation
   trail so following an earlier write never loses the original crash. Preserve
   pinned values and a back/forward history. The new exact-access jumps through
   `970860db` already address part of this; extend and test them instead of
   rebuilding them. Show a useful raw result while source naming continues.

2. Make uncertainty visible in the answer itself. Distinguish complete search,
   truncation, cancellation, unavailable memory, and unsupported write paths.
   The current search follows a virtual address back to process start/exec,
   rather than an allocation's lifetime. Reused addresses can belong to earlier
   objects. Explain that boundary near the result. Allocation/free tracking for
   supported allocators is a later feature, with an explicit coverage model;
   do not infer allocation identity from an address or a zero write.

3. Add a constrained "find when this became wrong" query after exact access
   identities work. Start with side-effect-free expressions over supported
   memory/register values and a user-selected interval. For example, find the
   transition to `queue == NULL`, then show the writer and reader that used it.
   Define whether the query finds any transition, the last transition, or the
   first violation. Binary search does not find the first violation of a general
   non-monotonic predicate; use it only with an explicit monotonicity contract,
   or scan supported transition points and report the coverage. Calls and
   overloaded operations that mutate the inferior must be rejected. The
   [transition-watchpoint paper](https://arxiv.org/abs/1703.10864) is prior work,
   not evidence that unrestricted first-violation search is cheap.

4. Finish passing/failing comparison around one selected question. Pin a value
   or thread, show the relevant accesses in both runs, and connect them to the
   scheduling difference. Align using a known common prefix or supported event
   identities, with ambiguous matches marked. Numeric steps and virtual addresses
   cannot generally align different runs or rebuilt binaries. Build on the
   existing Compare and Threads tabs and `check` narrowing. Avoid a second
   generic timeline that makes the user perform the alignment manually.

5. Save an investigation as a regression case and a shareable explanation. Keep
   the failure signature, input identities, explored schedule constraints,
   selected accesses, and source locations. Export a static summary with links
   into an existing replayable recording; incremental query results can be
   exported without an all-memory-access trace. Rebuild a candidate fix and run
   the saved test plus a stated exploration budget. Replaying the old recording
   does not validate changed code, and the same seed need not mean the same
   interleaving after a rebuild. Report exactly what passed.

6. Give agents the same bounded queries the app uses. Extend existing JSON
   commands with stable result identities, exact access references, truncation
   status, continuation semantics, and cancellation. Return enough evidence for
   an agent to cite an access and reproduce the jump. Keep the app as the place
   to inspect the claim. An agent should not need to parse screenshots or invent
   causality from source lines. Do not add a separate inference-only data model.

For performance work, use a fixed journey: open failure, inspect the suspicious
value, find its writer, inspect the writer, go one write earlier, and return to
the crash. Proposed UX targets are a cached answer within 100 ms and a repeated
nearby query within 500 ms on the small reference run. Measure CPU and retained
memory along with those targets. Long uncached searches need prompt progress,
cancellation, and navigation that remains responsive.

For purchase decisions, run observed trials on unrelated users' real failures.
Measure time to a supported explanation, number of manual actions, whether a
second engineer can follow the saved evidence, and repeat use on another bug.
Offer a paid pilot after a successful investigation and record actual acceptance.
Tutorial completion and praise are not purchase evidence. Start with a narrow
Linux systems audience whose workload already fits Rewind; the older product
document contains broader dependency-update ideas that should not delay this
shorter debugging workflow.

Preemption and compiler-assisted scheduling are separate research work. They can
increase the set of bugs Rewind can find, which may matter more than another
restore optimization, but [preemption.md](../preemption.md) records current correctness
and runtime obstacles. Do not enable the experimental path by default or promise
multicore/weak-memory coverage from a single executing vCPU. Seek a concrete
customer failure blocked by the current scheduling boundaries before committing
to that larger project.

**Reproduction material**

The original scratch home is `/tmp/rewind-review-home`. Existing run names/IDs:

| Name   | ID               | Probe                                             |
| ------ | ---------------- | ------------------------------------------------- |
| pair   | 99db11682e0eed7a | Two fields written by one instruction             |
| stress | d695f068861f04db | One field write followed by 20,000 counter writes |
| rep    | af0034f3e70656ad | Read then write at the same repeated instruction  |
| slow   | 75eae05ec406dc4a | Long compute interval for reverse interruption    |

For these exact recordings, the relevant accesses are in step 302 and the query
point is 303. Recreated recordings can have different steps; locate the final
`write(1, "done...` event using `rewind events` instead of hardcoding 303.

At the reviewed revision, with `rewind` naming the built binary:

```sh
export REWIND_HOME=/tmp/rewind-review-home
export REWIND_DEBUGINFOD=/nonexistent
rewind blame pair --at 303 pair.fields.low pair.fields.high --writes 3 --json
rewind blame pair --at 303 pair.fields.high --writes 3 --json
rewind blame stress --at 303 pair.fields.high counter
rewind blame stress --at 303 counter --max 1 --progress json --json
rewind blame rep --at 303 'buffer[2]' --reads --since 302 --json
```

The pinned binary used for the repeated probes is
`/nix/store/j6k50hsf42a3cbjqa9rylbmcmlcd8qqz-rewind/bin/rewind`.
To rebuild that revision without changing the working copy, run from the repo:

```sh
nix build --no-link --print-out-paths \
  'git+file:///home/fmzakari/code/github.com/fzakaria/rewind?rev=924b1a7b5aa873c91266b2aa6ffb968dc0b34502#rewind'
```

The scratch files may be deleted. The sources below make the three incorrect
histories reproducible without retaining the temporary recordings. Compile with
a static musl compiler, `-static -O1 -g`, place the binaries in `bin/` of a scratch
root, and record with an isolated `REWIND_HOME`:

```sh
rewind run --clock exits --mem 1024 --epoch 1793750400 \
  --name pair --root "$review_root" -- /bin/probe
rewind run --clock exits --mem 1024 --epoch 1793750400 \
  --name stress --root "$review_root" -- /bin/probe stress
rewind run --clock exits --mem 1024 --epoch 1793750400 \
  --name rep --root "$review_root" -- /bin/rep
```

On NixOS, obtain a compiler ephemerally, for example
`nix shell nixpkgs#pkgsStatic.stdenv.cc -c cc -static -O1 -g ...`.
Use explicit output paths and do not install globally. Inspect disassembly to
confirm the pair assignment compiles to one 64-bit store.

Pair and stress probe (`probe.c`):

```c
#include <stdint.h>
#include <unistd.h>

enum { START_BYTES = 6, DONE_BYTES = 5, STRESS_WRITES = 20000 };
static const uint64_t PAIR_VALUE = UINT64_C(0x0000000200000001);

union Pair {
    uint64_t whole;
    struct { uint32_t low, high; } fields;
};

volatile union Pair pair;
volatile uint64_t counter;

/* Tests simultaneous watchpoint hits and retention in a busy VM step. */
int main(int argc, char **argv)
{
    write(STDOUT_FILENO, "start\n", START_BYTES);

    /* One store changes both independently watched fields. */
    pair.whole = PAIR_VALUE;

    /* Later counter writes must not erase the field's requested history. */
    if (argc > 1) {
        for (uint64_t i = 0; i < STRESS_WRITES; i++) {
            counter = i;
        }
    }

    write(STDOUT_FILENO, "done\n", DONE_BYTES);
    return 0;
}
```

REP probe (`rep.c`):

```c
#include <stddef.h>
#include <unistd.h>

enum { BUFFER_BYTES = 6, COPY_BYTES = 5, START_BYTES = 6, DONE_BYTES = 5 };
unsigned char buffer[BUFFER_BYTES] = {1, 2, 3, 4, 5, 6};

/* Tests a read and a write at the same RIP in different REP iterations. */
int main(void)
{
    write(STDOUT_FILENO, "start\n", START_BYTES);

    /* Iteration two reads buffer[2]; iteration three writes buffer[2]. */
    unsigned char *source = buffer + 1;
    unsigned char *destination = buffer;
    size_t count = COPY_BYTES;
    __asm__ volatile("rep movsb"
                     : "+S"(source), "+D"(destination), "+c"(count)
                     :
                     : "memory");

    write(STDOUT_FILENO, "done\n", DONE_BYTES);
    return 0;
}
```

For R9, use a static program that writes a begin marker, performs 10,000,000
volatile `sum += i` operations, then writes `done`. Record with `--clock exits`.
Start GDB at the done event, select the process thread, issue `reverse-stepi`,
and send Ctrl-C. Capture the remote protocol to distinguish an interrupt actually
sent by GDB from a terminal-delivery problem. The original source and log are
`/tmp/rewind-review-root/src/slow.c` and
`/tmp/rewind-review-reverse-interrupt.log`.

For R10, in a subprocess: create a 4096-byte temporary file, call the safe
`Mapped::file(&file, 4096)`, call safe `file.set_len(0)`, then read
`mapped.bytes()[0]`. The original probe returned signal 7 (SIGBUS). Disable core
dumps for the subprocess. The original source is `/tmp/rewind-review-mapping.rs`.

Additional session evidence, useful while the scratch directory remains:

| File under /tmp                       | Contents                                    |
| ------------------------------------- | ------------------------------------------- |
| rewind-review-latest-pair.json        | R1 at 924b1a7b                              |
| rewind-review-pair-high.json          | Second field found when watched alone       |
| rewind-review-latest-rep.json         | R2 at 924b1a7b                              |
| rewind-review-latest-cap.txt          | R3 incorrect no-write claim                 |
| rewind-review-stress.err              | All 20,000 writes processed with max=1      |
| rewind-review-no-thp-head.err         | Branch abort after refused huge-page advice |
| rewind-review-no-thp-main.out         | Main succeeds under the same filter         |
| rewind-review-restore-times.json      | Six large-guest timing trials               |
| rewind-review-small-times.json        | Six small-run timing trials                 |
| rewind-review-restore-memory.json     | Separate RSS/PSS/anonymous memory samples   |
| rewind-review-unit-tests.log          | Workspace test results                      |
| rewind-review-app-tests.log           | App test results                            |
| rewind-review-integration.log         | Initial five KVM checks                     |
| rewind-review-pinned-debug-checks.log | Four further pinned KVM checks              |
| rewind-review-latest-build.log        | Pinned 924b1a7b build and blame check       |

Keep repairs separate from performance experiments. For each completed finding,
record the regression that failed before the fix and passed afterward. For each
accepted optimization, retain the benchmark command, workload/revision, timings,
CPU, memory, and disk delta beside the result. A faster wrong history fails the
acceptance criteria.
