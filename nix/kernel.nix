# `nix build .#kernel`: the guest kernel. Upstream Linux from nixpkgs, with
# the Rewind platform patches and a config of our own:
#   - x86_64: `guest/linux/config` merged over `x86_64_defconfig` (boots `vmlinux`)
#   - aarch64: `guest/linux/config-arm64` merged over `allnoconfig` with the
#     ARM64 Rewind platform (`guest/linux/arm64/rewind.c`, boots `Image` and
#     ships `vmlinux` alongside it for `rewind gdb`)
#
# A plain derivation rather than nixpkgs' kernel builder: that builder is
# shaped around distribution configs with thousands of modules, and this
# kernel has no modules at all.
#
# Two outputs of one build, so they describe the same code. `out` is what
# runs and `rewind gdb` start from: `vmlinux` (and `Image` on aarch64),
# `System.map`, `.config` and the kernel's gdb scripts (`lx-ps`, `lx-dmesg`
# and the rest). `debug` is nixpkgs' separateDebugInfo output: `vmlinux`'s
# DWARF under `lib/debug/.build-id`.
{ pkgs }:
let
  inherit (pkgs) lib;
  isAarch64 = pkgs.stdenv.hostPlatform.isAarch64;

  # On macOS Apple Silicon (`aarch64-darwin`), build the guest Linux kernel
  # via the aarch64-linux cross package set.
  kernelPkgs =
    if pkgs.stdenv.hostPlatform.isLinux then pkgs else pkgs.pkgsCross.aarch64-multiplatform;

  # A named series rather than linux_latest, so updating nixpkgs never
  # moves the guest to a new major version without the patch being ported.
  upstream = kernelPkgs.linux_7_2;

  x86Patches = [
    ../guest/linux/rewind-guest.patch
    # rewind.dirents=N: directories listed in the order of a SipHash of
    # each name keyed by N, as an ext4 made with another hash seed would
    ../guest/linux/rewind-dirents.patch
    # Read and write records, appended to a buffer the monitor drains at
    # steps it takes anyway
    ../guest/linux/rewind-io.patch
    # x87 and SSE marked in use in the init state and in every save, so
    # every kind of core saves the same bytes
    ../guest/linux/rewind-xstate.patch
    # A TLB flush where a write fault makes a page writable, so every kind
    # of core takes the same page faults
    ../guest/linux/rewind-tlb.patch
    # A record, and so an exit, at every context switch, and the
    # scheduler's hrtick off, which read the clock at every switch
    ../guest/linux/rewind-switch.patch
    # The trace events' formats left as compiled, rather than rewritten
    # for tracefs readers in a boot step that took more than half of a
    # short run's guest time
    ../guest/linux/rewind-trace.patch
    # The file events' path lock taken only by the system calls that
    # report a file event, not by every one that returns
    ../guest/linux/rewind-path-lock.patch
    # For rewind time: why the thread that left the CPU left it, and a
    # wake record for every wakeup, in the buffer the monitor drains
    ../guest/linux/rewind-time.patch
    # rewind.cpus=N in /proc/cpuinfo and sysfs's online, possible,
    # present and enabled lists, so every proc and sysfs a job mounts
    # says N, in any user namespace
    ../guest/linux/rewind-cpus.patch
  ];

  # Only the guest/linux/ subtree is needed to patch the ARM64 kernel, keeping
  # the derivation hash independent of Rust edits.
  guestLinuxSrc = lib.fileset.toSource {
    root = ../.;
    fileset = ../guest/linux;
  };
