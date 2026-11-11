---
date: 2026-10-09
kind: audit
revision: 376da929
---

# Code audit of 0.6.0

The first repairs should protect recordings and make replay verification fail
closed. Concurrent writers can publish wrong page offsets, importing an archive
can replace valid inputs, and seeking can accept a replay that omitted an expected
event. These affect the evidence a customer would rely on to diagnose a bug.

This is a repair backlog, not an implementation. No production code was changed.

## Scope and evidence

Reviewed revision `376da929`, version 0.6.0, from a clean working copy. The review
covered recording and replay, keyframes, the page store and garbage collection,
imports and exports, trace interpretation, file caches, schedule narrowing, CPU
configuration and PMU use. It also sampled the desktop engine, guest patch, Nix
packaging and CI. This was a targeted review, not an exhaustive review of every
line or a desktop usability test.

Validation on nyx:

| Check                                                                                         | Result                                    |
| --------------------------------------------------------------------------------------------- | ----------------------------------------- |
| `nix develop -c cargo test --workspace --locked`                                              | Passed                                    |
| `nix develop .#app -c cargo test --manifest-path crates/rewind-app/Cargo.toml --locked --lib` | 246 passed                                |
| `nix develop -c cargo test --manifest-path crates/rewind-init/Cargo.toml --locked`            | 12 passed                                 |
| Current CLI build, small KVM recording, boot replay                                           | Passed                                    |
| Targeted library and CLI probes below                                                         | Found failures outside the existing tests |

The KVM probes used a BusyBox guest, `--clock exits`, isolated `REWIND_HOME`
directories and a separate app cache under `/tmp/rewind-audit.iMkIxh`. The run
printed two lines, wrote a file and terminated at step 319. Existing user
recordings were not modified. Scratch probes are session artifacts; the procedures
below are the durable record of how to reproduce the findings.

The full flake/integration matrix, an Intel host, PMU saturation and power-loss
tests were not run. "Reproduced" means the current implementation returned the
reported result. "Code path" means the defect follows from inspected code but the
trigger was not exercised end to end.

## Repair order

P1 means fix before expanding paid replay or concurrent CI use. P2 means a
correctness repair for the normal backlog. These priorities describe impact, not
the probability of encountering a bug.

| ID  | Priority | Finding                                                   | Evidence                         |
| --- | -------- | --------------------------------------------------------- | -------------------------------- |
| A1  | P1       | Page pack offsets can be captured before ownership        | Code path                        |
| A2  | P1       | Removal ignores an active execution of a finished run     | Lock probe                       |
| A3  | P1       | Import replaces valid inputs without validating identity  | KVM/CLI reproduction             |
| A4  | P1       | Seek accepts missing events and an unreached target       | KVM/library reproduction         |
| A5  | P1       | Exact counters can be multiplexed without detection       | Code path and perf API           |
| A6  | P1       | Run identity omits guest-visible host differences         | Code path                        |
| A7  | P2       | Concurrent replays share temporary filenames              | KVM/CLI reproduction             |
| A8  | P2       | URL downloads collide on filename and length              | Local HTTP reproduction          |
| A9  | P2       | File contents become cacheable while another writer lives | Library reproduction             |
| A10 | P2       | Reused thread IDs corrupt thread lifetimes                | Library reproduction             |
| A11 | P2       | Persistence ordering omits directory synchronization      | Code path                        |
| A12 | P2       | Narrowing claims a minimum it does not establish          | Synthetic predicate reproduction |

## A1. Acquire the pack lock before reading its length

