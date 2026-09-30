# The release tarball's own flake, shipped as its flake.nix, so the
# tarball's URL is a flake reference:
#
#   nix run https://rewindvm.dev/download/rewind-0.1.0-x86_64-linux.tar.gz -- pmu status
#
# Nothing here is built from source. The package links the tarball's
# launcher, which finds the static rewind, the guest and the tools beside
# it. nixpkgs is locked by the flake.lock shipped next to this file.
{
  description = "Rewind VM: deterministic Linux VMs you can scrub, rewind and fork";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};

      rewind = pkgs.runCommand "rewind-0.1.0" {
        meta = {
          description = "Run Linux workloads in a deterministic VM, then scrub, rewind and fork them";
          mainProgram = "rewind";
          platforms = [ system ];
        };
      } "mkdir -p $out/bin && ln -s ${self}/bin/rewind $out/bin/rewind";
    in
    {
      packages.${system}.default = rewind;

      nixosModules.default = import ./module.nix { inherit rewind; };
    };
}
