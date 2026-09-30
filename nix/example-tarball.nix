# `nix build .#example`: the tutorials' example as a tarball, which the
# site serves at download/mylib-example.tar.gz while the repository is
# private. It unpacks to mylib/: the source and Containerfile from
# examples/mylib, and a flake (nix/example-flake.nix, with nix/mylib.nix
# and this repository's flake.lock) whose default package is the same
# derivation as `.#mylib`.
{ pkgs }:
let
  # The tarball's name and the directory it unpacks to.
  name = "mylib-example";
  dir = "mylib";
in
pkgs.runCommand "rewind-${name}" { } ''
  # The example's source, then the flake beside it.
  cp -r ${../examples/mylib} ${dir}
  chmod -R u+w ${dir}
  install -m 644 ${./example-flake.nix} ${dir}/flake.nix
  install -m 644 ${../flake.lock} ${dir}/flake.lock
  install -m 644 ${./mylib.nix} ${dir}/mylib.nix

  # Sorted, with fixed owners and times, so the tarball only changes when
  # its contents do.
  mkdir -p $out
  tar --sort=name --owner=0 --group=0 --numeric-owner --mtime=@1 \
    -czf $out/${name}.tar.gz ${dir}
''
