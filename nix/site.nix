# `nix build .#site`: the marketing site exactly as the pages workflow
# deploys it and `nix run .#serve` previews it, with the release tarball
# and the tutorials' example tarball under download/, which is where
# rewindvm.dev serves them from while the repository is private.
#
# The site directory alone rather than a subpath of the flake source, so a
# change to the engine or the app rebuilds only the tarball.
{
  pkgs,
  release,
  example,
}:
pkgs.runCommand "rewind-site" { } ''
  mkdir -p $out/download
  cp -r ${../site}/. $out/
  cp ${release}/*.tar.gz ${example}/*.tar.gz $out/download/
''
