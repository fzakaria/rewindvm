# `nix build .#app-release`: the desktop app's release asset, next to the
# command's. It unpacks to rewind-app-x86_64-linux/, with no version in the
# name so the asset has the same URL under releases/latest:
#
#   bin/rewind-app            the launcher
#   libexec/rewind-app        the app, built for glibc 2.31 (nix/app-portable.nix)
#   share/rewind-app/fonts/   IBM Plex Sans and JetBrains Mono, with their licenses
#   share/applications/       the desktop entry and icon, which the install
#   share/icons/              script copies into ~/.local/share (nix/app-desktop.nix)
#   LICENSE                   the app's license
#
# The launcher runs the app against the system's libraries and graphics
# drivers. Nix users run the flake's `app` package instead.
{ pkgs, rust }:
let
  inherit (pkgs) lib;
  dir = "rewind-app-x86_64-linux";

  portable = import ./app-portable.nix { inherit pkgs rust; };

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

  desktop = import ./app-desktop.nix { inherit pkgs; };
in
pkgs.runCommand "rewind-app-release" { } ''
  mkdir -p ${dir}/bin ${dir}/libexec ${dir}/share/rewind-app/fonts
  install -m 755 ${launcher} ${dir}/bin/rewind-app
  install -m 755 ${portable}/libexec/rewind-app ${dir}/libexec/rewind-app

  # The fonts and their licenses.
  for font in ${lib.concatStringsSep " " fonts}; do
    install -m 644 "$font" ${dir}/share/rewind-app/fonts/
  done
  install -m 644 ${ofl} ${dir}/share/rewind-app/fonts/OFL-JetBrainsMono.txt
  { echo '${plexCopyright}'; tail -n +2 ${ofl}; } > ${dir}/share/rewind-app/fonts/OFL-IBMPlex.txt

  cp -r ${desktop}/share/applications ${desktop}/share/icons ${dir}/share/
  install -m 644 ${../crates/rewind-app/LICENSE} ${dir}/LICENSE

  mkdir -p $out
  tar --sort=name --owner=0 --group=0 --numeric-owner --mtime=@1 \
    -czf $out/${dir}.tar.gz ${dir}
''
