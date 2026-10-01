# `nix build .#app-release`: the desktop app's tarball, which rewindvm.dev
# serves next to the command's. It unpacks to rewind-app-0.1.0-x86_64-linux/:
#
#   bin/rewind-app            the launcher
#   libexec/rewind-app        the app, built for glibc 2.31 (nix/app-portable.nix)
#   share/rewind-app/fonts/   IBM Plex Sans and JetBrains Mono, with their licenses
#   share/applications/       a desktop entry to copy into ~/.local/share
#   flake.nix, flake.lock     the tarball as a flake (nix/app-release-flake.nix)
#   module.nix                the NixOS module (nix/module.nix)
#   LICENSE                   the app's license
#
# On other distributions the launcher runs the app against the system's
# libraries and graphics drivers. On Nix the tarball's flake runs the same
# binary against the pinned nixpkgs instead.
{ pkgs }:
let
  inherit (pkgs) lib;
  version = "0.1.0";
  dir = "rewind-app-${version}-x86_64-linux";

  portable = import ./app-portable.nix { inherit pkgs; };

  # The weights the app draws with (src/theme.rs).
  plex = "${pkgs.ibm-plex}/share/fonts/opentype";
  jetbrains = "${pkgs.jetbrains-mono}/share/fonts/truetype";
  fonts = [
    "${plex}/IBMPlexSans-Regular.otf"
    "${plex}/IBMPlexSans-SemiBold.otf"
    "${plex}/IBMPlexSans-Bold.otf"
    "${jetbrains}/JetBrainsMono-Regular.ttf"
    "${jetbrains}/JetBrainsMono-SemiBold.ttf"
  ];

  # Both families are under the SIL Open Font License 1.1, which has to
  # travel with them. JetBrains Mono's source carries the license text;
  # IBM Plex's notice is the same text under IBM's copyright line.
  ofl = "${pkgs.jetbrains-mono.src}/OFL.txt";
  plexCopyright = ''Copyright © 2017 IBM Corp. with Reserved Font Name "Plex"'';

  launcher = ./app-launcher.sh;

  desktopEntry = pkgs.writeText "rewind-app.desktop" ''
    [Desktop Entry]
    Type=Application
    Name=Rewind
    Comment=Scrub, compare and fork recorded runs of Rewind VM
    Exec=rewind-app %f
    Terminal=false
    Categories=Development;Debugger;
  '';
in
pkgs.runCommand "rewind-app-release" { } ''
  mkdir -p ${dir}/bin ${dir}/libexec ${dir}/share/rewind-app/fonts ${dir}/share/applications
  install -m 755 ${launcher} ${dir}/bin/rewind-app
  install -m 755 ${portable}/libexec/rewind-app ${dir}/libexec/rewind-app

  # The fonts and their licenses.
  for font in ${lib.concatStringsSep " " fonts}; do
    install -m 644 "$font" ${dir}/share/rewind-app/fonts/
  done
  install -m 644 ${ofl} ${dir}/share/rewind-app/fonts/OFL-JetBrainsMono.txt
  { echo '${plexCopyright}'; tail -n +2 ${ofl}; } > ${dir}/share/rewind-app/fonts/OFL-IBMPlex.txt

  install -m 644 ${desktopEntry} ${dir}/share/applications/rewind-app.desktop
  install -m 644 ${../crates/rewind-app/LICENSE} ${dir}/LICENSE
  install -m 644 ${./app-release-flake.nix} ${dir}/flake.nix
  install -m 644 ${../flake.lock} ${dir}/flake.lock
  install -m 644 ${./module.nix} ${dir}/module.nix

  mkdir -p $out
  tar --sort=name --owner=0 --group=0 --numeric-owner --mtime=@1 \
    -czf $out/${dir}.tar.gz ${dir}
''
