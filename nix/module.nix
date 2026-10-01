# The NixOS module, `programs.rewind`: installs rewind, and on AMD Zen can
# set the branch counter workaround at every boot so runs use counter time
# (docs/pmu.md); `programs.rewind.app` installs the desktop app.
#
# The repository's flake and both release tarballs' flakes export this
# module, each with the packages it has: the command's tarball passes
# `rewind`, the app's tarball passes `app`. The options are declared in a
# module with a fixed key, so NixOS takes them once when a configuration
# imports both tarballs' modules, and each import sets its own package as
# the default.
{
  rewind ? null,
  app ? null,
}:
let
  options =
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
          type = lib.types.nullOr lib.types.package;
          default = null;
          description = "The rewind package to install.";
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
            type = lib.types.nullOr lib.types.package;
            default = null;
            description = "The desktop app package to install.";
          };
        };
      };

      config = lib.mkMerge [
        {
          assertions = [
            {
              assertion = !cfg.enable || cfg.package != null;
              message = "programs.rewind.enable needs programs.rewind.package; import the rewind tarball's nixosModules.default.";
            }
            {
              assertion = !cfg.app.enable || cfg.app.package != null;
              message = "programs.rewind.app.enable needs programs.rewind.app.package; import the rewind-app tarball's nixosModules.default.";
            }
          ];
        }

        (lib.mkIf (cfg.enable && cfg.package != null) {
          environment.systemPackages = [ cfg.package ];
        })

        (lib.mkIf (cfg.app.enable && cfg.app.package != null) {
          environment.systemPackages = [ cfg.app.package ];
        })

        # A oneshot at boot, since the MSR bit resets with the CPU.
        (lib.mkIf (cfg.enable && cfg.amdBranchCounterWorkaround && cfg.package != null) {
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
    };
in
{ lib, ... }:
{
  imports = [
    {
      key = "rewindvm.dev/module.nix";
      imports = [ options ];
    }
  ];

  config.programs.rewind = lib.mkMerge [
    (lib.mkIf (rewind != null) { package = lib.mkDefault rewind; })
    (lib.mkIf (app != null) { app.package = lib.mkDefault app; })
  ];
}
