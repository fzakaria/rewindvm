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

  # Guest userspace is always Linux (static musl matching the host CPU arch).
  # On macOS Apple Silicon (`aarch64-darwin`), cross-compile against aarch64-linux-musl.
  linuxPkgs =
    if pkgs.stdenv.hostPlatform.isLinux then pkgs else pkgs.pkgsCross.aarch64-multiplatform;
  staticPkgs = linuxPkgs.pkgsStatic;

  # The init is a workspace of its own, so nothing else in the tree
  # changes its source.
  src = lib.fileset.toSource {
    root = ../crates/rewind-init;
    fileset = lib.fileset.unions [
      ../crates/rewind-init/Cargo.toml
      ../crates/rewind-init/Cargo.lock
      ../crates/rewind-init/src
    ];
  };

  # nixpkgs' buildRustPackage rather than crane, which the other Rust
  # packages use (nix/crane.nix): crane's build embeds other paths for the
  # dependencies' sources, so the binary, and with it the initramfs whose
  # hash is part of every run's id, would change. The init has four
  # dependencies and is rebuilt only when the init itself changes.
  init = staticPkgs.rustPlatform.buildRustPackage {
    pname = "rewind-init";
    version = "0.1.0";
    inherit src;
    cargoLock.lockFile = ../crates/rewind-init/Cargo.lock;
    meta.mainProgram = "rewind-init";
  };

  # The init's lints (its Cargo.toml's [lints]), checked in a build of
  # its own for the host: clippy-driver in the init's build would make a
  # new initramfs, and every recording's guest with it, whenever clippy
  # changed. The init and its four dependencies compile in seconds.
  clippy = linuxPkgs.rustPlatform.buildRustPackage {
    pname = "rewind-init-clippy";
    version = "0.1.0";
    inherit src;
    cargoLock.lockFile = ../crates/rewind-init/Cargo.lock;
    nativeBuildInputs = [ linuxPkgs.clippy ];

    # The engine's clippy.toml, which lets tests unwrap.
    CLIPPY_CONF_DIR = lib.fileset.toSource {
      root = ../.;
      fileset = ../clippy.toml;
    };
    CARGO_BUILD_WARNINGS = "deny";
    buildPhase = ''
      runHook preBuild
      cargo clippy --all-targets --offline --locked
      runHook postBuild
    '';
    doCheck = false;
    installPhase = "touch $out";
  };

  # The shell Nix bind-mounts at /bin/sh in its sandbox, so builders see
  # the same one here. The static binary is copied in rather than linked
  # to its store path, so a run needs nothing from the store beyond the
  # derivation's own inputs, and a rewind installed without Nix works.
  sandboxShell = linuxPkgs.busybox-sandbox-shell;

  # For `rewind shell`: a static busybox whose ash has line editing,
  # history and completion, and its applets, which stand in for tools a
  # job's PATH lacks. rewind-init's TOOLS_DIR.
  tools = staticPkgs.busybox;

  # Host-runnable busybox for listing applets when cross-building initrd on darwin.
  hostBusybox = if pkgs.stdenv.hostPlatform.isLinux then tools else pkgs.busybox;

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
        mkdir -p root/rewind/tools/bin
        cp ${tools}/bin/busybox root/rewind/tools/busybox
        for applet in $(${hostBusybox}/bin/busybox --list); do
          ln -s ../busybox root/rewind/tools/bin/$applet
        done
        cp ${sandboxShell}/bin/busybox root/bin/sh
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
  inherit init initrd clippy;
}
