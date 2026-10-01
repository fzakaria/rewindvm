# The desktop app tarball's own flake, shipped as its flake.nix, so the
# tarball's URL is a flake reference:
#
#   nix run https://rewindvm.dev/download/rewind-app-0.1.0-x86_64-linux.tar.gz
#
# Nothing here is built from source. The package takes the tarball's
# prebuilt app, which is linked for other distributions, and points it at
# the pinned nixpkgs' loader, libraries and graphics drivers instead of the
# system's. nixpkgs is locked by the flake.lock shipped next to this file.
{
  description = "Rewind VM desktop app: scrub, compare and fork recorded runs";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      inherit (pkgs) lib;

      # The libraries the app links, and those wgpu and the Wayland client
      # open while it runs.
      libraries = [
        pkgs.libxcb
        pkgs.libxkbcommon
        pkgs.wayland
        pkgs.vulkan-loader
        pkgs.libGL
      ];

      app =
        pkgs.runCommand "rewind-app-0.1.0"
          {
            nativeBuildInputs = [
              pkgs.patchelf
              pkgs.makeBinaryWrapper
            ];
            meta = {
              description = "Scrub, compare and fork recorded runs of Rewind VM";
              mainProgram = "rewind-app";
              platforms = [ system ];
            };
          }
          ''
            mkdir -p $out/libexec $out/share/applications
            install -m 755 ${self}/libexec/rewind-app $out/libexec/rewind-app
            patchelf \
              --set-interpreter ${pkgs.stdenv.cc.bintools.dynamicLinker} \
              --set-rpath ${lib.makeLibraryPath libraries} \
              $out/libexec/rewind-app
            makeBinaryWrapper $out/libexec/rewind-app $out/bin/rewind-app \
              --set-default REWIND_APP_FONTS ${self}/share/rewind-app/fonts
            install -m 644 ${self}/share/applications/rewind-app.desktop $out/share/applications/
          '';
    in
    {
      packages.${system}.default = app;

      # `programs.rewind.app` with this tarball's app. Import the command's
      # module too for `programs.rewind` itself.
      nixosModules.default = import ./module.nix { inherit app; };
    };
}
