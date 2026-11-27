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
  isDarwin = pkgs.stdenv.hostPlatform.isDarwin;
  bootImage = kernel.bootImage or "vmlinux";

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
    // rust.clippy
    // {
      pname = "rewind";
      inherit cargoArtifacts;
      # The workspace's unit tests, linted as they compile (nix/crane.nix);
      # the ones that need /dev/kvm or Hypervisor.framework entitlement run
      # outside the sandbox.
      cargoTestExtraArgs = "--workspace" + lib.optionalString isDarwin " -- --skip erofs --skip image::tests --skip maps::tests";
      # The commit, for `rewind --version`; given to this build alone, so
      # the dependencies' build does not change with every commit.
      REWIND_COMMIT = commit;
      # Completions for bash, zsh and fish and a man page per command, from
      # the command's own definitions.
      nativeBuildInputs = [
        pkgs.installShellFiles
      ]
      ++ lib.optionals isDarwin [ pkgs.darwin.sigtool ];
      postInstall = ''
        ${lib.optionalString isDarwin ''
          codesign --sign - --entitlements ${../crates/rewind/entitlements.plist} --force $out/bin/rewind
        ''}
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
      ]
      ++ lib.optionals (!isDarwin) [ pkgs.erofs-utils ];
      meta = {
        description = "Run Linux workloads in a deterministic VM, then scrub, rewind and fork them";
        mainProgram = "rewind";
        license = lib.licenses.mit;
        platforms = [
          "x86_64-linux"
          "aarch64-linux"
          "aarch64-darwin"
        ];
      };
    }
  );

  # What only `rewind gdb` needs, named without depending on it, so the
  # package's closure stays without them: the kernel's DWARF, which runs
  # record the path of, and nixseparatedebuginfod2, the debuginfod server
  # that hands gdb DWARF and source files from the store and
  # cache.nixos.org. `rewind gdb` fetches each the first time it runs.
  kernelDebug = builtins.unsafeDiscardStringContext "${kernel.debug}";
  debuginfod =
    if nixseparatedebuginfod2 != null then
      builtins.unsafeDiscardStringContext (lib.getExe nixseparatedebuginfod2)
    else
      null;

  runtimeTools = [
    pkgs.gnutar
  ]
  ++ lib.optionals (!isDarwin) [ pkgs.erofs-utils ];
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
    # Builds use the guest pinned by this package, independent of the
    # environment overrides that interactive runs accept.
    mkdir -p $out/share/rewind
    ln -s ${kernel}/${bootImage} $out/share/rewind/${bootImage}
    ${lib.optionalString (bootImage != "vmlinux") ''
      ln -s ${kernel}/vmlinux $out/share/rewind/vmlinux
    ''}
    ln -s ${guest.initrd}/initrd $out/share/rewind/initrd
    makeWrapper ${unwrapped}/bin/rewind $out/bin/rewind \
      --prefix PATH : ${lib.makeBinPath runtimeTools} \
      --set REWIND_RUNTIME $out \
      --set-default REWIND_KERNEL ${kernel}/${bootImage} \
      --set-default REWIND_INITRD ${guest.initrd}/initrd \
      --set-default REWIND_KERNEL_DEBUG ${kernelDebug}${lib.optionalString (debuginfod != null) " \\\n      --set-default REWIND_DEBUGINFOD ${debuginfod}"}${lib.optionalString (gdb != null) " \\\n      --set-default REWIND_GDB ${gdb}/bin/gdb"}
  ''
