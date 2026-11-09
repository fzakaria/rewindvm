# Tutorial: deterministic derivations

A derivation names its builder, its arguments, its environment and every
store path the build may read, and Nix hashes all of that. The build reads
more: the clock, the kernel's randomness, the order its threads run in, and
the CPU and core count of whichever machine picked the job up. None of those
is in the hash, so one derivation can have many outputs.

Rewind VM fixes each of them, most with an option: `--epoch` for the clock,
`--seed` for the randomness, `--schedule` for the thread order. This
tutorial puts the options in the derivation, so Nix hashes the machine with
everything else and a store path names one output. It uses `inRewind`, a
function in the flake's `lib`, and then the other route, a platform Nix
hands whole builds to.

Every transcript below is real output on a 16 core AMD laptop. Install
Rewind as the [Nix tutorial](tutorial-nix.md) does.

## What the builder needs

A derivation that `inRewind` made asks Nix for two system features: `kvm`,
and `rewind-amd` or `rewind-intel` for the CPU's vendor. The guest's CPU
model is fixed, but it keeps the host's vendor, so runs of the same inputs
are the same on every machine of one vendor and differ between the two.
NixOS sets the second feature with the module:

```nix
programs.rewind.systemFeature = "amd";
# AMD only: counter time, here and inside Nix's sandbox
programs.rewind.amdBranchCounterWorkaround = true;
```

On another distribution, add `extra-system-features = rewind-amd` to
`nix.conf`, and on AMD `extra-sandbox-paths = /run/rewind?` as well, which
shows the sandbox the marker `sudo rewind pmu enable` leaves.

By default the VM's clock follows the work done inside it, counted by the
CPU's branch counter, which [Time inside the VM](pmu.md) explains. A machine
without an exact counter builds with `clock = "exits"` in the machine below.

## A build that stamps itself

