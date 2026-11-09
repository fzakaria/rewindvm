# Tutorial: deterministic derivations

<!-- let epoch = 1790985600 -->
<!-- let repo = github:fzakaria/rewindvm -->
<!-- let wrapped = with builtins.getFlake "{{repo}}"; lib.x86_64-linux.inRewind { epoch = {{epoch}}; clock = "exits"; vendor = "amd"; } packages.x86_64-linux.stamp -->

Nix hashes a build's declared inputs. A builder can still read the current
time or generate a random UUID, so rebuilding that derivation can produce
different bytes. `inRewind` runs the builder in a VM and puts the VM's
settings and runtime in the derivation. The build repeats with those inputs
fixed.

The [reproducible builds tutorial](tutorial-repro.md) varies the environment
to find dependencies you can remove, such as an unsorted directory listing.
Here we pin the environment so an unchanged program repeats. Sorting the
listing makes the build agree across directory orders; pinning the VM picks
one order. Both are useful. A race still needs fixing even if one pinned
schedule happens to pass.

## Set up the builder

Install Rewind as the [Nix tutorial](tutorial-nix.md) describes. Nix needs
sandboxing enabled and the `kvm` system feature, with `/dev/kvm` available
to builds. It also needs a feature naming the builder's CPU vendor. On NixOS:

```nix
programs.rewind.systemFeature = "amd";
```

On another distribution, add `extra-system-features = kvm rewind-amd` to
`nix.conf`. Use `intel` and `rewind-intel` on Intel. The examples below were
recorded on AMD and use `clock = "exits"`, which needs no branch counter.
The [reference](nix-builds.md) explains the clock options and machine settings.

## A build that stamps itself

[stamp](https://github.com/fzakaria/rewindvm/tree/main/examples) compiles a
UUID and a timestamp into its executable. Each ordinary build reads a new
UUID and the current time:

<!-- run: nix build --no-link {{repo}}#stamp -->

```console run name=host elide keep=^(build\s|checking\s|error:)
$ nix run {{repo}}#stamp
$ nix build --no-link {{repo}}#stamp --rebuild
```

<!-- assert: grep -q 'may not be deterministic' {{out:host}} -->

`--rebuild` builds the same derivation again and compares the result with
the stored output. The bytes differ.

## Wrap the derivation

Save this as `stamp.nix`, changing `vendor` to `"intel"` on an Intel builder:

```nix
let
  rewind = builtins.getFlake "github:fzakaria/rewindvm";
  inherit (rewind.lib.x86_64-linux) inRewind;
in
inRewind {
  epoch = 1790985600;
  clock = "exits";
  vendor = "amd";
} rewind.packages.x86_64-linux.stamp
```

`inRewind` takes the machine settings and a derivation. Nix runs that
derivation's builder inside Rewind and receives its outputs as usual. The
`epoch` sets the VM's clock at boot to midnight on 2026-10-03.

<!-- run: printf '%s\n' '{{wrapped}}' > stamp.nix -->
<!-- run: nix build --no-link --impure -f stamp.nix -->

```console run name=first keep=^(/nix/store|build\s)
$ nix build --no-link --print-out-paths --impure -f stamp.nix
$ $(nix build --no-link --print-out-paths --impure -f stamp.nix)/bin/stamp
```

<!-- capture first_path: (?m)^(/nix/store/.*-stamp-0.1.0)$ -->
<!-- assert: tail -n 1 {{out:first}} | grep -qx 'build [0-9a-f-]* at 2026-10-03T00:00:00Z' -->

Now rebuild it:

```console run name=again elide keep=^(checking|stamp>\srewind:\srun|stamp>\sgcc)
$ nix build -L --no-link --impure -f stamp.nix --rebuild
```

<!-- assert: grep -q 'rewind: run .* exited:0' {{out:again}} -->
<!-- assert: ! grep -q 'may not be deterministic' {{out:again}} -->

`--rebuild` passes. The program still reads the clock and the kernel's
random pool, but both builds see the same values. You can replace `stamp`
with your own derivation without changing its builder.

The result's hash covers the machine settings in `rewindFlags` and the
Rewind package in `rewindRuntime`, including its kernel and initrd. Changing
any of them gives Nix a different recipe and output path. In a project,
pin the Rewind flake input in `flake.lock` as you pin nixpkgs.

## Change the clock

Change the epoch to midnight on 2025-06-15:

```console run name=other keep=^(/nix/store|build\s)
$ sed -i 's/1790985600/1749945600/' stamp.nix
$ nix build --no-link --print-out-paths --impure -f stamp.nix
$ $(nix build --no-link --print-out-paths --impure -f stamp.nix)/bin/stamp
```

<!-- assert: tail -n 1 {{out:other}} | grep -qx 'build [0-9a-f-]* at 2025-06-15T00:00:00Z' -->
<!-- capture other_path: (?m)^(/nix/store/.*-stamp-0.1.0)$ -->
<!-- assert: test {{first_path}} != {{other_path}} -->

The new output has its own store path and timestamp. Rebuilding the original
recipe still produces the original timestamp.

## Costs and limits

The VM has one virtual CPU. Build threads take turns on that CPU, so builds
that benefit from many cores will take longer. Each build also packs its
input closure into an image. The build directory and outputs live in guest
memory; increase `mem` for large builds.

Builds have no network. Keep fixed-output derivations, such as source
fetches, outside `inRewind`. AMD and Intel are separate build targets, and
upgrading the pinned Rewind package rebuilds the derivations that use it.

The [Nix build reference](nix-builds.md) covers all machine settings,
exporting files from a recording, and `onRewind`, which uses Nix's
experimental external-builder interface.
