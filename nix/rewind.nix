# `nix build .#rewind`: the rewind command, wrapped with the guest it boots
# and the tools it calls to build input images. nix itself is left to the
# user's PATH, so derivations resolve against the user's own store and
# settings.
{
  pkgs,
  rust,
  kernel,
  guest,
  nixseparatedebuginfod2,
  gdb,
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
      # Completions for bash, zsh and fish and a man page per command, from
      # the command's own definitions.
      nativeBuildInputs = [ pkgs.installShellFiles ];
      postInstall = ''
        installShellCompletion --cmd rewind \
          --bash <($out/bin/rewind generate completions bash) \
          --zsh <($out/bin/rewind generate completions zsh) \
          --fish <($out/bin/rewind generate completions fish)
        $out/bin/rewind generate man man
        installManPage man/*.1
      '';
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
  debuginfod = builtins.unsafeDiscardStringContext (lib.getExe nixseparatedebuginfod2);

  runtimeTools = [
    pkgs.erofs-utils
    pkgs.gnutar
  ];
in
pkgs.runCommand "rewind"
  {
    nativeBuildInputs = [
      pkgs.makeWrapper
      pkgs.lndir
    ];
    inherit (unwrapped) meta;
    passthru = { inherit unwrapped; };
  }
  ''
    mkdir -p $out/bin $out/share
    lndir -silent ${unwrapped}/share $out/share
    makeWrapper ${unwrapped}/bin/rewind $out/bin/rewind \
      --prefix PATH : ${lib.makeBinPath runtimeTools} \
      --set-default REWIND_KERNEL ${kernel}/bzImage \
      --set-default REWIND_INITRD ${guest.initrd}/initrd \
      --set-default REWIND_KERNEL_DEBUG ${kernelDebug} \
      --set-default REWIND_DEBUGINFOD ${debuginfod} \
      --set-default REWIND_GDB ${gdb}/bin/gdb
  ''
