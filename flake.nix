{
  description = "Rewind VM: deterministic Linux VMs you can scrub, rewind and fork";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  # CI pushes every build to this cache, so `nix run github:fzakaria/rewindvm`
  # downloads the guest kernel and the app instead of compiling them. Nix
  # asks before using a flake's substituter unless the user is trusted.
  nixConfig = {
    extra-substituters = [ "https://rewindvm.cachix.org" ];
    extra-trusted-public-keys = [
      "rewindvm.cachix.org-1:N5gL5fQTxBxim2HQNlYVWNYLK1ttOmMU1GP43eo4V5g="
    ];
  };

  # Wiring only. Everything this repository builds lives in its own file
  # under nix/, so a flake output here is one line and its definition is one
  # file over there.
  outputs =
    { self, nixpkgs }:
    let
      # x86_64-linux only: the engine is a KVM virtual machine monitor, and
      # the guest kernel's hypervisor platform is x86 code.
      systems = [ "x86_64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system);

      # Everything a system's outputs share, built once per system.
      per =
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          kernel = import ./nix/kernel.nix { inherit pkgs; };
          guest = import ./nix/guest.nix { inherit pkgs; };
          app = import ./nix/app.nix { inherit pkgs; };
        in
        {
          inherit
            pkgs
            kernel
            guest
            app
            ;
          rewind = import ./nix/rewind.nix { inherit pkgs kernel guest; };
          release = import ./nix/release.nix { inherit pkgs kernel guest; };
          appRelease = import ./nix/app-release.nix { inherit pkgs; };
          site = import ./nix/site.nix { inherit pkgs; };
          examples = import ./nix/examples.nix { inherit pkgs; };
          license = import ./nix/license.nix { inherit pkgs app; };
        };
    in
    {
      packages = forAllSystems (
        system:
        let
          p = per system;
        in
        {
          # the rewind command with its guest (nix/rewind.nix)
          rewind = p.rewind;
          default = p.rewind;

          # the command's release asset, for people without Nix
          # (nix/release.nix)
          release = p.release;

          # the guest kernel with the Rewind platform (nix/kernel.nix)
          kernel = p.kernel;

          # the guest's init and initramfs (nix/guest.nix)
          init = p.guest.init;
          initrd = p.guest.initrd;

          # the marketing site the pages workflow deploys (nix/site.nix)
          site = p.site;

          # the desktop app, the scrubber over recorded runs (nix/app.nix)
          app = p.app;

          # the desktop app's release asset: a build for any distribution,
          # with a launcher (nix/app-release.nix)
          app-release = p.appRelease;

          # the license issuer, kept out of the app's package
          # (nix/license.nix)
          license = p.license;

          # the tutorials' flaky thread pool (nix/examples.nix)
          mylib = p.examples.mylib;
        }
      );

      apps = forAllSystems (
        system:
        let
          p = per system;
        in
        {
          default = {
            type = "app";
            program = "${p.rewind}/bin/rewind";
          };

          # `nix run .#license -- keygen | issue ...`: the license issuer
          # (nix/license.nix, crates/rewind-app/LICENSING.md)
          license = {
            type = "app";
            program = "${p.license}/bin/rewind-license";
          };

          # `nix run .#app -- <run>`: the desktop app
          app = {
            type = "app";
            program = "${p.app}/bin/rewind-app";
          };

          # `nix run .#serve [port]`: the built site on a local port
          serve = {
            type = "app";
            program = "${import ./nix/serve.nix { inherit (p) pkgs site; }}/bin/serve-site";
          };
        }
      );

      checks = forAllSystems (
        system:
        let
          p = per system;
        in
        import ./nix/checks.nix {
          inherit (p) pkgs rewind;
          module = self.nixosModules.default;
        }
      );

      devShells = forAllSystems (
        system:
        let
          p = per system;
        in
        import ./nix/dev-shells.nix { inherit (p) pkgs kernel guest; }
      );

      # `programs.rewind` and `programs.rewind.app` for NixOS (nix/module.nix)
      nixosModules.default = import ./nix/module.nix {
        rewind = self.packages.x86_64-linux.default;
        app = self.packages.x86_64-linux.app;
      };

      formatter = forAllSystems (
        system: import ./nix/formatter.nix { pkgs = nixpkgs.legacyPackages.${system}; }
      );
    };
}
