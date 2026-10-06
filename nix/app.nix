# `nix build .#app`: the desktop app, the scrubber over recorded runs
# (crates/rewind-app).
#
# The app is a Cargo workspace of its own with its own Cargo.lock; its
# source, and why it is only that, is in nix/crane.nix. A change to the
# engine does not rebuild the app: the engine comes in through the
# wrapper, a small derivation around the compiled app.
#
# GPUI draws through wgpu, which opens Vulkan or OpenGL when the window
# opens, and reaches Wayland through a library it also opens at run time.
# None of those are linked, so they go on the binary's rpath after the
# build, as nixpkgs does for zed-editor. The wrapper adds the app's two
# fonts, IBM Plex Sans and JetBrains Mono, to the fonts fontconfig already
# finds; without them the app falls back to whatever sans-serif and
# monospace the system has. It also points the app at the engine of the
# same commit, so the app never runs an older or newer `rewind` from
# PATH, which can boot a run differently from the build that recorded
# it; REWIND_BIN set by hand still wins. The desktop entry and icon come from
# nix/app-desktop.nix, so the app shows in launchers with its icon.
{
  pkgs,
  rust,
  rewind,
  releaseDate,
}:
let
  inherit (pkgs) lib;

  # A fontconfig setup that keeps the system's fonts and config and adds
  # the two the design is set in.
  fontsConf = pkgs.makeFontsConf {
    fontDirectories = [
      pkgs.ibm-plex
      pkgs.jetbrains-mono
    ];
  };

  desktop = import ./app-desktop.nix { inherit pkgs; };

  # Libraries wgpu and the Wayland client open at run time.
  runtimeLibraries = [
    pkgs.libGL
    pkgs.vulkan-loader
    pkgs.wayland
  ];

  # The app's dependencies, compiled once per Cargo.lock and set of
  # manifests. nix/license.nix builds the issuer on them too.
  cargoArtifacts = rust.craneLib.buildDepsOnly (rust.app // { pname = "rewind-app"; });

  # The compiled app, which finds the engine on PATH.
  unwrapped = rust.craneLib.buildPackage (
    rust.app
    // {
      pname = "rewind-app";
      inherit cargoArtifacts;

      # The day the commit was made, which licenses are held against
      # (build.rs); given to this build alone, so the dependencies' build
      # does not change with every commit.
      REWIND_RELEASE_DATE = releaseDate;

      # Writable copies: crane's install hooks rewrite toolchain paths in
      # every file of the output, the desktop entry and icon among them.
      postInstall = ''
        cp -r --no-preserve=mode ${desktop}/share $out/
      '';

      postFixup = ''
        patchelf $out/bin/rewind-app --add-rpath ${lib.makeLibraryPath runtimeLibraries}
      '';

      meta = {
        description = "Rewind VM desktop app: scrub, compare and fork recorded runs";
        # No license attribute: nixpkgs would refuse to build an unfree
        # package without allowUnfree, and this flake is where it is made.
        # crates/rewind-app/LICENSE is the license.
        mainProgram = "rewind-app";
        platforms = lib.platforms.linux;
      };
    }
  );
in
pkgs.runCommand "rewind-app-${unwrapped.version}"
  {
    nativeBuildInputs = [ pkgs.makeBinaryWrapper ];

    # For the `app` dev shell (nix/dev-shells.nix) and the license issuer
    # (nix/license.nix), which build on the same libraries, fonts and
    # compiled dependencies.
    passthru = {
      inherit
        unwrapped
        runtimeLibraries
        fontsConf
        cargoArtifacts
        ;
      inherit (unwrapped) buildInputs;
    };

    inherit (unwrapped) meta;
  }
  ''
    mkdir -p $out/bin
    ln -s ${unwrapped}/share $out/share
    makeWrapper ${unwrapped}/bin/rewind-app $out/bin/rewind-app \
      --set-default FONTCONFIG_FILE ${fontsConf} \
      --set-default REWIND_BIN ${lib.getExe rewind}
  ''
