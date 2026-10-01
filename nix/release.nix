# `nix build .#release`: the command's release asset, for people without
# Nix. It holds a static rewind, the guest kernel and initramfs, the
# kernel's vmlinux and gdb scripts for `rewind gdb`, static mkfs.erofs and
# GNU tar for building input images, and a launcher that points rewind at
# all of them. The host needs only /dev/kvm; `rewind nix`
# also needs nix on PATH.
#
# The file and the directory it unpacks to carry no version, so the
# release workflow's assets have the same URL under releases/latest.
{
  pkgs,
  kernel,
  guest,
}:
let
  inherit (pkgs) lib;

  static = pkgs.pkgsStatic.rustPlatform.buildRustPackage {
    pname = "rewind-static";
    version = lib.fileContents ../VERSION;
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

  # Named for `rewind gdb` without being shipped: on a host with Nix, the
  # kernel's DWARF and the debuginfod server are fetched from the binary
  # caches the first time someone debugs.
  kernelDebug = builtins.unsafeDiscardStringContext "${kernel.debug}";
  debuginfod = builtins.unsafeDiscardStringContext (lib.getExe pkgs.nixseparatedebuginfod2);

  launcher = pkgs.writeText "rewind" ''
    #!/bin/sh
    # Points rewind at the guest shipped next to it, unless the environment
    # already names one, and puts the shipped tools first on PATH.
    here=$(dirname "$(readlink -f "$0")")
    PATH="$here/../libexec/rewind-tools:$PATH"
    export PATH
    : "''${REWIND_KERNEL:=$here/../share/rewind/bzImage}"
    : "''${REWIND_INITRD:=$here/../share/rewind/initrd}"
    : "''${REWIND_KERNEL_DEBUG:=${kernelDebug}}"
    : "''${REWIND_DEBUGINFOD:=${debuginfod}}"
    export REWIND_KERNEL REWIND_INITRD REWIND_KERNEL_DEBUG REWIND_DEBUGINFOD
    exec "$here/../libexec/rewind" "$@"
  '';
in
pkgs.runCommand "rewind-release" { } ''
  dir=rewind-x86_64-linux
  mkdir -p $dir/bin $dir/libexec $dir/share/rewind
  cp ${static}/bin/rewind $dir/libexec/rewind
  mkdir -p $dir/libexec/rewind-tools
  cp ${pkgs.pkgsStatic.erofs-utils}/bin/mkfs.erofs ${staticTar}/bin/tar \
    $dir/libexec/rewind-tools/
  install -m 755 ${launcher} $dir/bin/rewind
  cp -r ${kernel}/bzImage ${kernel}/vmlinux ${kernel}/vmlinux-gdb.py ${kernel}/scripts \
    ${guest.initrd}/initrd $dir/share/rewind/
  chmod -R u+w $dir/share/rewind
  cp ${../LICENSE} $dir/LICENSE
  mkdir -p $out
  tar --sort=name --owner=0 --group=0 --numeric-owner --mtime=@1 \
    -czf $out/$dir.tar.gz $dir
''
