{
  description = "Rewind VM: deterministic Linux VMs you can scrub, rewind and fork";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
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
        in
        {
          inherit pkgs;
          kernel = import ./nix/kernel.nix { inherit pkgs; };
        };
    in
    {
      packages = forAllSystems (
        system:
        let
          p = per system;
        in
        {
          # the guest kernel with the Rewind platform (nix/kernel.nix)
          kernel = p.kernel;
        }
      );

      checks = forAllSystems (
        system:
        let
          p = per system;
        in
        import ./nix/checks.nix { inherit (p) pkgs; }
      );

      devShells = forAllSystems (
        system:
        let
          p = per system;
        in
        import ./nix/dev-shells.nix { inherit (p) pkgs; }
      );

      formatter = forAllSystems (
        system: import ./nix/formatter.nix { pkgs = nixpkgs.legacyPackages.${system}; }
      );
    };
}
