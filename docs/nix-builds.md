# Nix build reference

The flake exports `lib.x86_64-linux.inRewind` and
`lib.x86_64-linux.onRewind`. Both take machine settings followed by a
derivation. Start with the [deterministic derivations
tutorial](tutorial-derivations.md) for an `inRewind` example.

## Machine settings

Both functions accept the same settings:

| Argument   | Default       | Meaning                                                        |
| ---------- | ------------- | -------------------------------------------------------------- |
| `epoch`    | Required      | Wall clock at boot, in seconds since the Unix epoch            |
| `seed`     | `0`           | Seed of the kernel's randomness                                |
| `schedule` | `0`           | Thread schedule; 0 is the unperturbed schedule                 |
| `clock`    | `"branches"`  | `"branches"` counts guest work; `"exits"` advances at VM exits |
| `cpu`      | `"v3-compat"` | `"v3"`, or v3 without ERMS for hosts that lack that feature    |
| `vendor`   | `"amd"`       | CPU vendor of the builder: `"amd"` or `"intel"`                |
| `cores`    | `1`           | Reported CPU count and `NIX_BUILD_CORES`                       |
| `mem`      | `1024`        | Guest memory in MiB, including build files                     |

Counter time requires an exact branch counter. On AMD, the NixOS option
`programs.rewind.amdBranchCounterWorkaround = true` enables the counter
and makes its marker available in Nix's sandbox. On another distribution,
run `sudo rewind pmu enable` and add `extra-sandbox-paths = /run/rewind?`
to `nix.conf`. [Time inside the VM](pmu.md) explains the clock models.

The machine settings are stored in `rewindFlags`, with defaults written
out. `rewind build` refuses a missing epoch, an automatic clock, a host
CPU model, or an unspecified CPU vendor. The vendor comes from `--vendor`
or the derivation's `x86_64-rewind-amd` or `x86_64-rewind-intel` system.

## Runtime identity

Both functions add `rewindRuntime`, the store path of the Rewind package.
Nix includes that package in the derivation's inputs. The package pins the
VM executable, kernel and initrd; upgrading it changes the derivation and
its output paths.

`rewind build` checks that the running package matches `rewindRuntime`
before starting the VM. It loads the kernel and initrd from that package,
so `REWIND_KERNEL` and `REWIND_INITRD` overrides cannot change a pinned
build. Those overrides still work for interactive commands such as
`rewind run`.

## The builder wrapper

`inRewind machine drv` replaces `drv`'s builder with a script that starts
Rewind inside Nix's sandbox. The derivation remains `x86_64-linux` and asks
for the `kvm` and `rewind-<vendor>` system features. Rewind also checks the
host CPU's vendor before running the build.

The script describes the original builder and its input closure to
`rewind build`. The guest store contains that closure. Host-specific
`sandbox-paths` stay outside the guest. The build directory is `/build`,
so Nix's sandbox must be enabled.

Multiple outputs and `__structuredAttrs` are supported. Rewind copies each
output directly from the guest's file-tree stream into the path Nix
assigned. Exporting an archive is a separate operation. Fixed-output
derivations are refused by `inRewind` because the VM has no network.

## The external builder

`onRewind machine drv` keeps the original builder and changes `system` to
`x86_64-rewind-<vendor>`. Nix hands the build to the program configured in
its experimental
[external-builders](https://nix.dev/manual/nix/latest/command-ref/conf-file.html#conf-external-builders)
setting. Nix does not put its usual sandbox around the external builder.

```nix
rewind.lib.x86_64-linux.onRewind {
  epoch = 1790985600;
  clock = "exits";
  vendor = "amd";
} pkgs.hello
```

For a daemon, configure `nix.conf` with the same Rewind package used by the
client's flake. Replace the example program path with that package's actual
store path:

```
extra-experimental-features = external-builders
external-builders = [{"systems":["x86_64-rewind-amd"],"program":"/nix/store/<hash>-rewind/bin/rewind","args":["build","--external-builder"]}]
```

The daemon must be configured by its administrator; a client cannot set
`external-builders` for it. If the configured package differs from the
recipe's `rewindRuntime`, the build fails with both paths in the error.
Update the configuration to the package the recipe names. One configured
runtime serves recipes pinned to that runtime.

Use `inRewind` with a stock daemon. `onRewind` requires the experimental
interface on every builder, and nixpkgs does not treat the Rewind systems
as native platforms.

## Inspect and export a recorded build

Nix removes an `inRewind` build's recording with its sandbox. `rewind nix`
records a separate run that you can inspect:

```console
$ rewind nix .#my-package --epoch 1790985600 --cpu v3-compat --clock exits
```

This builds the original derivation. Its output paths and environment
differ from the wrapped derivation's, so a failure in `inRewind` may need
more work to reproduce here.

For a retained run, `rewind files RUN STEP PATH` lists a directory and
`rewind cat RUN STEP PATH --archive tar` streams its tree. For example,
with the run id, step and output path assigned to shell variables:

```sh
set -o pipefail
mkdir -p ./extracted
rewind cat "$run" "$step" "$output" --archive tar | tar -xf - -C ./extracted
```

The tar contains the path under its last component, with the VM's
permissions, numeric owners and modification times. Links are preserved; devices, sockets and pipes are omitted with a
warning. Check Rewind's exit status: tar can accept a stream cut short at
an entry boundary, which is why the pipeline uses `pipefail`.
