# `nix build .#release-debug`: the guest kernel's debug symbols for
# `rewind gdb`, a release asset next to the command's for people without
# Nix. On a host with Nix, `rewind gdb` fetches the kernel's `debug` output
# from the binary cache and its source files through nixseparatedebuginfod2;
# this tarball holds both instead. It unpacks to rewind-debug-x86_64-linux/
# next to rewind-x86_64-linux/, whose launcher then names it, with no
# version in the name so the asset has the same URL under releases/latest.
#
# The layout is the `debug` output's, so `rewind gdb` reads either the same
# way: vmlinux's DWARF under lib/debug, and under src/overlay/<source root>
# the source files that DWARF names, with the Rewind patch applied.
{
  pkgs,
  kernel,
}:
pkgs.runCommand "rewind-debug-release"
  {
    nativeBuildInputs = [ pkgs.llvm ];
  }
  ''
    dir=rewind-debug-x86_64-linux
    mkdir -p $dir/lib $dir/src/overlay
    cp -r ${kernel.debug}/lib/debug $dir/lib/

    # The one directory in the debug output's overlay is named like the
    # kernel's source tree, which the build compiled in /build.
    root=$(ls ${kernel.debug}/src/overlay)
    sources=$dir/src/overlay/$root

    # Every source file the DWARF names, from the upstream tarball. The
    # files the build generated are not in it, and are left out.
    tar -xf ${kernel.src}
    llvm-dwarfdump --show-sources $dir/lib/debug/vmlinux |
      sed -n "s|^/build/$root/\(\./\)\{0,1\}||p" |
      while read -r file; do
        if [ -f "$root/$file" ]; then
          install -D -m 644 "$root/$file" "$sources/$file"
        fi
      done

    # The files the Rewind patch adds or changes, over the upstream ones.
    cp -r --no-preserve=mode ${kernel.debug}/src/overlay/$root/. $sources/
    chmod -R a-w,u+w $dir

    cp ${../LICENSE} $dir/LICENSE
    mkdir -p $out
    tar --sort=name --owner=0 --group=0 --numeric-owner --mtime=@1 \
      -czf $out/$dir.tar.gz $dir
  ''
