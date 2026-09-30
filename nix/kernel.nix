# `nix build .#kernel`: the guest kernel. Upstream Linux from nixpkgs, with
# the Rewind platform patch (guest/linux/rewind-guest.patch) and a config of
# our own (guest/linux/config) merged over x86_64_defconfig.
#
# A plain derivation rather than nixpkgs' kernel builder: that builder is
# shaped around distribution configs with thousands of modules, and this
# kernel has no modules at all. The outputs are the bzImage the monitor
# boots, vmlinux for symbolized backtraces and gdb, and the final .config.
{ pkgs }:
let
  inherit (pkgs) lib;
  # A named series rather than linux_latest, so updating nixpkgs never
  # moves the guest to a new major version without the patch being ported.
  upstream = pkgs.linux_7_2;
in
pkgs.stdenv.mkDerivation {
  pname = "rewind-guest-kernel";
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
    make -j$NIX_BUILD_CORES bzImage vmlinux
    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall
    mkdir -p $out
    cp arch/x86/boot/bzImage vmlinux .config System.map $out/
    runHook postInstall
  '';

  meta = {
    description = "Linux for the Rewind VM guest";
    license = lib.licenses.gpl2Only;
    platforms = [ "x86_64-linux" ];
  };
}
