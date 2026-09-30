# The shell behind `nix develop`: the Rust toolchain from nixpkgs for
# iterating with `cargo build` and `cargo test`, the tools rewind calls to
# build input images, and the guest it boots, so `cargo run -- run ...`
# works from a checkout exactly as the packaged command does. The `app`
# shell below is the desktop app's.
{
  pkgs,
  kernel,
  guest,
}:
let
  app = import ./app.nix { inherit pkgs; };
in
{
  default = pkgs.mkShell {
    packages = [
      pkgs.cargo
      pkgs.rustc
      pkgs.rustfmt
      pkgs.clippy
      pkgs.pkg-config
      pkgs.erofs-utils
      pkgs.gnutar
      pkgs.cpio
      pkgs.python3
    ];

    REWIND_KERNEL = "${kernel}/bzImage";
    REWIND_INITRD = "${guest.initrd}/initrd";
    REWIND_SANDBOX_SHELL = "${guest.sandboxShell}";
  };

  # `nix develop .#app`: the desktop app's toolchain and libraries, for
  # `cargo run` and `cargo test` in crates/rewind-app. A cargo build is not
  # patched like the packaged one, so the libraries wgpu and Wayland open
  # at run time come from LD_LIBRARY_PATH, and the fonts from the same
  # fontconfig setup the package wraps the app with.
  app = pkgs.mkShell {
    packages = [
      pkgs.cargo
      pkgs.rustc
      pkgs.rustfmt
      pkgs.clippy
      pkgs.pkg-config
    ];
    buildInputs = app.buildInputs;

    LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath app.runtimeLibraries;
    FONTCONFIG_FILE = app.fontsConf;
  };
}