`stamp`, from
[examples](https://github.com/fzakaria/rewindvm/tree/main/examples), prints
when it was built and by which build. Its Makefile reads a UUID from the
kernel's random pool and the time from the clock as it compiles, the way
release builds record a build id:

```console
$ nix build --no-link --print-out-paths github:fzakaria/rewindvm#stamp
/nix/store/...-stamp-0.1.0

$ nix run github:fzakaria/rewindvm#stamp
build b2060565-ce20-4c7b-80f9-e19f540dbc78 at 2026-11-09T18:06:35Z

$ nix build --no-link github:fzakaria/rewindvm#stamp --rebuild
checking outputs of '/nix/store/...-stamp-0.1.0.drv'...
error: derivation '/nix/store/...-stamp-0.1.0.drv' may not be deterministic: output "/nix/store/...-stamp-0.1.0" differs
```

The second build of the same derivation made another program, and
`--rebuild` says so. The store path names whichever build got there first.

## The same build inside Rewind

`inRewind` takes a machine and a derivation, and returns the derivation
with its builder running inside the VM. In a project of your own:

```nix
let
  rewind = builtins.getFlake "github:fzakaria/rewindvm";
  inherit (rewind.lib.x86_64-linux) inRewind;
in
inRewind { epoch = 1790985600; } rewind.packages.x86_64-linux.stamp
```

The epoch is the VM's clock at boot, in seconds since 1970, and has no
default: Rewind's own is the start of today, which no hash would cover.
The commands below give the same expression on the command line. The stock
`nix-daemon` builds it:

```console
$ nix build --no-link --print-out-paths --impure --expr 'with builtins.getFlake "github:fzakaria/rewindvm"; lib.x86_64-linux.inRewind { epoch = 1790985600; } packages.x86_64-linux.stamp'
/nix/store/...-stamp-0.1.0

$ $(nix build --no-link --print-out-paths --impure --expr 'with builtins.getFlake "github:fzakaria/rewindvm"; lib.x86_64-linux.inRewind { epoch = 1790985600; } packages.x86_64-linux.stamp')/bin/stamp
build 4ce9bd9b-a9df-4eb0-968c-cae21737b552 at 2026-10-03T00:00:00Z
```

1790985600 is midnight on 2026-10-03, and the VM booted then. Build the
derivation again, with its log:

```console
$ nix build -L --no-link --impure --expr 'with builtins.getFlake "github:fzakaria/rewindvm"; lib.x86_64-linux.inRewind { epoch = 1790985600; } packages.x86_64-linux.stamp' --rebuild
checking outputs of '/nix/store/...-stamp-0.1.0.drv'...
...
stamp> gcc -O1 -Wall -DBUILD_ID='"4ce9bd9b-a9df-4eb0-968c-cae21737b552"' -DBUILT_AT='"2026-10-03T00:00:00Z"' -o stamp stamp.c
...
stamp> rewind: run 627beb52427760dd exited:0 after 4366 steps, 0.114s virtual, 0.573s wall (poweroff)
```

The builder's output is the build's log, and the last line of it is the
run: its id, a hash of everything the VM was given, and how it ended.
`--rebuild` passes: the second build made the bytes the first made. And a
third time:

```console
$ nix build -L --no-link --impure --expr 'with builtins.getFlake "github:fzakaria/rewindvm"; lib.x86_64-linux.inRewind { epoch = 1790985600; } packages.x86_64-linux.stamp' --rebuild
...
stamp> rewind: run 627beb52427760dd exited:0 after 4366 steps, 0.114s virtual, 0.609s wall (poweroff)
```

The same run, 627beb52, with the same 4366 steps. The UUID still
comes from the kernel's random pool and the time still comes from the
clock. They are the same on every build because the pool's seed and the
clock's start are in the derivation.

The run's id is this Rewind's. Rewind is an input of the derivation, so
another build of Rewind gives the output another path, which the builder's
environment names, and the run another id.

## What changed in the derivation

The result is an ordinary `x86_64-linux` derivation with three things
changed:

```console
$ nix eval --impure --expr '(with builtins.getFlake "github:fzakaria/rewindvm"; lib.x86_64-linux.inRewind { epoch = 1790985600; } packages.x86_64-linux.stamp).rewindFlags'
"--epoch 1790985600 --seed 0 --schedule 0 --cpu v3-compat --clock branches --cores 1 --mem 1024 --vendor amd"

$ nix eval --impure --expr '(with builtins.getFlake "github:fzakaria/rewindvm"; lib.x86_64-linux.inRewind { epoch = 1790985600; } packages.x86_64-linux.stamp).requiredSystemFeatures'
[ "kvm" "rewind-amd" ]
```

- `rewindFlags` is the machine, as the options `rewind run` takes for one,
  the defaults written out with the rest. It is an attribute, so Nix hashes
  it.
- The builder is a script that describes the build Nix set up and hands the
  description to `rewind build`, which runs the derivation's own builder in
  the VM, as `rewind nix` runs one, and copies the outputs back. The path of
  Rewind is among the builder's arguments, so Rewind is an input of the
  derivation, as the compiler is, and the hash covers the VM and its guest
  kernel.
- The system features route the build to a machine with KVM and the
  vendor's CPU. `rewind build` checks the vendor itself as well, and
  refuses the other one.

Three things the build can see are fixed without an option. The VM's store
holds the closure of what the original derivation names and nothing else:
Nix's sandbox also shows each machine's own `sandbox-paths`, which no
derivation names. `NIX_BUILD_CORES` is the machine's `cores`, 1 unless you
say otherwise, where Nix passes the count of whichever machine builds. The
build directory is `/build`, so the function needs Nix's sandbox on.

A derivation with `__structuredAttrs` works, and so does one with several
outputs. A content-addressed derivation does too: Nix gives its builder a
scratch path for each output, and `rewind build` copies to the paths it is
given. A fixed-output derivation is refused when it is evaluated, since
Nix builds one with the network and a build in the VM has none.

## The machine is data

The machine's arguments, with their defaults:

| argument   | default       | what it sets                                                     |
| ---------- | ------------- | ---------------------------------------------------------------- |
| `epoch`    | none          | the VM's wall clock at boot, in seconds since the Unix epoch     |
| `seed`     | `0`           | the seed of the kernel's randomness                              |
| `schedule` | `0`           | the thread schedule; 0 is the unperturbed one                    |
| `clock`    | `"branches"`  | what moves the clock: the VM's work, or `"exits"` for exits only |
| `cpu`      | `"v3-compat"` | the CPU model: `"v3"`, or v3 without ERMS, which more hosts show |
| `vendor`   | `"amd"`       | the CPU vendor of the machines that build it, or `"intel"`       |
| `cores`    | `1`           | the CPUs programs are told of, and `NIX_BUILD_CORES`             |
| `mem`      | `1024`        | the VM's memory in MiB, which also holds the build's files       |

Another machine is another derivation, with its own output:

```console
$ $(nix build --no-link --print-out-paths --impure --expr 'with builtins.getFlake "github:fzakaria/rewindvm"; lib.x86_64-linux.inRewind { epoch = 1749945600; seed = 7; } packages.x86_64-linux.stamp')/bin/stamp
build 85271e06-23f4-4b7c-b730-3bc9ffe174df at 2025-06-15T00:00:00Z
```

That build ran on 2025-06-15, as it will every time. A test that reads the
calendar, or a build whose threads race, can be pinned the same way, and a
list of seeds or schedules is a list of derivations.

## Looking inside a build

The run of a build made this way is kept in the sandbox and goes with it.
To look inside the build, run the original derivation on the same machine
with `rewind nix`, which keeps the run:

```console
$ rewind nix github:fzakaria/rewindvm#stamp --no-compare --epoch 1790985600 --cpu v3-compat --clock branches
...
rewind: run 9ee8893cc9ef8011 exited:0 after 4378 steps, 0.114s virtual, 0.598s wall (poweroff)
/nix/store/...-stamp-0.1.0 b35fadaece3b3931  differs from your store
...
```

`rewind nix` compares its output with the store's, which the host built
with another UUID at another time. The two runs in the VM have the same
builder on the same machine. Their ids differ, since the derivation's
attributes are in the builder's environment and `inRewind` changed them.
Every tool of the other tutorials works on the kept run. One of them is how
`rewind build` copies an output out: `rewind cat --archive` writes a
directory as it was at a step, as a tar or a NAR.

```console
$ rewind cat 9ee8893c 4378 /nix/store/yd7qsa9lqlygnddnikr0wjqqzwlcgv6w-stamp-0.1.0 --archive tar | tar --utc --numeric-owner -tvf -
drwxr-xr-x 1000/100          0 2026-10-03 00:00 yd7qsa9lqlygnddnikr0wjqqzwlcgv6w-stamp-0.1.0/
drwxr-xr-x 1000/100          0 2026-10-03 00:00 yd7qsa9lqlygnddnikr0wjqqzwlcgv6w-stamp-0.1.0/bin/
-rwxr-xr-x 1000/100      15704 2026-10-03 00:00 yd7qsa9lqlygnddnikr0wjqqzwlcgv6w-stamp-0.1.0/bin/stamp
```

The archive holds what the VM held: the build user's ids and the VM's
clock, and nothing of the machine that asked. So the same run at the same
step is the same bytes on any machine.

## The platform route

`inRewind` puts the VM inside the derivation: the builder starts it, in
Nix's sandbox. The other place for the VM is under Nix. Nix's experimental
[`external-builders`](https://nix.dev/manual/nix/latest/command-ref/conf-file.html#conf-external-builders)
setting, in Nix 2.32 and later, hands every build for the platforms it
lists to a program, in place of the sandbox. `rewind build
--external-builder` is such a program, and `onRewind` asks for the
platform:

```nix
onRewind { epoch = 1790985600; } rewind.packages.x86_64-linux.stamp
```

The derivation keeps its own builder. Its `system` becomes
`x86_64-rewind-amd`, and `rewindFlags` holds the machine as before. A
client cannot set `external-builders` for a daemon, so this build uses a
store of its own, in a directory:

```console
$ nix build -L --no-link --print-out-paths --impure --store ./platform-store --extra-experimental-features external-builders --external-builders '[{"systems":["x86_64-rewind-amd"],"program":"'$(command -v rewind)'","args":["build","--external-builder"]}]' --expr 'with builtins.getFlake "github:fzakaria/rewindvm"; lib.x86_64-linux.onRewind { epoch = 1790985600; } packages.x86_64-linux.stamp'
...
stamp> gcc -O1 -Wall -DBUILD_ID='"4ce9bd9b-a9df-4eb0-968c-cae21737b552"' -DBUILT_AT='"2026-10-03T00:00:00Z"' -o stamp stamp.c
...
stamp> rewind: run f4d0d049cdb2512a exited:0 after 4375 steps, 0.114s virtual, 0.526s wall (poweroff)
/nix/store/...-stamp-0.1.0
```

For a daemon, the same two settings go in its `nix.conf`:

```
extra-experimental-features = external-builders
external-builders = [{"systems":["x86_64-rewind-amd"],"program":"/run/current-system/sw/bin/rewind","args":["build","--external-builder"]}]
```

## Which route

Start with the function. It needs nothing from Nix but two system
features, the stock daemon builds its derivations, and it can wrap only the
derivations whose builds are known to differ.

The platform is the plainer description: a fixed kernel on a fixed CPU
model with a clock you set is another platform than some x86_64 Linux
machine. Nix hands the program the exact closure, and no sandbox sits
around the VM. It needs an experimental feature on every builder, and
nixpkgs knows no system called `x86_64-rewind-amd`.

What each derivation's hash covers:

| input                                         | `inRewind`                           | `onRewind`                    |
| --------------------------------------------- | ------------------------------------ | ----------------------------- |
| the clock, the seed and the schedule          | yes, in `rewindFlags`                | yes, in `rewindFlags`         |
| the CPU model, the cores and the memory       | yes, in `rewindFlags`                | yes, in `rewindFlags`         |
| the CPU vendor                                | yes, in the features and the options | yes, in `system`              |
| Rewind and its guest kernel                   | yes, an input                        | no, `nix.conf` names a Rewind |
| the builder's own kernel, CPU model and cores | not read                             | not read                      |

## What it costs

GNU hello, rebuilt on the host and then through `inRewind`:

```console
$ nix build --no-link --impure --expr '(builtins.getFlake "github:fzakaria/rewindvm").inputs.nixpkgs.legacyPackages.x86_64-linux.hello' --rebuild
```

```console
$ nix build -L --no-link --impure --expr 'with builtins.getFlake "github:fzakaria/rewindvm"; lib.x86_64-linux.inRewind { epoch = 1790985600; } inputs.nixpkgs.legacyPackages.x86_64-linux.hello' --rebuild
...
hello> rewind: run 03020571472f6a1a exited:0 after 124838 steps, 12.031s virtual, 12.449s wall (poweroff)
```

The host took 12 seconds and `inRewind` took
17, of which the run's wall time above is the build
itself. The rest is evaluating, packing the closure into an image, and
copying the output out.

- **One virtual CPU.** `cores` changes what programs are told, and their
  threads still take turns on one CPU. GNU hello's build is mostly a serial
  `configure`; a build that uses sixteen cores takes longer.
- **An image a build.** The sandbox keeps nothing between builds, so each
  build packs its input closure into an image again.
- **Memory.** The build directory and the outputs live in the VM's memory
  until they are copied out, each in a filesystem half of `mem` large. A
  build that writes more is told there is no space left, as on a full disk.
- **Rewind is in the hash.** With `inRewind`, a new Rewind rebuilds every
  derivation the function made, as a new compiler does.
- **Two vendors.** An AMD builder cannot check an Intel builder's output.
- **No network.** Fixed-output derivations stay as they are.
