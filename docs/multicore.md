# Deterministic multicore builds

Start with independent build actions in separate single-vCPU Rewind VMs.
Let Nix run the actions concurrently on physical cores. General SMP is a
larger project requiring deterministic shared-memory execution as well as
precise preemption. No implementation or Rewind multicore performance
measurements were made during this investigation.

## Current constraints

[Machine](../crates/rewind-vmm/src/lib.rs) owns one `VcpuFd` and creates
CPU 0. The [guest kernel](../guest/linux/config) disables SMP; the Rewind
platform requires `!SMP` and protects shared record buffers by disabling
interrupts on the sole CPU. `cores` in [nix/lib.nix](../nix/lib.nix) changes
the reported CPU count and `NIX_BUILD_CORES`, while execution stays on one
vCPU.

Sequential exits give devices and trace records a defined order. The
[clock](../crates/rewind-vmm/src/pv.rs) has one timer, and
[snapshots](../crates/rewind-vmm/src/snapshot.rs) capture one CPU plus the
global device state. Debugging and restoration use the same global steps.

The counter skid described in [preemption.md](preemption.md) is a problem
with stopping at an exact point. Even with exact preemption, concurrent
CPUs can observe different shared-memory access orders between exits.
A race-free program can append results in different orders when
workers compete for a mutex. Ordering device exits cannot undo differences
already visible to guest code. Recording one such execution would help
replay that run; two fresh builds could still choose different executions.

## First prototype: isolated actions

Use one real C/C++ package with enough independent translation units:

1. Express configuration, generated headers, compilations, linking and
   installation as an explicit dependency graph. Initially give each
   compilation the complete immutable source tree and declared generated
   headers. Discovering minimal input sets can wait.
2. Make each action a Nix derivation wrapped with `inRewind`. Pin the
   runtime, toolchain, machine parameters, paths and seed. Give each action
   private scratch space and declared outputs. Keep the guest's reported
   CPU count fixed when changing the number of host workers.
3. Let Nix schedule independent derivations concurrently. Specify link
   input order explicitly, and collect logs and failures by stable action
   identity. Host completion order must not become an action's input.

This preserves each action's existing repeatability, replay, snapshots and
debugging. Immutable dependencies compose those actions into repeatable
final outputs. The new recipe may differ from the original monolithic
recipe in paths, process identities and logical timing; identical bytes
between the two recipes are not promised.

Debugging has action-local traces and steps. Sibling actions cannot
communicate through shared memory, pipes, writable files or build daemons.
Applying `onRewind` to an unchanged monolithic derivation still uses one
vCPU. Automatically splitting arbitrary `make` invocations would require
handling hidden dependencies, shared caches and completion-sensitive
scheduling.

Independent C/C++ compilations are natural actions. Separate Rust crates
and Go packages offer similar boundaries, but build scripts and generated
inputs need integration. A large compiler or linker process remains a
bottleneck: Rust's [codegen units] run LLVM work inside `rustc`, and Go's
[`-p` option] controls parallel build commands. Neither becomes parallel
inside one Rewind VM through this design. LLVM's [distributed ThinLTO]
interface exposes backend commands and their inputs/outputs to a
distributor, which could later launch Rewind actions; the final link still
has to complete separately. Toolchain support must be checked.

Initial work belongs in Nix recipes/helpers and integration tests. A later
manifest and viewer could connect the action graph to individual runs.
CPU creation, the guest scheduler and snapshot formats can stay as they
are. VM startup, input preparation, output extraction and memory use must
be included when evaluating the prototype.

## Validation gates

- Fresh builds with one, two and four physical workers must produce
  identical action outputs and final output hashes. Repeat under host
  contention and deliberately reordered completion.
- Compare logical traces for each action and exercise existing replay,
  seek and fork behavior. Host timing metadata is outside the comparison.
- Missing dependencies and conflicting output ownership must fail
  explicitly. Collected logs and failures must have a stable order.
- Demonstrate simultaneous CPU use and lower complete build time than
  the same action graph run serially. Measure compilation and linking
  separately so a serial critical path is visible.

## General SMP routes

Published systems establish feasibility, with different compatibility
costs:

- [CoreDet] instruments potentially shared memory accesses and uses
  deterministic rounds with ownership or buffered commits. The compiler
  and runtime approach assumes memory safety and generally serializes
  uninstrumented library code. Arbitrary binaries, assembly and generated
  code need wider coverage; guest-kernel ordering remains separate work.
- [dOS] runs unmodified binaries using per-thread page ownership, parallel
  private execution and serialized communication. System calls, futexes,
  pipes and filesystem behavior also need deterministic treatment. Its
  2010 Linux `make -j` experiment with filesystem determinism was 8.2 times
  slower than native at eight workers; increasing two workers to eight
  gave a 1.1 times speedup. Those old results identify a possible syscall
  bottleneck. Rewind's performance would need to be measured separately.
- [Determinator] uses private memory/filesystem state and explicit merges.
  Independent compiler outputs fit that model, but its Unix emulation
  changes PID and `wait()` semantics and rejects conflicting writes.
  Preserving arbitrary existing Linux process behavior would require
  more work than adopting those primitives.

A Rewind SMP design would need guest SMP boot and interrupt support,
per-CPU platform buffers, deterministic memory ownership or commits, and
ordered kernel interactions. VMM work would include multiple vCPU threads,
logical device ordering, consistent snapshots of all CPUs, and CPU-aware
tracing and debugging. A conventional kernel lock does not supply a
deterministic acquisition order. Serializing kernel execution also leaves
user-space memory races to solve.

Stock [KVM] can execute concurrent workers, but dirty-page logging does
not describe read dependencies or shared-memory access order. Ownership
could be enforced through guest page tables; a custom KVM is not
automatically required. Software emulation helps implement exact
instruction budgets, but concurrent execution still needs a memory
protocol. QEMU's [instruction counting] is incompatible with multithreaded
TCG. Use isolated-action measurements to decide whether a workload warrants
this larger redesign.

[codegen units]: https://doc.rust-lang.org/stable/rustc/codegen-options/#codegen-units
[`-p` option]: https://pkg.go.dev/cmd/go
[distributed ThinLTO]: https://llvm.org/docs/DTLTO.html
[CoreDet]: https://homes.cs.washington.edu/~djg/papers/asplos10-coredet.pdf
[dOS]: https://ready.cs.washington.edu/~luisceze/publications/osdi10-dos.pdf
[Determinator]: https://dedis.cs.yale.edu/2010/det/papers/osdi10.pdf
[KVM]: https://docs.kernel.org/virt/kvm/api.html
[instruction counting]: https://www.qemu.org/docs/master/devel/tcg-icount.html
