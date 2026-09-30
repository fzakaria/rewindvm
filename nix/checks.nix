# `nix flake check`: everything CI asserts, offline.
{ pkgs }:
{
  # House style, checked rather than remembered: no em dashes and none of
  # the vocabulary that marks machine written prose, in docs, site copy and
  # code comments alike. Runs over a copy of the whole tree.
  prose = pkgs.runCommand "rewind-prose" { nativeBuildInputs = [ pkgs.python3 ]; } ''
    cp -r ${../.} tree
    chmod -R u+w tree
    python3 tree/tools/check-prose.py
    touch $out
  '';
}
