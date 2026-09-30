# The shell behind `nix develop`: the Rust toolchain from nixpkgs for
# iterating with `cargo build` and `cargo test`, plus the musl target the
# guest init is built for.
{ pkgs }:
{
  default = pkgs.mkShell {
    packages = [
      pkgs.cargo
      pkgs.rustc
      pkgs.rustfmt
      pkgs.clippy
      pkgs.pkg-config
      pkgs.erofs-utils
      pkgs.python3
    ];
  };
}
