# The examples the tutorials use. mylib is a small C thread pool whose
# shutdown test fails only under some thread interleavings: the Nix
# tutorial builds it with `rewind check` and scrubs the failure. Its
# derivation is nix/mylib.nix, which the example tarball shares.
{ pkgs }:
{
  mylib = import ./mylib.nix {
    inherit pkgs;
    src = ../examples/mylib;
  };
}
