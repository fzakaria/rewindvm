# `nix run .#license -- keygen` and `nix run .#license -- issue ...`: the
# license issuer, rewind-license, built with the app's `issuer` feature.
# It is a package of its own so the issuer never ships in the app's
# package. See crates/rewind-app/LICENSING.md.
#
# The feature adds no dependencies, so the issuer builds on the app's
# compiled dependencies and compiles only the app's own crate again.
{
  pkgs,
  rust,
  app,
  releaseDate,
}:
rust.craneLib.buildPackage (
  rust.app
  // {
    pname = "rewind-license";
    inherit (app) cargoArtifacts;
    REWIND_RELEASE_DATE = releaseDate;

    cargoExtraArgs = "--locked --features issuer";
    cargoBuildExtraArgs = "--bin rewind-license";
    # The app's package runs the tests, the issuer's among them.
    doCheck = false;

    meta = {
      description = "Makes the Rewind VM license signing key and issues licenses";
      mainProgram = "rewind-license";
      platforms = pkgs.lib.platforms.linux;
    };
  }
)