in
kernelPkgs.stdenv.mkDerivation {
  pname = "rewind-guest-kernel";
  separateDebugInfo = true;
  inherit (upstream) version src;
  patches = lib.optionals (!isAarch64) x86Patches;

  depsBuildBuild = [ kernelPkgs.buildPackages.stdenv.cc ];
  nativeBuildInputs = [
    kernelPkgs.buildPackages.bison
    kernelPkgs.buildPackages.flex
    kernelPkgs.buildPackages.perl
    kernelPkgs.buildPackages.bc
    kernelPkgs.buildPackages.openssl
    kernelPkgs.buildPackages.elfutils
    kernelPkgs.buildPackages.zstd
    kernelPkgs.buildPackages.python3
    kernelPkgs.buildPackages.hexdump
  ];

  # The kernel's own Makefiles hardcode /bin/pwd and friends.
  postPatch = ''
    ${lib.optionalString isAarch64 ''
      python3 ${guestLinuxSrc}/guest/linux/arm64/patch-kernel.py ${guestLinuxSrc} .
    ''}
    patchShebangs scripts
  '';

  # A fixed build identity, so the kernel's version banner does not change
  # between otherwise identical builds.
  env = {
    KBUILD_BUILD_USER = "rewind";
    KBUILD_BUILD_HOST = "rewind";
    KBUILD_BUILD_TIMESTAMP = "1970-01-01";
  };

  makeFlags = lib.optionals (
    isAarch64 && kernelPkgs.stdenv.buildPlatform != kernelPkgs.stdenv.hostPlatform
  ) [
    "ARCH=arm64"
    "CROSS_COMPILE=${kernelPkgs.stdenv.cc.targetPrefix}"
  ];

  configurePhase =
    if isAarch64 then
      ''
        runHook preConfigure
        make $makeFlags allnoconfig
        scripts/kconfig/merge_config.sh -m .config ${../guest/linux/config-arm64}
        make $makeFlags olddefconfig

        grep -qxF "CONFIG_REWIND_GUEST=y" .config
        grep -qxF "CONFIG_OF_PMEM=y" .config
        grep -qxF "CONFIG_EVENT_TRACING=y" .config
        runHook postConfigure
      ''
    else
      ''
        runHook preConfigure
        make x86_64_defconfig
        scripts/kconfig/merge_config.sh -m .config ${../guest/linux/config}
        make olddefconfig

        # merge_config warns and carries on when a symbol does not take. Here
        # that would mean a kernel that boots somewhere else than intended, so
        # every symbol the fragment sets must come out exactly as written.
        failed=0
        while IFS= read -r line; do
          case "$line" in
            CONFIG_*=*)
              grep -qxF "$line" .config || { echo "config did not take: $line"; failed=1; } ;;
            "# CONFIG_"*" is not set")
              sym=''${line#\# }; sym=''${sym%% *}
              if grep -q "^$sym=" .config; then echo "config did not take: $line"; failed=1; fi ;;
          esac
        done < ${../guest/linux/config}
        [ "$failed" = 0 ]
        runHook postConfigure
      '';

  buildPhase =
    if isAarch64 then
      ''
        runHook preBuild
        make $makeFlags -j$NIX_BUILD_CORES Image vmlinux scripts_gdb
        runHook postBuild
      ''
    else
      ''
        runHook preBuild
        make -j$NIX_BUILD_CORES vmlinux scripts_gdb
        runHook postBuild
      '';

  # vmlinux goes in whole: the separateDebugInfo hook moves its DWARF to
  # `debug` in the fixup phase, and the strip after it, told about
  # vmlinux here, leaves the symbol table.
  installPhase = ''
    runHook preInstall
    mkdir -p $out
    cp .config System.map vmlinux $out/
    ${lib.optionalString isAarch64 ''
      cp arch/arm64/boot/Image $out/Image
    ''}
    # The gdb scripts are the Python files in scripts/gdb, some of them
    # generated, next to the build's own files there.
    (cd scripts/gdb && find . -name '*.py' -exec install -D -m 444 {} $out/scripts/gdb/{} \;)
    cp -L vmlinux-gdb.py $out/vmlinux-gdb.py
    runHook postInstall
  '';
  stripDebugList = [ "vmlinux" ];

  # The hook's source overlay holds the files that differ from the
  # tarball, for gdb to show as built.
  postFixup =
    if isAarch64 then
      ''
        overlay=$debug/src/overlay/$sourceRoot
        rm -rf "$overlay"
        for file in \
          arch/arm64/kernel/rewind.c \
          arch/arm64/include/asm/rewind.h \
          arch/arm64/include/asm/hypervisor.h \
          arch/arm64/include/asm/fixmap.h \
          arch/arm64/include/asm/timex.h \
          arch/arm64/include/asm/irq_work.h \
          arch/arm64/kernel/setup.c \
          arch/arm64/kernel/irq.c \
          arch/arm64/kernel/time.c \
          arch/arm64/kernel/idle.c \
          arch/arm64/kernel/traps.c \
          arch/arm64/kernel/process.c \
          arch/arm64/kernel/cpuinfo.c \
          arch/arm64/kernel/smp.c; do
          install -D -m 444 "$file" "$overlay/$file"
        done
      ''
    else
      ''
        overlay=$debug/src/overlay/$sourceRoot
        rm -rf "$overlay"
        sed -n 's|^+++ b/||p' ${
          lib.concatMapStringsSep " " (patch: "${patch}") x86Patches
        } | cut -f1 | sort -u |
          while read -r file; do
            install -D -m 444 "$file" "$overlay/$file"
          done
      '';

  passthru = {
    bootImage = if isAarch64 then "Image" else "vmlinux";
  };

  meta = {
    description = "Linux for the Rewind VM guest";
    license = lib.licenses.gpl2Only;
    platforms = [
      "x86_64-linux"
      "aarch64-linux"
      "aarch64-darwin"
    ];
  };
}
