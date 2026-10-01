# `nix run .#license -- keygen` and `nix run .#license -- issue ...`: the
# license issuer, rewind-license, built with the app's `issuer` feature.
# It is a package of its own so the issuer never ships in the app's
# package. See crates/rewind-app/LICENSING.md.
{ pkgs, app }:
pkgs.rustPlatform.buildRustPackage {
  pname = "rewind-license";
  inherit (app)
    version
    src
    cargoRoot
    buildAndTestSubdir
    nativeBuildInputs
    buildInputs
    ;
  cargoLock.lockFile = ../crates/rewind-app/Cargo.lock;

  buildFeatures = [ "issuer" ];
  cargoBuildFlags = [
    "--bin"
    "rewind-license"
  ];
  # The app's package runs the tests, the issuer's among them.
  doCheck = false;

  meta = {
    description = "Makes the Rewind VM license signing key and issues licenses";
    mainProgram = "rewind-license";
    platforms = pkgs.lib.platforms.linux;
  };
}
