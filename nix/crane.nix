# The Rust builds' shared parts, built with crane rather than nixpkgs'
# buildRustPackage. crane splits a build in two: buildDepsOnly compiles a
# workspace's dependencies against a copy of the source with every crate
# of ours replaced by an empty one, so that derivation changes only with
# Cargo.lock and the manifests, and the package's own build starts from
# its target directory. A change to our sources then compiles our crates
# and nothing else, and CI's cache serves the dependencies.
#
# Each workspace's sources, and the arguments its dependency build and its
# package build must agree on, are here, so the files that build the
# packages (nix/rewind.nix, nix/release.nix, nix/app.nix,
# nix/app-portable.nix and nix/license.nix) cannot drift apart. The
# guest's init stays on buildRustPackage; nix/guest.nix says why.
{ pkgs, crane }:
let
  inherit (pkgs) lib;
  fs = lib.fileset;

  # VERSION, the one place a release's version is written.
  version = lib.fileContents ../VERSION;

  # buildDepsOnly runs cargo check before cargo build, for crane's clippy
  # and check derivations, which nothing here uses. Skipping it saves a
  # pass over every dependency. Every workspace's arguments carry it.
  skipCheck = {
    cargoCheckCommand = "true";
  };
in
{
  # crane over this nixpkgs, and over its static musl package set for the
  # release binary.
  craneLib = crane.mkLib pkgs;
  craneLibStatic = crane.mkLib pkgs.pkgsStatic;

  # The engine's workspace without the desktop app, which is a workspace
  # of its own, and without build directories. Only the rewind command is
  # built; it depends on every crate in the workspace.
  engine = skipCheck // {
    inherit version;
    strictDeps = true;
    src = fs.toSource {
      root = ../.;
      fileset = fs.unions [
        ../Cargo.toml
        ../Cargo.lock
        (fs.difference ../crates (
          fs.unions [
            ../crates/rewind-app
            (fs.maybeMissing ../crates/rewind-init/target)
          ]
        ))
      ];
    };
    cargoBuildExtraArgs = "-p rewind";
  };

  # The app is a Cargo workspace of its own with its own Cargo.lock, so the
  # source is only that crate, the rewind-trace crate it reads traces with,
  # and the root Cargo.toml that rewind-trace inherits its version, edition
  # and license from. A change to the engine does not rebuild the app.
  app = skipCheck // {
    inherit version;
    strictDeps = true;
    src = fs.toSource {
      root = ../.;
      fileset = fs.difference (fs.unions [
        ../Cargo.toml
        ../crates/rewind-app
        ../crates/rewind-trace
      ]) (fs.maybeMissing ../crates/rewind-app/target);
    };
    cargoLock = ../crates/rewind-app/Cargo.lock;
    cargoToml = ../crates/rewind-app/Cargo.toml;

    # The workspace is crates/rewind-app, not the source's root. postUnpack
    # runs before the unpack phase enters the source, so it enters the
    # workspace itself and leaves sourceRoot pointing there.
    postUnpack = ''
      cd $sourceRoot/crates/rewind-app
      sourceRoot="."
    '';

    # crane's dependency source keeps only the Cargo.lock it is given, at
    # its root, and empties every crate it finds, the vendored ones under
    # crates/rewind-app/vendor among them. The lock goes where cargo looks
    # for it, and the vendored crates keep their real sources: they are
    # dependencies too, and an emptied gpui-pre-linux would rebuild it and
    # gpui-pre-platform with every change to the app.
    extraDummyScript = ''
      cp ${../crates/rewind-app/Cargo.lock} $out/crates/rewind-app/Cargo.lock
      rm -rf $out/crates/rewind-app/vendor
      cp -r ${
        fs.toSource {
          root = ../crates/rewind-app/vendor;
          fileset = ../crates/rewind-app/vendor;
        }
      } $out/crates/rewind-app/vendor
      chmod -R u+w $out/crates/rewind-app/vendor
    '';

    nativeBuildInputs = [ pkgs.pkg-config ];

    # Linked: fontconfig and freetype for text, xkbcommon for the keyboard,
    # xcb for X11 windows.
    buildInputs = [
      pkgs.fontconfig
      pkgs.freetype
      pkgs.libxkbcommon
      pkgs.libxcb
      pkgs.wayland
    ];
  };
}
