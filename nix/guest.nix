# The guest's user space: rewind-init, built static against musl so the
# initramfs needs nothing else, and the initramfs itself.
#
# The initramfs is laid out like the Nix build sandbox: /bin/sh is the
# sandbox shell, /etc/passwd and /etc/group name nixbld and nobody, and
# /build is the builder's home. The monitor appends one more archive at
# run time with the job to run. The archive here is uncompressed, since
# the guest would only spend exits decompressing it, and reproducible:
# every file owned by root with the same timestamp, in sorted order.
{ pkgs }:
let
  inherit (pkgs) lib;

  # The engine's workspace without the desktop app, which is a workspace
  # of its own with its own build directory.
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      (lib.fileset.difference ../crates ../crates/rewind-app)
    ];
  };

  init = pkgs.pkgsStatic.rustPlatform.buildRustPackage {
    pname = "rewind-init";
    version = "0.1.0";
    inherit src;
    cargoLock.lockFile = ../Cargo.lock;
    cargoBuildFlags = [
      "-p"
      "rewind-init"
    ];
    cargoTestFlags = [
      "-p"
      "rewind-init"
    ];
    meta.mainProgram = "rewind-init";
  };

  # The shell Nix bind-mounts at /bin/sh in its sandbox, so builders see
  # the same one here.
  sandboxShell = pkgs.busybox-sandbox-shell;

  passwd = pkgs.writeText "passwd" ''
    root:x:0:0:Nix build user:/build:/noshell
    nixbld:x:1000:100:Nix build user:/build:/noshell
    nobody:x:65534:65534:Nobody:/:/noshell
  '';
  group = pkgs.writeText "group" ''
    root:x:0:
    nixbld:!:100:
    nogroup:x:65534:
  '';
  hosts = pkgs.writeText "hosts" ''
    127.0.0.1 localhost
    ::1 localhost
  '';

  initrd =
    pkgs.runCommand "rewind-initrd"
      {
        nativeBuildInputs = [ pkgs.cpio ];
      }
      ''
        mkdir -p root/{dev,proc,sys,tmp,build,etc,bin,nix,rewind}
        cp ${init}/bin/rewind-init root/init
        ln -s ${sandboxShell}/bin/busybox root/bin/sh
        cp ${passwd} root/etc/passwd
        cp ${group} root/etc/group
        cp ${hosts} root/etc/hosts
        chmod 1777 root/tmp
        chmod -R u+w root
        find root -exec touch -h -d @1 {} +

        mkdir -p $out
        (cd root && find . | LC_ALL=C sort | cpio -o -H newc --reproducible -R 0:0 --quiet) > $out/initrd
      '';
in
{
  inherit init initrd sandboxShell;
}
