# `nix build .#release`: the tarball rewindvm.dev serves. It holds a
# static rewind, the guest kernel and initramfs, static mkfs.erofs and GNU
# tar for building input images, and a launcher that points rewind at all
# of them. The host needs only /dev/kvm; `rewind nix` also needs nix on
# PATH.
#
# The tarball is a flake too (nix/release-flake.nix, with this
# repository's flake.lock), so Nix users can run or install it by URL and
# import its NixOS module (nix/module.nix) without access to the source.
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

  # Static GNU tar fails to link against static libacl, which defines the
  # same xattr helpers; images are built from the store, which has no ACLs.
  # The override rebuilds tar from source, and its test suite writes sparse
  # files larger than a CI runner's free disk, so the tests are skipped.
  staticTar = (pkgs.pkgsStatic.gnutar.override { acl = null; }).overrideAttrs { doCheck = false; };

  launcher = pkgs.writeText "rewind" ''
    #!/bin/sh
    # Points rewind at the guest shipped next to it, unless the environment
    # already names one, and puts the shipped tools first on PATH.
    here=$(dirname "$(readlink -f "$0")")
    PATH="$here/../libexec/rewind-tools:$PATH"
    export PATH
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
  mkdir -p $dir/libexec/rewind-tools
  cp ${pkgs.pkgsStatic.erofs-utils}/bin/mkfs.erofs ${staticTar}/bin/tar \
    $dir/libexec/rewind-tools/
  install -m 755 ${launcher} $dir/bin/rewind
  cp ${kernel}/bzImage ${guest.initrd}/initrd $dir/share/rewind/
  cp ${../LICENSE} $dir/LICENSE
  cp ${./release-flake.nix} $dir/flake.nix
  cp ${../flake.lock} $dir/flake.lock
  cp ${./module.nix} $dir/module.nix
  mkdir -p $out
  tar --sort=name --owner=0 --group=0 --numeric-owner --mtime=@1 \
    -czf $out/$dir.tar.gz $dir
''
