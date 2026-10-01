# The NixOS module, `programs.rewind`: installs rewind, adds the binary
# cache Rewind's builds are in, and on AMD Zen can set the branch counter
# workaround at every boot so runs use counter time (docs/pmu.md);
# `programs.rewind.app` installs the desktop app. The flake passes its own
# packages as the defaults.
{ rewind, app }:
{
  config,
  lib,
  ...
}:
let
  cfg = config.programs.rewind;

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

    (lib.mkIf cfg.app.enable {
      environment.systemPackages = [ cfg.app.package ];
    })

    # A oneshot at boot, since the MSR bit resets with the CPU.
    (lib.mkIf (cfg.enable && cfg.amdBranchCounterWorkaround) {
      boot.kernelModules = [ "msr" ];
      systemd.services.rewind-pmu = {
        description = "Make the AMD branch counter exact for Rewind VM";
        wantedBy = [ "multi-user.target" ];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
          ExecStart = "${lib.getExe cfg.package} pmu enable";
        };
      };
    })
  ];
}
