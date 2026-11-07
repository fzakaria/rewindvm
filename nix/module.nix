# The NixOS module, `programs.rewind`: installs rewind, adds the binary
# cache Rewind's builds are in, on AMD Zen can set the branch counter
# workaround at every boot so runs use counter time (docs/pmu.md), and can
# tell Nix the machine builds derivations that run Rewind on its CPU
# vendor;
# `programs.rewind.app` installs the desktop app. The flake passes its own
# packages as the defaults.
{ rewind, app }:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.programs.rewind;

  # The system feature a derivation that runs Rewind in its build names in
  # `requiredSystemFeatures`. It carries the CPU vendor because the guest's
  # fixed CPU model keeps the host's: AMD and Intel make different runs of
  # the same inputs.
  feature = vendor: "rewind-${vendor}";

  # The cache CI pushes every build to, the same one the flake's nixConfig
  # names.
  cache = "https://rewindvm.cachix.org";
  cacheKey = "rewindvm.cachix.org-1:N5gL5fQTxBxim2HQNlYVWNYLK1ttOmMU1GP43eo4V5g=";
in
{
  options.programs.rewind = {
    enable = lib.mkEnableOption "Rewind VM, deterministic Linux VMs you can scrub, rewind and fork";

    package = lib.mkOption {
      type = lib.types.package;
      default = rewind;
      description = "The rewind package to install.";
    };

    binaryCache = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = ''
        Add ${cache} to Nix's substituters. It holds Rewind's own
        builds, among them the VM kernel's debug output, which
        `rewind gdb` fetches the first time it runs; nothing else has it.
      '';
    };

    amdBranchCounterWorkaround = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Set rr's workaround for AMD Zen's branch counter at boot, as
        `sudo rewind pmu enable` does until reboot, so runs can let the
        VM's clock follow its work. This sets bit 54 of MSR 0xc0011020
        on every CPU, which changes how the CPU speculates around locked
        instructions for every program on the machine.
      '';
    };

    systemFeature = lib.mkOption {
      type = lib.types.nullOr (
        lib.types.enum [
          "amd"
          "intel"
        ]
      );
      default = null;
      description = ''
        This machine's CPU vendor, to add `rewind-amd` or `rewind-intel`
        to Nix's system features, so derivations that run Rewind in their
        build and name that feature in `requiredSystemFeatures` build
        here. Runs of the same inputs are the same on every host of one
        vendor, and differ between vendors. Set it only where
        `rewind doctor` reports kvm, the CPU and counter time as ok. On
        AMD Zen, counter time needs `amdBranchCounterWorkaround` as well.
      '';
    };

    app = {
      enable = lib.mkEnableOption "the Rewind VM desktop app, the scrubber over recorded runs";

      package = lib.mkOption {
        type = lib.types.package;
        default = app;
        description = "The desktop app package to install.";
      };
    };
  };

  config = lib.mkMerge [
    (lib.mkIf cfg.enable {
      environment.systemPackages = [ cfg.package ];
    })

    (lib.mkIf (cfg.enable && cfg.binaryCache) {
      nix.settings.extra-substituters = [ cache ];
      nix.settings.extra-trusted-public-keys = [ cacheKey ];
    })

    # extra-system-features adds to NixOS's own list (kvm, big-parallel,
    # ...) where system-features would replace it.
    (lib.mkIf (cfg.enable && cfg.systemFeature != null) {
      nix.settings.extra-system-features = [ (feature cfg.systemFeature) ];
    })

    (lib.mkIf cfg.app.enable {
      environment.systemPackages = [ cfg.app.package ];
    })

    # A oneshot at boot, since the MSR bit resets with the CPU. It runs once
    # the msr module is loaded, and has modprobe for a switch that has not
    # loaded it yet.
    (lib.mkIf (cfg.enable && cfg.amdBranchCounterWorkaround) {
      boot.kernelModules = [ "msr" ];
      systemd.services.rewind-pmu = {
        description = "Make the AMD branch counter exact for Rewind VM";
        wantedBy = [ "multi-user.target" ];
        after = [ "systemd-modules-load.service" ];
        path = [ pkgs.kmod ];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
          ExecStart = "${lib.getExe cfg.package} pmu enable";
        };
      };
    })
  ];
}
