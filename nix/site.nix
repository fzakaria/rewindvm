# `nix build .#site`: the marketing site exactly as the pages workflow
# deploys it and `nix run .#serve` previews it.
#
# The site directory alone rather than a subpath of the flake source, so a
# change to the engine or the app does not rebuild the site.
{ pkgs }:
pkgs.runCommand "rewind-site" { } ''
  mkdir -p $out
  cp -r ${../site}/. $out/
''
