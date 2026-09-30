# The example tarball's own flake, shipped as its flake.nix, so the
# tarball's URL is a flake reference for the tutorials:
#
#   nix build https://rewindvm.dev/download/mylib-example.tar.gz
#   rewind nix https://rewindvm.dev/download/mylib-example.tar.gz
#
# Unpacked, the same directory is a flake too, so `rewind check .` checks
# a local edit. nixpkgs is locked by the flake.lock shipped next to this
# file, which is the repository's own.
{
  description = "mylib: a thread pool with a shutdown race, for the Rewind VM tutorials";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};

      # The name the repository's examples/mylib gets in the store. The
      # source must have the same name and contents for the derivation to
      # be the same as the repository's.
      sourceName = "mylib";

      # Files in this directory that are not mylib's source: this flake,
      # its lock, the derivation it imports, and `nix build` result links.
      flakeFiles = [
        "flake.nix"
        "flake.lock"
        "mylib.nix"
      ];
      resultLink = "result";

      # mylib's source is this directory without the files above.
      src = builtins.path {
        name = sourceName;
        path = ./.;
        filter =
          path: _type:
          let
            name = baseNameOf path;
            topLevel = dirOf path == toString ./.;
          in
          !(topLevel && (builtins.elem name flakeFiles || pkgs.lib.hasPrefix resultLink name));
      };
    in
    {
      packages.${system}.default = import ./mylib.nix { inherit pkgs src; };
    };
}
