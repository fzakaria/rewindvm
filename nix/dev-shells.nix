# The shell behind `nix develop`: the Rust toolchain from nixpkgs for
# iterating with `cargo build` and `cargo test`, the tools rewind calls to
# build input images, and the guest it boots, so `cargo run -- run ...`
# works from a checkout exactly as the packaged command does. The `app`
# shell below is the desktop app's.
{
  pkgs,
  kernel,
  guest,
  nixseparatedebuginfod2,
  gdb,
  app,
}:
let
  inherit (pkgs) lib;
  isLinux = pkgs.stdenv.hostPlatform.isLinux;
  bootImage = kernel.bootImage or "vmlinux";
in
{
  default = pkgs.mkShell (
    {
      packages = [
        pkgs.cargo
        pkgs.rustc
        pkgs.rustfmt
        pkgs.clippy
        pkgs.pkg-config
        pkgs.gnutar
        pkgs.cpio
        pkgs.python3
      ]
      ++ lib.optionals isLinux [ pkgs.erofs-utils ];

      REWIND_KERNEL = "${kernel}/${bootImage}";
      REWIND_INITRD = "${guest.initrd}/initrd";
      REWIND_KERNEL_DEBUG = "${kernel.debug}";
    }
    // lib.optionalAttrs (nixseparatedebuginfod2 != null) {
      REWIND_DEBUGINFOD = lib.getExe nixseparatedebuginfod2;
    }
    // lib.optionalAttrs (gdb != null) {
      REWIND_GDB = "${gdb}/bin/gdb";
    }
  );

  # `nix develop .#web`: the toolchain for the desktop app built for the
  # browser, the site's viewer (nix/web.nix). nixpkgs' rustc carries the
  # wasm32-unknown-unknown standard library, and the wasm-bindgen command
  # must be the version crates/rewind-app pins.
  web = pkgs.mkShell {
    packages = [
      pkgs.cargo
      pkgs.rustc
      pkgs.rustfmt
      pkgs.clippy
      pkgs.lld
      pkgs.wasm-bindgen-cli_0_2_127
      pkgs.binaryen
      pkgs.python3
    ];
  };

  # `nix develop .#fuzz`: cargo-fuzz, for the fuzz targets in fuzz/ (see
  # fuzz/README.md). cargo-fuzz passes rustc unstable flags, which
  # nixpkgs' stable rustc takes only with RUSTC_BOOTSTRAP, so that is set
  # here and in no other shell.
  fuzz = pkgs.mkShell {
    packages = [
      pkgs.cargo
      pkgs.rustc
      pkgs.rustfmt
      pkgs.cargo-fuzz
    ];

    RUSTC_BOOTSTRAP = "1";
  };

}
// lib.optionalAttrs (app != null) {
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
