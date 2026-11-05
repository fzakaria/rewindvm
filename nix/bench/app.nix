# `nix run .#bench -- ...`: tools/bench.py with GNU time and taskset on
# PATH and the benchmark inputs (nix/bench) passed as its
# --inputs. Nix stays the host's own: `rewind nix` asks it to evaluate and
# realise, as it does for anyone.
{ pkgs, inputs }:
pkgs.writeShellApplication {
  name = "rewind-bench";
  runtimeInputs = [
    pkgs.python3
    pkgs.time
    pkgs.util-linux
  ];
  text = ''
    exec python3 ${../../tools/bench.py} --inputs ${inputs} "$@"
  '';
}
