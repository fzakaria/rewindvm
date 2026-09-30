# The examples the tutorials use. mylib is a small C thread pool whose
# shutdown test fails only under some thread interleavings: the Nix
# tutorial builds it with `rewind check .#mylib` and scrubs the failure.
{ pkgs }:
{
  mylib = pkgs.stdenv.mkDerivation {
    pname = "mylib";
    version = "0.3.0";
    src = ../examples/mylib;
    makeFlags = [ "PREFIX=$(out)" ];
    doCheck = true;
    meta.description = "A thread pool with a shutdown race, for the Rewind VM tutorials";
  };
}
