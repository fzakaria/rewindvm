{
  description = "Rewind VM: deterministic Linux VMs you can scrub, rewind and fork";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    # Rust builds whose dependencies are a derivation of their own
    # (nix/crane.nix). It takes nixpkgs from its caller, so it has no
    # nixpkgs input to follow.
    crane.url = "github:ipetkov/crane";
    # Any version of any nixpkgs package from one input, fetched only when
    # used: `rewind gdb` needs a newer gdb than nixpkgs above ships.
    multiverse.url = "github:fzakaria/nixpkgs-multiverse";
    # nixbot's effects library, for the Cachix effect (nix/effects.nix)
    nixbot.url = "github:Mic92/nixbot";
    nixbot.inputs.nixpkgs.follows = "nixpkgs";
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
      multiverse,
      nixbot,
    }:
    let
      # Supported host platforms: Linux KVM (x86_64 and aarch64) and macOS
      # Apple Silicon (Hypervisor.framework).
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
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
          isLinux = pkgs.stdenv.hostPlatform.isLinux;
          isX86Linux = system == "x86_64-linux";
          rust = import ./nix/crane.nix { inherit pkgs crane; };
          kernel = import ./nix/kernel.nix { inherit pkgs; };
          guest = import ./nix/guest.nix { inherit pkgs; };
          nixseparatedebuginfod2 = if isLinux then pkgs.nixseparatedebuginfod2 else null;
          # gdb 17 or later, which finds thread-local variables itself
          # where `rewind gdb`'s stub lists the libraries: Hydra's build on
          # x86_64-linux, pkgs.gdb on aarch64-linux, and null on darwin.
          gdb =
            if isX86Linux then
              multiverse.multiverse.${system}.fast.version "gdb" "17.2"
            else if isLinux then
              pkgs.gdb
            else
              null;
          rewind = import ./nix/rewind.nix {
            inherit
              pkgs
              rust
              kernel
              guest
              nixseparatedebuginfod2
              gdb
              commit
              ;
          };
          app =
            if isLinux then
              import ./nix/app.nix {
                inherit
                  pkgs
                  rust
                  rewind
                  releaseDate
                  ;
              }
            else
              null;
          # The desktop app built for the browser, the site's viewer.
          web = import ./nix/web.nix { inherit pkgs rust releaseDate; };
        in
        {
          inherit
            pkgs
            isLinux
            isX86Linux
            rust
            kernel
            guest
            nixseparatedebuginfod2
            gdb
            rewind
            app
            web
            ;
          release =
            if isLinux then
              import ./nix/release.nix {
                inherit
                  pkgs
                  rust
                  kernel
                  guest
                  nixseparatedebuginfod2
                  commit
                  ;
              }
            else
              null;
          releaseDebug = if isLinux then import ./nix/release-debug.nix { inherit pkgs kernel; } else null;
          appRelease =
            if isX86Linux then
              import ./nix/app-release.nix { inherit pkgs rust releaseDate; }
            else
              null;
          site = import ./nix/site.nix { inherit pkgs web; };
          examples = import ./nix/examples.nix { inherit pkgs; };
          benchInputs = if isLinux then import ./nix/bench { inherit pkgs; } else null;
          license =
            if isLinux then
              import ./nix/license.nix {
                inherit
                  pkgs
                  rust
                  app
                  releaseDate
                  ;
              }
            else
              null;
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

          # the guest kernel with the Rewind platform (nix/kernel.nix)
          kernel = p.kernel;

          # the same kernel's DWARF, by build ID, for `rewind gdb`
          kernel-debug = p.kernel.debug;

          # the guest's init and initramfs (nix/guest.nix)
          init = p.guest.init;
          initrd = p.guest.initrd;

          # the marketing site the pages workflow deploys (nix/site.nix)
          site = p.site;

          # the desktop app built for the browser, which the site serves
          # as its viewer (nix/web.nix)
          web = p.web;

          # the tutorials' flaky thread pool (nix/examples.nix)
          mylib = p.examples.mylib;

          # the dining philosophers, a deadlock (nix/examples.nix)
          philosophers = p.examples.philosophers;

          # two tellers losing a deposit, a lost update (nix/examples.nix)
          bank = p.examples.bank;

          # two threads losing a hit with no system call between read and
          # store, reached only with a schedule point (nix/examples.nix)
          counter = p.examples.counter;

          # bank as a JUnit 5 suite marked with the Rewind SDK's extension
          # (nix/examples.nix)
          bank-java = p.examples.bank-java;

          # a config read while it is rewritten, a file race between
          # processes (nix/examples.nix)
          config-reload = p.examples.config-reload;

          # a parent waiting on a signal that already came, a lost wakeup
          # (nix/examples.nix)
          waiter = p.examples.waiter;

          # a test that expects a HashSet's order, decided by the seed
          # (nix/examples.nix)
          hash-order = p.examples.hash-order;

          # a pkg-config file whose Requires order is decided by Python's
          # hash seed (nix/examples.nix)
          tiles = p.examples.tiles;

          # a plugin index in the order its directory lists, decided by
          # the directory order (nix/examples.nix)
          plugins = p.examples.plugins;

          # a program stamped with a random id and the time of its build,
          # another on every build outside Rewind (nix/examples.nix)
          stamp = p.examples.stamp;
        }
        // nixpkgs.lib.optionalAttrs p.isLinux {
          # the command's release asset, for people without Nix
          # (nix/release.nix)
          release = p.release;

          # the kernel's debug symbols and sources for `rewind gdb`, a
          # release asset next to the command's (nix/release-debug.nix)
          release-debug = p.releaseDebug;

          # the debuginfod server `rewind gdb` starts
          debuginfod = p.nixseparatedebuginfod2;

          # the desktop app, the scrubber over recorded runs (nix/app.nix)
          app = p.app;

          # the license issuer, kept out of the app's package
          # (nix/license.nix)
          license = p.license;

          # the programs and data tools/bench.py measures every build
          # against (nix/bench)
          bench-inputs = p.benchInputs;
        }
        // nixpkgs.lib.optionalAttrs p.isX86Linux {
          # the desktop app's release asset: a build for any distribution,
          # with a launcher (nix/app-release.nix)
          app-release = p.appRelease;
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

          # `nix run .#serve [port]`: the built site on a local port
          serve = {
            type = "app";
            program = "${import ./nix/serve.nix { inherit (p) pkgs site; }}/bin/serve-site";
          };
        }
        // nixpkgs.lib.optionalAttrs p.isLinux {
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

          # `nix run .#bench -- ...`: tools/bench.py with its tools on
          # PATH and the inputs built (nix/bench/app.nix)
          bench = {
            type = "app";
            program = "${
              import ./nix/bench/app.nix {
                inherit (p) pkgs;
                inputs = p.benchInputs;
              }
            }/bin/rewind-bench";
          };
        }
      );

      checks = forAllSystems (
        system:
        let
          p = per system;
        in
        (nixpkgs.lib.optionalAttrs p.isX86Linux (
          import ./nix/checks.nix {
            inherit (p)
              pkgs
              rust
              rewind
              app
              kernel
              guest
              gdb
              site
              ;
            inherit releaseDate;
            module = self.nixosModules.default;
            rewindLib = self.lib.${system};
          }
        ))

        # nixbot (nixbot.decio.us) builds `checks` and nothing else, so the
        # tree's formatting and every package are checks as well: a package
        # that fails to build, or a file `nix fmt` would change, fails CI.
        # The tutorials' examples (nix/examples.nix) have tests that fail
        # under some interleavings or seeds on purpose, for Rewind to find,
        # so their checks build them with those tests off.
        // {
          formatting = self.formatter.${system}.check self;
        }
        // nixpkgs.lib.mapAttrs' (
          name: drv:
          nixpkgs.lib.nameValuePair "package-${name}" (
            if p.examples ? ${name} then drv.overrideAttrs { doCheck = false; } else drv
          )
        ) self.packages.${system}
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
            gdb
            app
            ;
        }
      );

      # `lib.<system>.inRewind { epoch = 1790985600; } drv`: any derivation
      # with its builder running inside Rewind, and the clock, the seed and
      # the schedule among its attributes; `onRewind` asks Nix for the
      # platform instead (nix/lib.nix)
      lib = forAllSystems (
        system:
        let
          p = per system;
        in
        import ./nix/lib.nix { inherit (p) pkgs rewind; }
      );

      # `programs.rewind` and `programs.rewind.app` for NixOS (nix/module.nix)
      nixosModules.default = import ./nix/module.nix {
        rewind = self.packages.x86_64-linux.default;
        app = self.packages.x86_64-linux.app;
      };

      formatter = forAllSystems (
        system: import ./nix/formatter.nix { pkgs = nixpkgs.legacyPackages.${system}; }
      );

      # nixbot's effects (nix/effects.nix)
      herculesCI = import ./nix/effects.nix rec {
        pkgs = nixpkgs.legacyPackages.x86_64-linux;
        effects = nixbot.lib.effects { inherit pkgs; };
        packages = self.packages.x86_64-linux;
      };
    };
}
