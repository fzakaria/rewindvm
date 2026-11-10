# Tutorial: deterministic derivations

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

```console
$ nix run github:fzakaria/rewindvm#stamp
...
build b2060565-ce20-4c7b-80f9-e19f540dbc78 at 2026-11-09T18:06:35Z

$ nix build --no-link github:fzakaria/rewindvm#stamp --rebuild
...
checking outputs of '/nix/store/...-stamp-0.1.0.drv'...
error: derivation '/nix/store/...-stamp-0.1.0.drv' may not be deterministic: output "/nix/store/...-stamp-0.1.0" differs
```

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

```console
$ nix build --no-link --print-out-paths --impure -f stamp.nix
...
/nix/store/kamhwi6kmqj0y3s7kz46afrw2kah3m6n-stamp-0.1.0

$ $(nix build --no-link --print-out-paths --impure -f stamp.nix)/bin/stamp
...
build 353dda9f-1143-4548-9a60-a961dc181945 at 2026-10-03T00:00:00Z
```

Now rebuild it:

```console
$ nix build -L --no-link --impure -f stamp.nix --rebuild
...
checking outputs of '/nix/store/...-stamp-0.1.0.drv'...
...
stamp> gcc -O1 -Wall -DBUILD_ID='"353dda9f-1143-4548-9a60-a961dc181945"' -DBUILT_AT='"2026-10-03T00:00:00Z"' -o stamp stamp.c
...
stamp> rewind: run f864cf827414c782 exited:0 after 4299 steps, 0.021s virtual, 0.507s wall (poweroff)
```

`--rebuild` passes. The program still reads the clock and the kernel's
random pool, but both builds see the same values. You can replace `stamp`
with your own derivation without changing its builder.

The result's hash covers the machine settings in `rewindFlags` and the
Rewind package in `rewindRuntime`, including its kernel and initrd. Changing
any of them gives Nix a different recipe and output path. In a project,
pin the Rewind flake input in `flake.lock` as you pin nixpkgs.

## Change the clock

Change the epoch to midnight on 2025-06-15:

```console
$ sed -i 's/1790985600/1749945600/' stamp.nix
$ nix build --no-link --print-out-paths --impure -f stamp.nix
...
/nix/store/xww0r7mqd7sxvgqbdfxsjnxh1c752nhs-stamp-0.1.0

$ $(nix build --no-link --print-out-paths --impure -f stamp.nix)/bin/stamp
...
build 3abd3873-04a1-4984-ae0a-7111128006c3 at 2025-06-15T00:00:00Z
```

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
