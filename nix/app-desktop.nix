# The desktop app's desktop entry and icon, for the Nix package
# (nix/app.nix) and the release asset (nix/app-release.nix) alike: the
# entry under share/applications and the icon under share/icons/hicolor,
# as an SVG and as PNGs rendered from it for desktops that want bitmaps.
#
# Both files are named after the app id the window sets
# (crates/rewind-app/src/ui/mod.rs), which is how a Wayland compositor or
# an X11 window manager pairs the running window with its entry and icon.
{ pkgs }:
let
  appId = "surf.lunchtime.Rewind";
  assets = ../crates/rewind-app/assets;

  # The bitmap sizes desktops ask for most: launchers, docks and the
  # window switcher.
  pngSizes = [
    "48"
    "128"
    "256"
  ];
in
pkgs.runCommand "rewind-app-desktop" { nativeBuildInputs = [ pkgs.librsvg ]; } ''
  install -D -m 644 ${assets}/${appId}.desktop $out/share/applications/${appId}.desktop
  install -D -m 644 ${assets}/${appId}.svg $out/share/icons/hicolor/scalable/apps/${appId}.svg
  for size in ${pkgs.lib.concatStringsSep " " pngSizes}; do
    dir=$out/share/icons/hicolor/''${size}x''${size}/apps
    mkdir -p $dir
    rsvg-convert -w $size -h $size ${assets}/${appId}.svg -o $dir/${appId}.png
  done
''
