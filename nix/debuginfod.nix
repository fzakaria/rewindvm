# nixseparatedebuginfod2, the debuginfod server `rewind gdb` and `rewind
# where` start for each session to hand gdb DWARF and source files. Every
# session's server uses the same cache directory, and the patch lets them
# share it: without it, two sessions fetching the same sources at once
# leave a cache entry with files missing, which the server then serves 404
# for until it expires. The patch is upstream as
# https://github.com/symphorien/nixseparatedebuginfod2/pull/3; drop it once
# nixpkgs has a release with it.
{ pkgs }:
pkgs.nixseparatedebuginfod2.overrideAttrs (old: {
  patches = (old.patches or [ ]) ++ [ ./nixseparatedebuginfod2-shared-cache.patch ];
})
