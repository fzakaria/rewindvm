# `nix build .#app`: the desktop app, the scrubber over recorded runs
# (crates/rewind-app).
#
# The app is a Cargo workspace of its own with its own Cargo.lock, so the
# source is only that crate, the rewind-trace crate it reads traces with,
# and the root Cargo.toml that rewind-trace inherits its version, edition
# and license from. A change to the engine does not rebuild the app.
#
# GPUI draws through wgpu, which opens Vulkan or OpenGL when the window
# opens, and reaches Wayland through a library it also opens at run time.
# None of those are linked, so they go on the binary's rpath after the
# build, as nixpkgs does for zed-editor. The wrapper adds the app's two
# fonts, IBM Plex Sans and JetBrains Mono, to the fonts fontconfig already
# finds; without them the app falls back to whatever sans-serif and
# monospace the system has. The desktop entry and icon come from
# nix/app-desktop.nix, so the app shows in launchers with its icon.
{ pkgs }:
let
  inherit (pkgs) lib;
  fs = lib.fileset;

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
in
pkgs.rustPlatform.buildRustPackage {
  pname = "rewind-app";
  # VERSION, the one place a release's version is written.
  version = lib.fileContents ../VERSION;

  src = fs.toSource {
    root = ../.;
    fileset = fs.difference (fs.unions [
      ../Cargo.toml
      ../crates/rewind-app
      ../crates/rewind-trace
    ]) (fs.maybeMissing ../crates/rewind-app/target);
  };

  cargoLock.lockFile = ../crates/rewind-app/Cargo.lock;
  cargoRoot = "crates/rewind-app";
  buildAndTestSubdir = "crates/rewind-app";

  nativeBuildInputs = [
    pkgs.pkg-config
    pkgs.makeBinaryWrapper
  ];

  # Linked: fontconfig and freetype for text, xkbcommon for the keyboard,
  # xcb for X11 windows.
  buildInputs = [
    pkgs.fontconfig
    pkgs.freetype
    pkgs.libxkbcommon
    pkgs.libxcb
    pkgs.wayland
  ];

  postInstall = ''
    cp -r ${desktop}/share $out/
  '';

  postFixup = ''
    patchelf $out/bin/rewind-app --add-rpath ${lib.makeLibraryPath runtimeLibraries}
    wrapProgram $out/bin/rewind-app --set-default FONTCONFIG_FILE ${fontsConf}
  '';

  # For the `app` dev shell (nix/dev-shells.nix), which runs `cargo run`
  # against the same libraries and fonts.
  passthru = {
    inherit runtimeLibraries fontsConf;
  };

  meta = {
    description = "Rewind VM desktop app: scrub, compare and fork recorded runs";
    # No license attribute: nixpkgs would refuse to build an unfree
    # package without allowUnfree, and this flake is where it is made.
    # crates/rewind-app/LICENSE is the license.
    mainProgram = "rewind-app";
    platforms = lib.platforms.linux;
  };
}
