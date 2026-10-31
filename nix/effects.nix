# Pushes to rewindvm.cachix.org and pins main's builds after nixbot builds main.
# `effects` is nixbot's effects library (nixbot.lib.effects): mkEffect
# gives the script CA certificates, and readSecretString reads the token
# from the secrets nixbot holds for this repository.
{
  pkgs,
  effects,
  packages,
}:
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
      effects.mkEffect {
        name = "cachix-effect";
        inputs = [ pkgs.cachix ];
        secretsMap.cachix = "cachix";
        effectScript = ''
          CACHIX_AUTH_TOKEN=$(readSecretString cachix .token)
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
