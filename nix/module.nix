# The NixOS module, `programs.rewind`: installs rewind, and on AMD Zen can
# set the branch counter workaround at every boot so runs use counter time
# (docs/pmu.md). The repository's flake and the release tarball's flake
# both export it, each with its own default package.
{ rewind }:
{
  config,
  lib,
  ...
}:
let
  cfg = config.programs.rewind;
in
{
  options.programs.rewind = {
    enable = lib.mkEnableOption "Rewind VM, deterministic Linux VMs you can scrub, rewind and fork";

    package = lib.mkOption {
      type = lib.types.package;
      default = rewind;
      description = "The rewind package to install.";
    };

    amdBranchCounterWorkaround = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Set rr's workaround for AMD Zen's branch counter at boot, as
        `sudo rewind pmu enable` does until reboot, so runs can let the
        guest's clock follow its work. This sets bit 54 of MSR 0xc0011020
        on every CPU, which changes how the CPU speculates around locked
        instructions for every program on the machine.
      '';
    };
  };

  config = lib.mkIf cfg.enable (
    lib.mkMerge [
      {
        environment.systemPackages = [ cfg.package ];
      }

      # A oneshot at boot, since the MSR bit resets with the CPU.
      (lib.mkIf cfg.amdBranchCounterWorkaround {
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
    ]
  );
}
