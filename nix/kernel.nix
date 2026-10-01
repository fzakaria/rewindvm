# `nix build .#kernel`: the guest kernel. Upstream Linux from nixpkgs, with
# the Rewind platform patch (guest/linux/rewind-guest.patch) and a config of
# our own (guest/linux/config) merged over x86_64_defconfig.
#
# A plain derivation rather than nixpkgs' kernel builder: that builder is
# shaped around distribution configs with thousands of modules, and this
# kernel has no modules at all.
#
# Two outputs of one build, so they describe the same code. `out` is what
# runs and `rewind gdb` start from: the bzImage the VM boots, vmlinux with
# its symbol table but no DWARF, System.map, the final .config and the
# kernel's gdb scripts (lx-ps, lx-dmesg and the rest). `debug` is
# nixpkgs' separateDebugInfo output: vmlinux's DWARF under
# lib/debug/.build-id, which gdb and debuginfod servers such as
# nixseparatedebuginfod2 find by build ID, with links to the source and
# the files the Rewind patch changes. Runs depend on `out` only, so the
# DWARF is fetched only by someone who debugs.
{ pkgs }:
let
  inherit (pkgs) lib;
  # A named series rather than linux_latest, so updating nixpkgs never
  # moves the guest to a new major version without the patch being ported.
  upstream = pkgs.linux_7_2;
in
pkgs.stdenv.mkDerivation {
  pname = "rewind-guest-kernel";
  separateDebugInfo = true;
  inherit (upstream) version src;

  patches = [ ../guest/linux/rewind-guest.patch ];

  depsBuildBuild = [ pkgs.buildPackages.stdenv.cc ];
  nativeBuildInputs = [
    pkgs.bison
    pkgs.flex
    pkgs.perl
    pkgs.bc
    pkgs.openssl
    pkgs.elfutils
    pkgs.zstd
    pkgs.python3
    pkgs.hexdump
  ];

  # The kernel's own Makefiles hardcode /bin/pwd and friends.
  postPatch = ''
    patchShebangs scripts
  '';

  # A fixed build identity, so the kernel's version banner does not change
  # between otherwise identical builds.
  env = {
    KBUILD_BUILD_USER = "rewind";
    KBUILD_BUILD_HOST = "rewind";
    KBUILD_BUILD_TIMESTAMP = "1970-01-01";
  };

  configurePhase = ''
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

  buildPhase = ''
    runHook preBuild
    make -j$NIX_BUILD_CORES bzImage vmlinux scripts_gdb
    runHook postBuild
  '';

  # vmlinux goes in whole: the separateDebugInfo hook moves its DWARF to
  # `debug` in the fixup phase, and the strip after it, told about
  # vmlinux here, leaves the symbol table.
  installPhase = ''
    runHook preInstall
    mkdir -p $out
    cp arch/x86/boot/bzImage .config System.map vmlinux $out/
    # The gdb scripts are the Python files in scripts/gdb, some of them
    # generated, next to the build's own files there.
    (cd scripts/gdb && find . -name '*.py' -exec install -D -m 444 {} $out/scripts/gdb/{} \;)
    cp -L vmlinux-gdb.py $out/vmlinux-gdb.py
    runHook postInstall
  '';
  stripDebugList = [ "vmlinux" ];

  # The hook's source overlay holds the files that differ from the
  # tarball, for gdb to show as built. It finds them by checksum, so it
  # misses the files the patch adds, rewind.c among them, and takes in the
  # scripts patchShebangs changed, which would make `debug` depend on perl
  # and python. The overlay is made the patch's files instead.
  postFixup = ''
    overlay=$debug/src/overlay/$sourceRoot
    rm -rf "$overlay"
    sed -n 's|^+++ b/||p' ${../guest/linux/rewind-guest.patch} | cut -f1 |
      while read -r file; do
        install -D -m 444 "$file" "$overlay/$file"
      done
  '';

  meta = {
    description = "Linux for the Rewind VM guest";
    license = lib.licenses.gpl2Only;
    platforms = [ "x86_64-linux" ];
  };
}
