# Rewind VM

Deterministic Linux virtual machines you can scrub, rewind and fork.

Rewind runs a workload, a Nix build or any command in a root filesystem, inside
a KVM virtual machine that behaves the same way every time. A run is a
function of its inputs, so there is nothing to record: every step of it can be
reached again, inspected with a debugger, and forked with a different seed.

## Status

Early. The engine is being built; see [docs/design.md](docs/design.md).

## Layout

- `nix/`: one file per derivation; `flake.nix` only wires them together.
- `tools/`: the prose check and the local site server.

## License

The engine is MIT, please see [LICENSE](LICENSE). The desktop app under
`crates/rewind-app` is proprietary to Lunch Time Surf LLC; see its own
LICENSE.
