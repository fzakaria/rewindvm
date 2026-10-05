# `nix build .#rewind`: the rewind command, wrapped with the guest it boots
# and the tools it calls to build input images. nix itself is left to the
# user's PATH, so derivations resolve against the user's own store and
# settings.
{
  pkgs,
  rust,
  kernel,
  guest,
  commit,
}:
let
  inherit (pkgs) lib;

  # The workspace's dependencies, compiled once per Cargo.lock and set of
  # manifests (nix/crane.nix). The tests' dev-dependencies are among them.
  cargoArtifacts = rust.craneLib.buildDepsOnly (
    rust.engine
    // {
      pname = "rewind";
      cargoTestExtraArgs = "--no-run --workspace";
    }
  );

  unwrapped = rust.craneLib.buildPackage (
    rust.engine
    // {
      pname = "rewind";
      inherit cargoArtifacts;
      # The workspace's unit tests; the ones that need /dev/kvm run as flake
      # checks instead, where the sandbox can be given it.
      cargoTestExtraArgs = "--workspace";
      # The commit, for `rewind --version`; given to this build alone, so
      # the dependencies' build does not change with every commit.
      REWIND_COMMIT = commit;
      # erofs-utils for the test that reads a file out of an image.
      nativeCheckInputs = [
        pkgs.cpio
        pkgs.erofs-utils
      ];
      meta = {
        description = "Run Linux workloads in a deterministic VM, then scrub, rewind and fork them";
        mainProgram = "rewind";
        license = lib.licenses.mit;
        platforms = [ "x86_64-linux" ];
      };
    }
  );

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
