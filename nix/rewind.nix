# `nix build .#rewind`: the rewind command, wrapped with the guest it boots
# and the tools it calls to build input images. nix itself is left to the
# user's PATH, so derivations resolve against the user's own store and
# settings.
{
  pkgs,
  kernel,
  guest,
}:
let
  inherit (pkgs) lib;

  unwrapped = pkgs.rustPlatform.buildRustPackage {
    pname = "rewind";
    # VERSION, the one place a release's version is written.
    version = lib.fileContents ../VERSION;
    # The engine's workspace without the desktop app, which is a workspace
    # of its own, and without build directories.
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
    # The workspace's unit tests; the ones that need /dev/kvm run as flake
    # checks instead, where the sandbox can be given it.
    cargoTestFlags = [ "--workspace" ];
    nativeCheckInputs = [ pkgs.cpio ];
    meta = {
      description = "Run Linux workloads in a deterministic VM, then scrub, rewind and fork them";
      mainProgram = "rewind";
      license = lib.licenses.mit;
      platforms = [ "x86_64-linux" ];
    };
  };

  # What only `rewind gdb` needs, named without depending on it, so the
  # package's closure stays without them: the kernel's DWARF, which runs
  # record the path of, and nixseparatedebuginfod2, the debuginfod server
  # that hands gdb DWARF and source files from the store and
  # cache.nixos.org. `rewind gdb` fetches each the first time it runs.
  kernelDebug = builtins.unsafeDiscardStringContext "${kernel.debug}";
  debuginfod = builtins.unsafeDiscardStringContext (lib.getExe pkgs.nixseparatedebuginfod2);

  # gdb comes after the user's own PATH, for `rewind gdb`, so a gdb the
  # user prefers wins.
  runtimeTools = [
    pkgs.erofs-utils
    pkgs.gnutar
  ];
in
pkgs.runCommand "rewind"
  {
    nativeBuildInputs = [ pkgs.makeWrapper ];
    inherit (unwrapped) meta;
    passthru = { inherit unwrapped; };
  }
  ''
    mkdir -p $out/bin
    makeWrapper ${unwrapped}/bin/rewind $out/bin/rewind \
      --prefix PATH : ${lib.makeBinPath runtimeTools} \
      --suffix PATH : ${lib.makeBinPath [ pkgs.gdb ]} \
      --set-default REWIND_KERNEL ${kernel}/bzImage \
      --set-default REWIND_INITRD ${guest.initrd}/initrd \
      --set-default REWIND_KERNEL_DEBUG ${kernelDebug} \
      --set-default REWIND_DEBUGINFOD ${debuginfod}
  ''
