# `nix build .#release`: rewind for people without Nix. A tarball with a
# static rewind, the guest kernel and initramfs, and a launcher that points
# rewind at them. The host needs only /dev/kvm and mkfs.erofs from its own
# erofs-utils package; `rewind nix` additionally needs nix on PATH.
{
  pkgs,
  kernel,
  guest,
}:
let
  inherit (pkgs) lib;

  static = pkgs.pkgsStatic.rustPlatform.buildRustPackage {
    pname = "rewind-static";
    version = "0.1.0";
    src = lib.fileset.toSource {
      root = ../.;
      fileset = lib.fileset.unions [
        ../Cargo.toml
        ../Cargo.lock
        (lib.fileset.difference ../crates (
          lib.fileset.unions [
            ../crates/rewind-app
            (lib.fileset.maybeMissing ../crates/rewind-init/target)
          ]
        ))
      ];
    };
    cargoLock.lockFile = ../Cargo.lock;
    cargoBuildFlags = [
      "-p"
      "rewind"
    ];
    # The unit tests run in the dynamically linked package.
    doCheck = false;
  };

  launcher = pkgs.writeText "rewind" ''
    #!/bin/sh
    # Points rewind at the guest shipped next to it, unless the environment
    # already names one.
    here=$(dirname "$(readlink -f "$0")")
    : "''${REWIND_KERNEL:=$here/../share/rewind/bzImage}"
    : "''${REWIND_INITRD:=$here/../share/rewind/initrd}"
    export REWIND_KERNEL REWIND_INITRD
    exec "$here/../libexec/rewind" "$@"
  '';
in
pkgs.runCommand "rewind-release" { } ''
  dir=rewind-0.1.0-x86_64-linux
  mkdir -p $dir/bin $dir/libexec $dir/share/rewind
  cp ${static}/bin/rewind $dir/libexec/rewind
  install -m 755 ${launcher} $dir/bin/rewind
  cp ${kernel}/bzImage ${guest.initrd}/initrd $dir/share/rewind/
  cp ${../LICENSE} $dir/LICENSE
  mkdir -p $out
  tar --sort=name --owner=0 --group=0 --numeric-owner --mtime=@1 \
    -czf $out/$dir.tar.gz $dir
''
