# Rewind VM

Deterministic Linux virtual machines you can scrub, rewind and fork.

```console
$ rewind check github:fzakaria/rewind#mylib
schedule   0: exited:0             5115 steps  aa30ea54dc47  run efd74e9a5a5098f5
schedule   1: exited:2             4013 steps    run 2ae737e23ef6e78c

schedule 1 ends differently; narrowing the steps it perturbs
perturbing only steps 3779..3980 still ends differently

passing: run efd74e9a5a5098f5
failing: run 27586aa31b6292bf

where ./tests/test_pool_shutdown first behaves differently:
  ...
  right       3811   174/175   write(1, "job 17 done: 43360\n")
  right       3812   174/176   write(1, "job 16 done: 5986\n")
  right       3815   174/176   SIGSEGV code=1 addr=0x108

$ rewind replay 27586aa3
identical: 1608 events over 3848 steps
```

Rewind VM runs a Nix build, or any command in a root filesystem such as a
Docker export, inside a KVM virtual machine whose every run is a function of
its inputs. The same inputs give the same run, at the same steps, every time.
A failure you found once is a failure you have, to replay, scrub through step
by step, and fork with a different thread interleaving from any point.

The guest kernel carries a small hypervisor platform: time is virtual and
moves only when the guest exits to the monitor, and interrupts arrive only at
those exits. A GNU hello build from nixpkgs runs at native speed, and its
output is bit for bit the one Nix builds on the host.

**Documentation:** [Nix tutorial](./docs/tutorial-nix.md) ·
[Container tutorial](./docs/tutorial-container.md) ·
[Design](./docs/design.md)

## Status

Early, and working: runs, Nix builds, containers, schedule search, forks,
replay, keyframes and the desktop app all work on an x86_64 Linux laptop with
KVM. Not yet published: no release, and the flake is not on GitHub yet.
[Design](./docs/design.md#limits) lists what the approach cannot do, and
[the roadmap](./docs/design.md#roadmap) what comes next.

## Quickstart

```console
# build a derivation in the deterministic VM
$ nix run . -- nix nixpkgs#hello

# find a failing interleaving of the example's tests
$ nix run . -- check .#mylib

# a command in a Docker export
$ nix run . -- run --root mylib.tar --cwd /src -- make check

# list runs, and look inside one
$ nix run . -- ls
$ nix run . -- log <run> --steps
$ nix run . -- ps <run> --at <step>

# the desktop app, the landing page, the tarball for people without Nix
$ nix run .#app -- ~/.local/share/rewind/runs/<run>
$ nix run .#serve
$ nix build .#release

# work on it
$ nix develop
$ cargo test --workspace
```

`/dev/kvm` must be readable and writable by you.

## Layout

- `guest/linux/`: the kernel patch and config fragment for the guest.
- `crates/rewind-vmm/`: the monitor: KVM, boot, exits, time, keyframes.
- `crates/rewind-init/`: the guest's PID 1, a workspace of its own.
- `crates/rewind-trace/`: reading and querying run traces.
- `crates/rewind-store/`: the content-addressed page store.
- `crates/rewind-core/`: input images, Nix derivations, runs, seeking.
- `crates/rewind/`: the `rewind` command.
- `crates/rewind-app/`: the desktop app, a workspace of its own.
- `examples/mylib/`: the flaky thread pool the tutorials use.
- `nix/`: one file per derivation; `flake.nix` only wires them together.
- `site/`: rewindvm.dev.
- `docs/`: tutorials and design.
- `tools/`: the prose check and the local site server.

## License

The engine is MIT, please see [LICENSE](LICENSE). The guest kernel patch is
GPL-2.0, as Linux is. The desktop app under `crates/rewind-app` is
proprietary to Lunch Time Surf LLC; see its own LICENSE.
