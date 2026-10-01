# `nix build .#site`: the marketing site exactly as the pages workflow
# deploys it and `nix run .#serve` previews it, with the release tarball,
# the desktop app's tarball and the tutorials' example tarball under
# download/, which is where rewindvm.dev serves them from while the
# repository is private. The VM's kernel patch and config sit beside them:
# the tarball ships a GPL-2.0 kernel, so its source has to be available
# where the binary is.
#
# The site directory alone rather than a subpath of the flake source, so a
# change to the engine or the app rebuilds only the tarball.
{
  pkgs,
  release,
  example,
  appRelease,
}:
pkgs.runCommand "rewind-site" { } ''
  mkdir -p $out/download
  cp -r ${../site}/. $out/
  cp ${release}/*.tar.gz ${example}/*.tar.gz ${appRelease}/*.tar.gz $out/download/
  cp ${../guest/linux/rewind-guest.patch} $out/download/rewind-linux.patch
  cp ${../guest/linux/config} $out/download/rewind-linux.config
''
