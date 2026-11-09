# `lib.<system>`: derivations built inside Rewind, with the machine written
# down in the derivation (docs/nix-builds.md).
#
# Both functions take the machine first and a derivation second, and return
# the derivation with its builder running in the VM. The machine lands in
# the derivation as the `rewindFlags` attribute, the options `rewind run`
# takes for one, so the derivation's hash covers the clock, the seed and
# the schedule as it covers the compiler. `rewind build` reads the
# attribute and takes its machine from nowhere else.
#
#   inRewind { epoch = 1790985600; } pkgs.hello
#
# Import the file with another `pkgs` to take bash, jq and closureInfo
# from that nixpkgs instead of this flake's.
{ pkgs, rewind }:
let
  inherit (pkgs) lib;

  # The CPU vendors a run can be for. The guest's fixed CPU model keeps
  # the host's vendor, so the two make different runs of one derivation.
  vendors = [
    "amd"
    "intel"
  ];

  # The machine's arguments, with the defaults of the ones left out.
  machineOf =
    {
      # The VM's wall clock at boot, in seconds since the Unix epoch.
      # Required: rewind's own default is the start of today.
      epoch,
      # Seeds the VM's randomness.
      seed ? 0,
      # The thread schedule; 0 is the unperturbed one.
      schedule ? 0,
      # What moves the VM's clock besides exits: the work done inside the
      # VM, counted by the builder's branch counter ("branches"), or
      # nothing ("exits").
      clock ? "branches",
      # The CPU the VM sees: x86-64-v3 ("v3"), or the same without ERMS
      # ("v3-compat"), which every machine that can show v3 can show.
      cpu ? "v3-compat",
      # The CPU vendor of the machines the derivation builds on.
      vendor ? "amd",
      # The CPUs programs in the VM are told it has, and the build's
      # NIX_BUILD_CORES. The VM has one vCPU however many it reports.
      cores ? 1,
      # The VM's memory in MiB. The build directory and the outputs are
      # kept in memory too.
      mem ? 1024,
    }:
    assert lib.assertMsg (builtins.isInt epoch) "rewind: epoch is seconds since the Unix epoch";
    assert lib.assertOneOf "clock" clock [
      "branches"
      "exits"
    ];
    assert lib.assertOneOf "cpu" cpu [
      "v3"
      "v3-compat"
    ];
    assert lib.assertOneOf "vendor" vendor vendors;
    {
      inherit
        epoch
        seed
        schedule
        clock
        cpu
        vendor
        cores
        mem
        ;
    };

  # The script `inRewind` makes the builder, as a store path of its own: a
  # path into this repository's source would make the whole source an
  # input of every derivation the function rewrites.
  script = builtins.path {
    name = "in-rewind.sh";
    path = ./in-rewind.sh;
  };

  # A machine as the options `rewind run` takes for one. Every option is
  # written out, so a default that changes in rewind changes no build.
  flagsOf =
    machine:
    lib.concatStringsSep " " [
      "--epoch ${toString machine.epoch}"
      "--seed ${toString machine.seed}"
      "--schedule ${toString machine.schedule}"
      "--cpu ${machine.cpu}"
      "--clock ${machine.clock}"
      "--cores ${toString machine.cores}"
      "--mem ${toString machine.mem}"
    ];
in
{
  # The function route: `inRewind machine drv` is `drv` with a builder that
  # starts the VM inside Nix's sandbox and runs `drv`'s own builder there.
  # The result is an ordinary x86_64-linux derivation, which a stock
  # nix-daemon builds on a machine with the `kvm` and `rewind-<vendor>`
  # system features. Rewind is an input of the derivation, so the hash
  # covers the VM and its guest kernel too.
  inRewind =
    args: drv:
    let
      machine = machineOf args;

      # Every store path `drv` names, in one file, so the file's closure
      # is the closure `drv`'s builder may read.
      inputs = pkgs.writeText "${drv.name}-inputs" (builtins.toJSON drv.drvAttrs);
      closure = pkgs.closureInfo { rootPaths = [ inputs ]; };
    in
    assert lib.assertMsg (!(drv.drvAttrs ? outputHash)) ''
      inRewind: ${drv.name} is a fixed-output derivation, which Nix builds with the network, and a build in Rewind has none.
    '';
    lib.overrideDerivation drv (_: {
      # `drv`'s own builder and arguments become the job the VM runs.
      builder = pkgs.runtimeShell;
      args = [
        script
        (lib.getExe rewind)
        (lib.getExe pkgs.jq)
        "${closure}/store-paths"
        "${inputs}"
        drv.drvAttrs.builder
      ]
      ++ (drv.drvAttrs.args or [ ]);

      # A builder with KVM whose Nix says its CPU is this vendor's
      # (`programs.rewind.systemFeature` in the NixOS module).
      requiredSystemFeatures = (drv.drvAttrs.requiredSystemFeatures or [ ]) ++ [
        "kvm"
        "rewind-${machine.vendor}"
      ];

      # The machine, and the vendor for `rewind build` to hold the builder
      # to, whatever its Nix says.
      rewindFlags = "${flagsOf machine} --vendor ${machine.vendor}";
      rewindRuntime = toString rewind;
    });

  # The platform route: `onRewind machine drv` is `drv` asking for the
  # platform x86_64-rewind-<vendor> in place of x86_64-linux. Nix hands a
  # build for that platform to the program its external-builders setting
  # names, `rewind build --external-builder`, with no sandbox of its own
  # around the VM. `rewindRuntime` pins the Rewind package, including its
  # kernel and initrd. The configured external builder must match.
  onRewind =
    args: drv:
    let
      machine = machineOf args;
    in
    lib.overrideDerivation drv (_: {
      system = "x86_64-rewind-${machine.vendor}";
      rewindFlags = flagsOf machine;
      rewindRuntime = toString rewind;
    });
}
