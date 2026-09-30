# mylib's derivation, given its source. The repository builds it from
# examples/mylib (nix/examples.nix), and the example tarball's flake
# (nix/example-flake.nix) ships this file as mylib.nix and builds it from
# the unpacked tarball. Both pass the same source under the same name, so
# both evaluate to the same derivation.
{ pkgs, src }:
pkgs.stdenv.mkDerivation {
  pname = "mylib";
  version = "0.3.0";
  inherit src;
  makeFlags = [ "PREFIX=$(out)" ];
  doCheck = true;
  meta.description = "A thread pool with a shutdown race, for the Rewind VM tutorials";
}
