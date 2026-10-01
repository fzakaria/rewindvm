# The desktop app built to run on other distributions: linked against
# glibc 2.31 (Ubuntu 20.04, Debian 11, Fedora 32 and anything newer) with
# the system's /lib64 loader and no store paths, so it uses the system's
# libraries and, above all, its graphics drivers.
#
# A binary built against the store's glibc cannot load a distribution's
# Vulkan or OpenGL driver, which links against that distribution's glibc,
# libdrm and LLVM. So instead of bundling libraries, this one is linked
# with zig, through cargo-zigbuild, against glibc 2.31's symbol versions.
# The app links only three libraries besides glibc (libxcb, libxkbcommon
# and libxkbcommon-x11), which every Linux desktop has; Wayland, Vulkan
# and OpenGL it opens at run time. It links against the store's copies of
# those three, whose own glibc references are left to the system's copies
# at run time, hence --allow-shlib-undefined.
#
# nix/app-release.nix puts the result in the tarball, and the tarball's
# flake patches the same binary to run on NixOS.
{ pkgs }:
let
  inherit (pkgs) lib;
  fs = lib.fileset;

  # glibc 2.31 is the oldest the app is built for.
  target = "x86_64-unknown-linux-gnu";
  glibc = "2.31";
in
pkgs.stdenv.mkDerivation {
  pname = "rewind-app-portable";
  version = lib.fileContents ../VERSION;

  src = fs.toSource {
    root = ../.;
    fileset = fs.difference (fs.unions [
      ../Cargo.toml
      ../crates/rewind-app
      ../crates/rewind-trace
    ]) (fs.maybeMissing ../crates/rewind-app/target);
  };

  cargoDeps = pkgs.rustPlatform.importCargoLock {
    lockFile = ../crates/rewind-app/Cargo.lock;
  };
  cargoRoot = "crates/rewind-app";

  nativeBuildInputs = [
    pkgs.rustPlatform.cargoSetupHook
    pkgs.cargo
    pkgs.rustc
    # zig 0.16 compiles zstd's legacy decoders into objects that lld
    # rejects for undefined symbols without names; 0.15 links them. The
    # nixpkgs cargo-zigbuild puts its own zig first on PATH, so the older
    # zig goes in through its override.
    (pkgs.cargo-zigbuild.override { zig = pkgs.zig_0_15; })
    pkgs.pkg-config
  ];

  # The store's copies of the libraries the app links, for the linker only.
  # fontconfig and freetype are for the build scripts that look for them;
  # the app does not use them, and --as-needed leaves them out.
  buildInputs = [
    pkgs.libxcb
    pkgs.libxkbcommon
    pkgs.fontconfig
    pkgs.freetype
  ];

  buildPhase = ''
    runHook preBuild

    # zig and cargo-zigbuild keep caches and generated wrappers here.
    export HOME=$TMPDIR
    export XDG_CACHE_HOME=$TMPDIR/cache
    export ZIG_GLOBAL_CACHE_DIR=$TMPDIR/zig

    export RUSTFLAGS="-L native=${lib.getLib pkgs.libxcb}/lib -L native=${lib.getLib pkgs.libxkbcommon}/lib -C link-arg=-Wl,--allow-shlib-undefined"
    cd crates/rewind-app
    cargo zigbuild --release --offline --frozen --bin rewind-app --target ${target}.${glibc}

    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall
    install -D target/${target}/release/rewind-app $out/libexec/rewind-app
    strip $out/libexec/rewind-app
    runHook postInstall
  '';

  # The binary must keep the system loader and carry no store paths.
  dontPatchELF = true;
  dontStrip = true;
  doCheck = false;

  # The system's loader and no RUNPATH: anything else would tie the binary
  # to this machine's store.
  postFixup = ''
    readelf -l $out/libexec/rewind-app | grep -q "interpreter: /lib64/ld-linux-x86-64.so.2"
    if readelf -d $out/libexec/rewind-app | grep -q "RUNPATH\|RPATH"; then
      echo "the portable app has a RUNPATH" >&2
      exit 1
    fi
  '';

  meta = {
    description = "Rewind VM desktop app, built for other Linux distributions";
    mainProgram = "rewind-app";
    platforms = [ "x86_64-linux" ];
  };
}
