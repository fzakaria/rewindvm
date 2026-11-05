# `nix build .#bench-inputs`: what tools/bench.py runs every Rewind build
# against. The inputs are fixed by this checkout, not by the build being
# measured, so every release and every commit runs the same programs on
# the same data.
#
# root/ is a root filesystem for `rewind run`: static busybox, pingpong
# (nix/bench/pingpong.c) and a source tarball to unpack. hello.drv names
# GNU hello's derivation for `rewind nix`; the closure holds the
# derivation's inputs, built, so that a benchmark of `rewind nix` never
# waits on a download.
{ pkgs }:
let
  # The busybox applets the workloads call.
  applets = [
    "sh"
    "echo"
    "true"
    "awk"
    "tar"
    "xz"
    "find"
    "wc"
    "mkdir"
  ];

  root =
    pkgs.runCommand "rewind-bench-root"
      {
        nativeBuildInputs = [ pkgs.pkgsStatic.stdenv.cc ];
      }
      ''
        mkdir -p $out/bin $out/data $out/tmp
        cp ${pkgs.pkgsStatic.busybox}/bin/busybox $out/bin/
        for applet in ${pkgs.lib.concatStringsSep " " applets}; do
          ln -s busybox $out/bin/$applet
        done
        $CC -static -O2 -pthread -o $out/bin/pingpong ${./pingpong.c}
        cp ${pkgs.coreutils.src} $out/data/src.tar.xz
      '';

  # Without structured attributes, which releases before 0.3.1 cannot
  # pass to a build; the build is otherwise nixpkgs' own.
  helloPackage = pkgs.hello.overrideAttrs { __structuredAttrs = false; };
  hello = builtins.unsafeDiscardOutputDependency helloPackage.drvPath;
in
pkgs.runCommand "rewind-bench-inputs" { } ''
  mkdir -p $out
  ln -s ${root} $out/root
  echo ${hello} > $out/hello.drv
  # Holds hello's build inputs in the closure.
  ln -s ${helloPackage.inputDerivation} $out/hello-inputs
''
