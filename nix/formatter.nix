# `nix fmt`. The tree wrapper, matching tidemark and nixpkgs-multiverse: one
# formatting step covers every language in the tree. rustfmt for the crates,
# clang-format for the guest kernel code, black for the tools, prettier for
# the site and for markdown.
{ pkgs }:
pkgs.nixfmt-tree.override {
  runtimeInputs = [
    pkgs.rustfmt
    pkgs.black
    pkgs.prettier
  ];
  settings = {
    # Kernel patches are diffs against upstream Linux, and upstream's style is
    # checkpatch's, not ours.
    global.excludes = [
      "guest/linux/*.patch"
      "*.png"
    ];
    formatter.rustfmt = {
      command = "rustfmt";
      options = [
        "--edition"
        "2024"
      ];
      includes = [ "*.rs" ];
    };
    formatter.black = {
      command = "black";
      options = [ "--quiet" ];
      includes = [ "*.py" ];
    };
    formatter.prettier = {
      command = "prettier";
      options = [ "--write" ];
      includes = [
        "*.css"
        "*.js"
      ];
    };
    # proseWrap stays at its default of preserving the author's line breaks:
    # these files are hand-wrapped prose, and reflowing would make every
    # future diff a whole-file diff.
    formatter.prettier-markdown = {
      command = "prettier";
      options = [
        "--write"
        "--print-width"
        "80"
      ];
      includes = [ "*.md" ];
    };
    # HTML in prettier's default whitespace-sensitive mode, as tidemark
    # found: the insensitive mode splits "</a>." and renders a stray space.
    formatter.prettier-html = {
      command = "prettier";
      options = [
        "--write"
        "--print-width"
        "100"
      ];
      includes = [ "*.html" ];
    };
  };
}
