# Pushes to rewindvm.cachix.org and pins main's builds after nixbot builds main.
{ pkgs, packages }:
args:
let
  pushed = with packages; [
    rewind
    app
    release
    release-debug
    app-release
    site
    debuginfod
    kernel
    kernel-debug
  ];
  pinned = [
    "rewind"
    "app"
    "kernel"
    "kernel-debug"
  ];
in
{
  onPush.default.outputs.effects.cachix =
    if args.branch == "main" then
      pkgs.stdenvNoCC.mkDerivation {
        name = "cachix-effect";
        phases = [ "buildPhase" ];
        nativeBuildInputs = [
          pkgs.cachix
          pkgs.jq
        ];
        buildPhase = ''
          export HOME=/tmp
          CACHIX_AUTH_TOKEN=$(jq -er .cachix.data.token "$HERCULES_CI_SECRETS_JSON")
          export CACHIX_AUTH_TOKEN
          cachix push rewindvm ${toString pushed}
          ${pkgs.lib.concatMapStrings (attr: ''
            cachix pin rewindvm main-${attr} ${packages.${attr}} --keep-revisions 5
          '') pinned}
        '';
      }
    else
      null;
}
