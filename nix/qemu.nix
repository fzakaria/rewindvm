# Rewind's machine on QEMU's emulator, which replays exit-time runs
# without KVM: qemu-system-x86_64 with `-machine rewind` (qemu/patches,
# GPL-2.0), and rewind-qemu, which starts it for a recorded run and
# compares the trace it makes with the run's.
#
# QEMU is the qemu-wasm fork at the commit trynix builds its browser
# engine from, a fork of QEMU 8.2.0, rather than nixpkgs' QEMU. The
# browser build and this one then emulate the guest with the same TCG
# front end, so a replay that matches here has no newer or older
# emulator to differ from in the browser. The fork's wasm backend is
# not compiled into a native build.
{ pkgs, rust }:
let
  inherit (pkgs) lib;

  # ktock/qemu-wasm, as trynix's nix/native-qemu.nix pins it.
  qemuWasmRev = "0ef7b4e2814b231705d8371dd7997f5b72e70baf";
  src = pkgs.fetchFromGitHub {
    owner = "ktock";
    repo = "qemu-wasm";
    rev = qemuWasmRev;
    hash = "sha256-7tXHhucQnJPQSGV1KK8cC+8QeVRVLSUYU9v9fkm9N2I=";
  };

  # The meson wrap subprojects the build reads, at the revisions the
  # fork's subprojects/*.wrap files name; the sandbox has no network.
  wrap =
    name: rev: hash:
    pkgs.fetchgit {
      url = "https://gitlab.com/qemu-project/${name}.git";
      inherit rev hash;
    };
  subprojects = {
    keycodemapdb =
      wrap "keycodemapdb" "f5772a62ec52591ff6870b7e8ef32482371f22c6"
        "sha256-EQrnBAXQhllbVCHpOsgREzYGncMUPEIoWFGnjo+hrH4=";
    berkeley-softfloat-3 =
      wrap "berkeley-softfloat-3" "b64af41c3276f97f0e181920400ee056b9c88037"
        "sha256-Yflpx+mjU8mD5biClNpdmon24EHg4aWBZszbOur5VEA=";
    berkeley-testfloat-3 =
      wrap "berkeley-testfloat-3" "e7af9751d9f9fd3b47911f51a5cfd08af256a9ab"
        "sha256-inQAeYlmuiRtZm37xK9ypBltCJ+ycyvIeIYZK8a+RYU=";
  };

  # Every patch in qemu/patches, in order.
  patchDir = ../qemu/patches;
  patches = map (name: patchDir + "/${name}") (
    builtins.sort builtins.lessThan (
      builtins.filter (lib.hasSuffix ".patch") (builtins.attrNames (builtins.readDir patchDir))
    )
  );

  python = pkgs.python3.withPackages (ps: [ ps.distlib ]);

  qemu = pkgs.stdenv.mkDerivation {
    pname = "rewind-qemu-system";
    version = "8.2.0-${builtins.substring 0 8 qemuWasmRev}";
    inherit src patches;

    nativeBuildInputs = with pkgs; [
      pkg-config
      meson
      ninja
      python
      perl
      bison
      flex
    ];
    buildInputs = with pkgs; [
      glib
      pixman
      zlib
      dtc
    ];

    postPatch = lib.concatStrings (
      lib.mapAttrsToList (name: tree: ''
        cp -r ${tree} subprojects/${name}
        chmod -R u+w subprojects/${name}
        if [ -d subprojects/packagefiles/${name} ]; then
          cp -r subprojects/packagefiles/${name}/. subprojects/${name}/
        fi
      '') subprojects
    );

    # QEMU 8.2 predates the const-correct string functions of the glibc
    # in this nixpkgs, which -Werror turns into errors.
    configurePhase = ''
      runHook preConfigure
      ./configure --prefix=$out --target-list=x86_64-softmmu \
        --without-default-features --enable-system --enable-tcg \
        --with-coroutine=ucontext --disable-werror \
        --extra-cflags=-Wno-error=discarded-qualifiers \
        --ninja=ninja --python=${python}/bin/python3 --disable-download --disable-docs
      runHook postConfigure
    '';

    buildPhase = ''
      runHook preBuild
      make -j$NIX_BUILD_CORES qemu-system-x86_64
      runHook postBuild
    '';

    installPhase = ''
      runHook preInstall
      install -Dm755 build/qemu-system-x86_64 $out/bin/qemu-system-x86_64
      install -Dm644 ${../qemu/LICENSE} $out/share/doc/rewind-qemu/LICENSE
      runHook postInstall
    '';

    meta = {
      description = "QEMU with Rewind VM's deterministic machine";
      license = lib.licenses.gpl2Plus;
      mainProgram = "qemu-system-x86_64";
      platforms = [ "x86_64-linux" ];
    };
  };

  # The replay command, an engine crate (MIT), pointed at that QEMU.
  replay = rust.craneLib.buildPackage (
    rust.engine
    // {
      pname = "rewind-qemu";
      cargoExtraArgs = "--locked -p rewind-qemu";
      cargoBuildExtraArgs = "-p rewind-qemu";
      doCheck = false;
      nativeBuildInputs = [ pkgs.makeWrapper ];
      postInstall = ''
        wrapProgram $out/bin/rewind-qemu --set-default REWIND_QEMU ${lib.getExe qemu}
      '';
      meta = {
        description = "Replay Rewind VM runs on QEMU's emulator";
        mainProgram = "rewind-qemu";
        license = lib.licenses.mit;
        platforms = [ "x86_64-linux" ];
      };
    }
  );
in
{
  inherit qemu replay;
}
