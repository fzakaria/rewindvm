# The examples the tutorials use. mylib is a small C thread pool whose
# shutdown test fails only under some thread interleavings: the Nix
# tutorial builds it with `rewind check` and scrubs the failure.
# philosophers is the dining philosophers, whose check deadlocks under
# some interleavings and is ended by a timeout. bank loses a deposit to
# two threads racing on one balance, config-reload has a reader process
# see a config half rewritten, and waiter's parent sleeps through the
# signal that should wake it; each fails only under some interleavings.
# hash-order's test expects a HashSet's order, which Rust draws from the
# system's randomness, so it fails only under some seeds. bank-java is bank
# as a JUnit 5 suite marked with the Rewind SDK's extension. counter loses
# a hit to two threads racing with no system call between read and store,
# which only a schedule point reaches. tiles writes its pkg-config file's
# Requires line from a Python set, in an order drawn from the hash seed,
# so its output differs between seeds. plugins writes an index of its
# plugins in the order the directory lists them, so its output differs
# between directory orders. The reproducible builds tutorial finds both.
# stamp compiles a random UUID and the time of its build into its program,
# so no two builds of it on an ordinary machine are the same.
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

  bank = pkgs.stdenv.mkDerivation {
    pname = "bank";
    version = "0.1.0";
    src = ../examples/bank;
    makeFlags = [ "PREFIX=$(out)" ];
    doCheck = true;
    meta.description = "Two tellers and one account, losing a deposit under some interleavings";
  };

  counter = pkgs.stdenv.mkDerivation {
    pname = "counter";
    version = "0.1.0";
    src = ../examples/counter;
    makeFlags = [ "PREFIX=$(out)" ];
    doCheck = true;
    meta.description = "Two threads and one count, losing a hit only at a schedule point";
  };

  bank-java = pkgs.stdenv.mkDerivation {
    pname = "bank-java";
    version = "0.1.0";
    src = ../examples/bank-java;
    nativeBuildInputs = [ pkgs.jdk_headless ];
    makeFlags = [
      "PREFIX=$(out)"
      "JUNIT=${import ./junit.nix { inherit pkgs; }}"
    ];
    doCheck = true;
    meta.description = "bank as a JUnit 5 suite, losing a deposit under some interleavings";
  };

  config-reload = pkgs.stdenv.mkDerivation {
    pname = "config-reload";
    version = "0.1.0";
    src = ../examples/config-reload;
    makeFlags = [ "PREFIX=$(out)" ];
    doCheck = true;
    meta.description = "A config rewritten in place, read half written under some interleavings";
  };

  hash-order = pkgs.stdenv.mkDerivation {
    pname = "hash-order";
    version = "0.1.0";
    src = ../examples/hash-order;
    nativeBuildInputs = [ pkgs.rustc ];
    makeFlags = [ "PREFIX=$(out)" ];
    doCheck = true;
    meta.description = "A test that expects a HashSet's order, failing under some seeds";
  };

  waiter = pkgs.stdenv.mkDerivation {
    pname = "waiter";
    version = "0.1.0";
    src = ../examples/waiter;
    makeFlags = [ "PREFIX=$(out)" ];
    doCheck = true;
    meta.description = "A parent waiting with pause, sleeping through its wakeup under some interleavings";
  };

  stamp = pkgs.stdenv.mkDerivation {
    pname = "stamp";
    version = "0.1.0";
    src = ../examples/stamp;
    makeFlags = [ "PREFIX=$(out)" ];
    meta.description = "A program stamped with a random id and the time of its build";
    meta.mainProgram = "stamp";
  };

  tiles = pkgs.stdenv.mkDerivation {
    pname = "tiles";
    version = "0.2.0";
    src = ../examples/tiles;
    nativeBuildInputs = [ pkgs.python3 ];
    makeFlags = [ "PREFIX=$(out)" ];
    # nixpkgs' Python setup hook exports PYTHONHASHSEED=0 in any build
    # with Python as an input. A build that runs Python without that hook
    # draws a new hash seed every time, which this unset stands for.
    preBuild = "unset PYTHONHASHSEED";
    meta.description = "A pkg-config file written from a Python set, differing between seeds";
  };

  plugins = pkgs.stdenv.mkDerivation {
    pname = "plugins";
    version = "0.1.0";
    src = ../examples/plugins;
    nativeBuildInputs = [ pkgs.python3 ];
    makeFlags = [ "PREFIX=$(out)" ];
    meta.description = "An index written in directory order, differing between directory orders";
  };
}
