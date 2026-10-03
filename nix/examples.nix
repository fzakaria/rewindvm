# The examples the tutorials use. mylib is a small C thread pool whose
# shutdown test fails only under some thread interleavings: the Nix
# tutorial builds it with `rewind check` and scrubs the failure.
# philosophers is the dining philosophers, whose check deadlocks under
# some interleavings and is ended by a timeout.
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

  philosophers = pkgs.stdenv.mkDerivation {
    pname = "philosophers";
    version = "0.1.0";
    src = ../examples/philosophers;
    makeFlags = [ "PREFIX=$(out)" ];
    doCheck = true;
    meta.description = "The dining philosophers, deadlocking under some interleavings";
  };
}
