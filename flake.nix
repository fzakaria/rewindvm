{
  description = "Rewind VM: deterministic Linux VMs you can scrub, rewind and fork";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    # Rust builds whose dependencies are a derivation of their own
    # (nix/crane.nix). It takes nixpkgs from its caller, so it has no
    # nixpkgs input to follow.
    crane.url = "github:ipetkov/crane";
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
    {
      self,
      nixpkgs,
      crane,
    }:
    let
      # x86_64-linux only: the engine is a KVM virtual machine monitor, and
      # the guest kernel's hypervisor platform is x86 code.
      systems = [ "x86_64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system);

      # The commit the packages are built from, for `rewind --version` and
      # the manifests of the runs it records: 12 hex digits, marked dirty
      # for a tree with uncommitted changes, and null outside git.
      commit =
        if self ? rev then
          builtins.substring 0 12 self.rev
        else if self ? dirtyRev then
          "${builtins.substring 0 12 self.dirtyRev}-dirty"
        else
          null;

      # The day the commit was made, YYYY-MM-DD in UTC: the desktop app's
      # release date, which a license's Updates-Until is held against.
      releaseDate =
        let
          d = self.lastModifiedDate;
        in
        "${builtins.substring 0 4 d}-${builtins.substring 4 2 d}-${builtins.substring 6 2 d}";

      # Everything a system's outputs share, built once per system.
      per =
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          rust = import ./nix/crane.nix { inherit pkgs crane; };
          kernel = import ./nix/kernel.nix { inherit pkgs; };
          guest = import ./nix/guest.nix { inherit pkgs; };
          nixseparatedebuginfod2 = import ./nix/debuginfod.nix { inherit pkgs; };
          rewind = import ./nix/rewind.nix {
            inherit
              pkgs
              rust
              kernel
              guest
              nixseparatedebuginfod2
              commit
              ;
          };
          app = import ./nix/app.nix {
            inherit
              pkgs
              rust
              rewind
              releaseDate
              ;
          };
        in
        {
          inherit
            pkgs
            kernel
            guest
            nixseparatedebuginfod2
            rewind
            app
            ;
          release = import ./nix/release.nix {
            inherit
              pkgs
              rust
              kernel
              guest
              nixseparatedebuginfod2
              commit
              ;
          };
          releaseDebug = import ./nix/release-debug.nix { inherit pkgs kernel; };
          appRelease = import ./nix/app-release.nix { inherit pkgs rust releaseDate; };
          site = import ./nix/site.nix { inherit pkgs; };
          examples = import ./nix/examples.nix { inherit pkgs; };
          license = import ./nix/license.nix {
            inherit
              pkgs
              rust
              app
              releaseDate
              ;
          };
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

          # the kernel's debug symbols and sources for `rewind gdb`, a
          # release asset next to the command's (nix/release-debug.nix)
          release-debug = p.releaseDebug;

          # the guest kernel with the Rewind platform (nix/kernel.nix)
          kernel = p.kernel;

          # the same kernel's DWARF, by build ID, for `rewind gdb`
          kernel-debug = p.kernel.debug;

          # the debuginfod server `rewind gdb` starts, patched so sessions
          # share its cache (nix/debuginfod.nix)
          debuginfod = p.nixseparatedebuginfod2;

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

          # the dining philosophers, a deadlock (nix/examples.nix)
          philosophers = p.examples.philosophers;
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
          inherit (p) pkgs rewind kernel;
          module = self.nixosModules.default;
        }
      );

      devShells = forAllSystems (
        system:
        let
          p = per system;
        in
        import ./nix/dev-shells.nix {
          inherit (p)
            pkgs
            kernel
            guest
            nixseparatedebuginfod2
            app
            ;
        }
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
