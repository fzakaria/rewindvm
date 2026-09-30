# The shell behind `nix develop`: the Rust toolchain from nixpkgs for
# iterating with `cargo build` and `cargo test`, the tools rewind calls to
# build input images, and the guest it boots, so `cargo run -- run ...`
# works from a checkout exactly as the packaged command does.
{
  pkgs,
  kernel,
  guest,
}:
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
}
