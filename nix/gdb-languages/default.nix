# A root filesystem for checks.gdb-languages: a program in each of C++,
# Rust and Go, built with their DWARF, and a Python and a Ruby script, run
# one after another by /bin/languages. The store paths they need are copied
# into the root's /nix/store, where the VM finds them at the same paths as
# this machine, so `rewind gdb` loads them from the store as it would a
# Nix build's.
{ pkgs }:
let
  cpp = pkgs.runCommandCC "gdb-languages-cpp" { dontStrip = true; } ''
    mkdir -p $out/bin
    $CXX -g -O0 -pthread ${./greet.cpp} -o $out/bin/greet-cpp
  '';

  rust =
    pkgs.runCommand "gdb-languages-rust"
      {
        nativeBuildInputs = [
          pkgs.rustc
          pkgs.stdenv.cc
        ];
        dontStrip = true;
      }
      ''
        mkdir -p $out/bin
        rustc -g -C opt-level=0 --crate-name greet ${./greet.rs} -o $out/bin/greet-rust
      '';

  go =
    pkgs.runCommand "gdb-languages-go"
      {
        nativeBuildInputs = [ pkgs.go ];
        dontStrip = true;
      }
      ''
        mkdir -p $out/bin
        export HOME=$TMPDIR GOCACHE=$TMPDIR/cache GO111MODULE=off CGO_ENABLED=0
        cp ${./greet.go} greet.go
        go build -gcflags='all=-N -l' -o $out/bin/greet-go greet.go
      '';

  languages = pkgs.writeShellScript "languages" ''
    ${cpp}/bin/greet-cpp
    ${rust}/bin/greet-rust
    ${go}/bin/greet-go
    ${pkgs.python3}/bin/python3 ${./greet.py}
    ${pkgs.ruby}/bin/ruby ${./greet.rb}
  '';

  closure = pkgs.closureInfo { rootPaths = [ languages ]; };
in
pkgs.runCommand "rewind-gdb-languages-root" { } ''
  mkdir -p $out/bin $out/nix/store
  while read -r path; do
    cp -a "$path" $out/nix/store/
  done < ${closure}/store-paths
  ln -s ${languages} $out/bin/languages
''