Location: [Store::claim_pack](../../crates/rewind-store/src/lib.rs#L544), with offsets
used by `Store::put` in the same file.

An existing pack's length is read before `try_lock()`. Writer A can append and
release the pack between writer B's `metadata()` and successful lock acquisition.
B writes at the actual end because the file uses append mode, but publishes an
index entry using the old `pack.len`. Reading the page later fails decompression
or hash verification. The content check detects damage; it does not recover the
page's correct location.

The new-pack branch has a related race. `create_new()` publishes the filename
before the creator locks the file. Another writer can open, lock and append to
that file first. The creator eventually acquires the lock and assumes length zero.

Repair: acquire the lock, then read and validate the current length in both
branches. Check the size limit under that lock. Keep pack ownership explicit in
the type that writes and publishes offsets.

Acceptance test: force a writer to append between another writer's open/stat and
lock acquisition. Cover the create-before-lock case too. After both writers exit,
reopen the store and verify every returned hash, then repeat after GC. Barriers or
a test filesystem abstraction will make this more reliable than a timing loop.

## A2. Protect removal throughout the run lifecycle

Locations: [prune::member](../../crates/rewind-core/src/prune.rs#L17),
[delete](../../crates/rewind-core/src/prune.rs#L102),
[execute_manifest](../../crates/rewind-core/src/run.rs#L574) and
[HomeHold](../../crates/rewind/src/main.rs#L246).

`member()` reports `executing: !finished && run.executing()`. Re-execution keeps
the previous finished manifest visible until the replacement finishes. A run can
therefore be both finished on disk and actively executing, while removal reports
that the run is idle.

Reproduction: hold an exclusive `flock` on a completed run's `executing.lock`,
then invoke `rewind remove audit --dry-run`. The command returned success and
`would remove e1a9cdcf19d754db`. This exercised the actual planning path with a
held lock; the probe did not delete a running VM's directory.

There is also a check/use gap. Removal lists dependencies once, then calls
`remove_dir_all` without owning the execution lock. Another process can start a
run or fork between planning and deletion. `remove` and `prune` do not take the
home's in-use hold.

Repair: consult execution state independently of the manifest outcome. Define a
transaction that prevents new executions, readers and dependent forks while
validating and removing the set. A lock inside a directory that can itself be
deleted needs care: another process must not recreate that path and acquire a
different lock inode.

Acceptance test: completed run plus held execution lock must refuse removal.
Use barriers to race removal against re-execution and fork creation; the result
must be a refusal or a complete operation with valid remaining references.

## A3. Validate imported inputs before replacing a run

Location: [import placement](../../crates/rewind-core/src/export.rs#L234).

For an existing finished run, import checks that the incoming trace hashes to the
local trace hash. The importer then renames incoming kernel, initrd and image
files over existing inputs. It does not establish that those bytes and the
incoming spec produce the claimed run ID. Keeping the trace unchanged bypasses
the conflict check.

Reproduction:

1. Export a working run with `--replayable` and import it into an empty home.
2. Repack the archive with the same manifest, trace and ID, replacing only
   `inputs/kernel` with `not a kernel\n`.
3. Import the changed archive into that home.

Both imports returned `replayable: true`. The second replaced the local kernel;
boot replay then failed with `kernel image is too small to be a bzImage`.

Repair: persist and validate input digests and the canonical spec identity in
staging. Validate the declared trace digest, keyframe references and format
compatibility there too. Publish a complete generation only after validation;
an error must leave the old generation usable. Treat an archive with missing
inputs as view-only unless the required inputs are actually available and
verified. A view-only import must not replace working local spec paths with paths
from another machine.

Acceptance test: the altered-kernel archive is rejected and the original still
replays. Repeat for image, initrd and changed spec fields with an unchanged trace.
Inject a failure between each placement operation and verify the old generation.

## A4. Complete replay verification before returning a machine

Locations: [Checked](../../crates/rewind-core/src/run.rs#L245) and
[Run::machine_at](../../crates/rewind-core/src/run.rs#L930).

`Checked` compares records only when the replay emits one. It never checks
whether expected records remain at or before the requested step. `machine_at`
also returns `Ok(machine)` when the VM stops before the requested target; the
`reached` check only controls whether to save a keyframe.

Reproduction: copy the small recording and append an extra expected trace record
at its terminal step, 319. The real last event was at 318. Asking `machine_at`
for step 319 returned success despite never producing the appended event. Asking
for step 419 also returned success with a machine at step 319.

The modified trace is a controlled way to exercise an omitted event, not evidence
that this workload naturally diverges. The same missing finalization check matters
when a different engine or guest path emits a shorter matching prefix.

Repair: finalize the checker against the requested boundary and reject unconsumed
expected records through that boundary. Return an explicit reached/stopped result
or reject an unreached target. Observer cancellation needs its own outcome rather
than being mistaken for arrival. Define whether seeking to the terminal step is
valid and apply that rule consistently.

Acceptance test: omitted final event, early guest termination, cancellation, exact
terminal step, and a future event beyond the target that should remain unchecked.
Exercise boot and keyframe restoration paths.

## A5. Fail closed when exact PMU counting is unavailable

Location: [open_event](../../crates/rewind-vmm/src/pmu.rs#L225) and the counter read
path in the same file.

The perf event is neither pinned nor configured to return enabled/running time.
Reads consume the raw count alone. Under PMU contention, Linux can multiplex
events; a raw count then covers only part of execution. An estimated scaled count
would also be unsuitable for deterministic guest time. The
[perf API](https://man7.org/linux/man-pages/man2/perf_event_open.2.html) documents
both multiplexing accounting and pinned-event failure behavior.

Repair: require the hardware resources needed for exact counting, detect periods
without coverage, and reject the run when the requirement fails. Validate the
accounting around guest entry/exit on each supported CPU. Do not silently switch
an existing recording to an approximate clock. Use an owned file descriptor
immediately after opening so later setup errors cannot leak the raw descriptor.

Acceptance test: record and replay with competing perf events and host load. Every
trial must either produce the same trace or refuse with a specific capability
error. Include migration between supported cores. Saturation was not reproduced
during this audit.

The Intel event-selection problem is already described in
[preemption.md](../preemption.md#intel): a single encoding is used across CPU
families that need different events. Treat CPU model/core-type selection and
affinity as part of this work, not as a newly discovered issue.

## A6. Make execution identity include the actual CPU contract

Locations: [Spec](../../crates/rewind-trace/src/manifest.rs#L161),
[SpecExt::id](../../crates/rewind-core/src/run.rs#L91) and
[baseline CPUID](../../crates/rewind-vmm/src/cpu.rs#L250).

The spec records a CPU enum, but the V3 CPU exposes the host vendor and uses a
vendor-dependent signature. `CpuModel::Host` exposes more host-dependent features.
Those guest-visible differences are absent from the identity. In particular,
`--clock exits` does not carry the AMD/Intel distinction that a vendor-specific
branch-clock enum can incidentally provide.

The same hashed inputs can therefore name different guest CPU contracts. Kernel
boot paths and user-space dispatch can differ while sharing a run ID. Cross-host
execution was not tested. This is a missing identity component visible directly
in the spec and CPUID construction.

Repair: record a versioned execution compatibility fingerprint, including the
guest-visible CPUID contract and counter semantics. Validate the fingerprint
before replay or keyframe restoration. Separate recording provenance, such as a
build version, from the explicit contract that defines compatible execution.
Persist any preemption tuning that affects results; the margin issue is already
documented in `preemption.md`.

Acceptance test: synthetic AMD/Intel CPUID fixtures and differing Host features
must produce distinct compatibility identities or explicit refusal. Then validate
actual replay on the supported hardware matrix.

## A7. Give each replay its own temporary trace

Locations: [Run::replay](../../crates/rewind-core/src/run.rs#L768) and
[Run::replay_from](../../crates/rewind-core/src/run.rs#L1054).

Replays use fixed `replay.bin.tmp` and `replay-from.bin.tmp` paths without mutual
exclusion. Concurrent clients can truncate, read or remove each other's trace.

Reproduction: launch two `rewind replay audit --json` processes against the same
completed recording. One reported an identical replay. The other failed with
`No such file or directory (os error 2)`.

Repair: use per-operation temporary files and owned handles, with cleanup on all
paths. Replay should remain a read operation that can run concurrently. Reuse the
existing unique-temporary-file facility instead of another naming convention.

Acceptance test: overlapping boot replays and overlapping keyframe replays must
both succeed and compare their own traces. Include one canceled client.

## A8. Identify downloads by origin and content

Location: [archive::download](../../crates/rewind-app/src/archive.rs#L97).

The download cache keys by URL basename and treats equal `Content-Length` as a
cache hit. The origin, path and query do not establish identity. A changed response
at the same URL can also have the same size.

Reproduction: a local server returned `AAAA` at `/a/run.rwd` and `BBBB` at
`/b/run.rwd`, both four bytes. The second call returned the first cached file and
the bytes `AAAA`. Arbitrary bytes were enough to test the download layer; archive
parsing was not involved.

Repair: hash the complete URL for the lookup key, use HTTP validators or a known
content digest, and publish downloaded content under an immutable identity.
Concurrent requests must not replace a file another caller is about to import.

Acceptance test: equal-size responses with matching basenames at different URLs;
same URL with new equal-size content; concurrent downloads; interrupted response.

## A9. Do not cache a file while any possible writer remains

Location: [contents::version](../../crates/rewind-trace/src/contents.rs#L35), consumed
by the source cache and desktop file viewer.

The algorithm finds the last relevant event. For a writable open, it checks only
whether that opener has exited. An earlier opener can still hold and write the
same file. A rename also returns a stable version even when a writer retains an
open descriptor.

Reproduction with synthetic events: PID 100 opens the file at step 10, PID 200
opens it at 20, and PID 200 exits at 30. At step 40 the function returns
`Since(20)` although PID 100 is still alive. The source cache can serve those bytes
at later steps after PID 100 writes again.

Repair: return `Changing` unless stability is established for every possible
writer. Conservatively account for descriptor inheritance and rename. Longer
term, use file identity and successful mutation events instead of treating path
strings and open attempts as complete file provenance. Missing observations must
reduce caching, rather than establish immutability.

Acceptance test: overlapping writers, inherited descriptors, rename while open,
and aliases. Read the file at two steps with different contents and verify that
the second lookup cannot reuse the first result. The existing comment excluding
inherited writers is a workload assumption that needs an enforceable boundary.

## A10. Match thread exits to the live incarnation

Location: [Trace::processes](../../crates/rewind-trace/src/lib.rs#L281).

A thread exit updates the first thread entry with a matching TID. Linux can reuse
the TID after exit, so the second incarnation overwrites the first incarnation's
end and never receives an end of its own.

Reproduction: create TID 101 at 10, exit at 20, create 101 again at 30 and exit at 40. The lifetimes become `(101, 10, Some(40))` and `(101, 30, None)`. The expected
ends are 20 and 40. This can mislead the thread timeline and thread-at-step lookup.

Repair: resolve the newest live incarnation, or give thread lifetimes explicit
identity as the process history already attempts to do for reused PIDs.

Acceptance test: the sequence above, plus thread lookup between both lifetimes
and after the second exit. Review non-leader exec against the same lifetime model.

## A11. Specify and test crash durability across files

Locations: [image::write_atomic](../../crates/rewind-core/src/image.rs#L177),
[Store::sync](../../crates/rewind-store/src/lib.rs#L681) and
[Collector::move_pages](../../crates/rewind-store/src/lib.rs#L875).

`write_atomic` syncs file contents and renames but does not sync the containing
directory. `image::place` does sync the directory, so the two helpers provide
different persistence guarantees. New page packs are synced without syncing the
`packs` directory before indexes can refer to them. GC must also preserve the
new pack names before it discards old packs.

This is a persistence-ordering gap, not a reproduced power-loss incident. Atomic
visibility during normal execution and durability after a crash are separate
properties. The store's promise that `sync()` makes everything durable should
cover both contents and directory entries.

Repair: document the commit order for packs, indexes, keyframes and manifests.
Sync newly created/renamed entries in the relevant directories before publishing
references or deleting the previous copy. Prefer one helper with an explicit
durability contract.

Acceptance test: fault injection at each commit boundary, followed by reopening
and checking every retained run's reachable pages. Test interrupted GC as well as
recording. Add a VM/filesystem crash test for the promised persistence behavior.

## A12. Describe schedule narrowing as a heuristic

Location: [check::narrow](../../crates/rewind-core/src/check.rs#L1).

The comments promise the smallest window, latest start and earliest end. The
search discards intervals based on sampled pass/fail results. Schedule-sensitive
failure is not monotone: adding or removing a preemption can create or hide a
failure, so a passing sample does not rule out another failing region.

A synthetic probe of the actual function uses the range `0..8`, one job, and a
predicate that fails when `(from == 0 && until >= 4)` or
`(from == 6 && until >= 7)`. Narrowing returns `0..4`, although `6..7` also fails.
This disproves the global-minimum/latest-start claim. It does not invalidate the
passing/failing boundary the implementation actually probes.

Repair: call the result a reduced failing window, expose the neighboring result
as evidence, and state the search budget. If stronger reduction is needed, use
non-monotone reduction of explicit scheduling interventions and report the exact
minimality criterion. A locally minimal intervention set is still different from
a globally shortest schedule.

Acceptance test: retain monotone cases and add disjoint failing regions and
fail/pass/fail outcomes. Product copy should make the same claim as the tested
algorithm.

## Code quality and engineering follow-ups

1. Introduce a run-operation layer for locking and publication. Execution,
   import, keyframe additions, removal and GC currently enforce related rules in
   different places. Encode ownership and generation transitions there before
   splitting files for size. A2 and A3 provide concrete tests for the abstraction.
2. Bound archive and snapshot decoding. Existing per-entry checks do not amount
   to an aggregate decompression, allocation or disk budget. Give imports
   configurable limits, cancellation and cleanup; validate snapshot counts and
   reference chains before allocating large structures. This is hardening work,
   not a demonstrated exploit from this audit.
3. Make cancellation and target attainment explicit in replay APIs. A wall-time
   limit used during recording does not automatically bound `replay()` or seeking.
   Add an operation budget and structured stopped/canceled/diverged results, then
   propagate those through the CLI and desktop engine. Include the known
   computation-without-exits workload from `preemption.md`.
4. Test invariants across components. Useful properties include "every published
   keyframe resolves", "a rejected import leaves the prior run replayable", and
   "concurrent read operations do not mutate each other's state". Add these at
   the core/store boundary, alongside the existing unit tests. Root-workspace
   tests alone do not run the separate app and init workspaces.
5. Keep trace guarantees beside the event schema. Distinguish an attempted
   syscall from successful mutation, a path from an inode, and a numeric PID from
   a process lifetime. Consumers should see observation gaps instead of inferring
   stronger guarantees, as the contents cache does today.
6. Maintain a measured replay compatibility matrix. Record host CPU/core type,
   guest CPU contract, PMU mode, engine format and guest patch version in test
   artifacts. Include boot and snapshot replay under host load. A capability
   probe that works once is weaker evidence than repeated workload replay.

## Suggested patch sequence

Start with failing regression tests for A1, A2, A3, A4 and A7. Fix the local bugs
before the larger lifecycle refactor so the tests define the required behavior.
Address A5 and A6 before promoting wider hardware compatibility or parallel CI.
Then repair cache and lifetime interpretation, correct narrowing claims, and
complete the crash-recovery work.
