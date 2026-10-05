# Grammars for syntax highlighting

Sublime Text syntax definitions the app highlights source with, in the
source panel and the file viewer. `build.rs` compiles every
`.sublime-syntax` file under this directory into one syntax set and dumps it
for `src/syntax.rs` to load. Matching runs on fancy-regex; every pattern in
these files compiles with it.

The files are as their sources have them, unchanged.

## sublimehq-packages

C, C++, Rust, Go, Python with its regular expressions, Bash with the two
grammars it includes, Makefile and Markdown, from
https://github.com/sublimehq/Packages at commit
`fa6b8629c95041bf262d4c1dab95c456a0530122`, the commit syntect 5.3.0 builds
its own default syntaxes from, so they parse as syntect expects.

The repository's license, in `LICENSE`, lets anyone copy, use, modify, sell
and distribute the files. Its exception covers files with a license of their
own: the Rust grammar's directory carries the MIT license, copied here as
`Rust.sublime-syntax-license.txt` after the repository's own convention for
naming a file's license.

Python's embedded SQL and Markdown's embedded HTML have no grammar here and
show as plain text.

## docker-tmbundle

`Dockerfile-bash.sublime-syntax`, Dockerfiles with their RUN, CMD and
ENTRYPOINT lines highlighted as Bash, from
https://github.com/asbjornenge/Docker.tmbundle at commit
`c001fb280561d7c16f0f2837d76af493cf6c3bf8`, file
`Syntaxes/Dockerfile-bash.sublime-syntax`. MIT, Copyright 2014 Asbjorn Enge,
in `LICENSE`. Its JSON-array form of CMD refers to a JSON grammar that is not
here, and shows as plain text.

## sublime-asm

`asm.sublime-syntax`, assembly in Intel, AT&T (what GCC emits), ARM and Go
syntax, from https://github.com/mitranim/sublime-asm at commit
`fe04f51f2587e6ec83880d9bddad8de6dcab5525`. Released into the public domain
under the Unlicense, in `UNLICENSE`.

## sublime-nix

`Nix.sublime-syntax`, the Nix language, from https://github.com/sharkdp/bat
at commit `4608fc959aa8abf80d32198836511a570b7ae9ea`, file
`assets/syntaxes/02_Extra/Nix.sublime-syntax`: bat's conversion to
`.sublime-syntax` of the TextMate grammar in
https://github.com/wmertens/sublime-nix. The grammar is MIT, Copyright (c)
2016 Wout Mertens, in `LICENSE`; bat's conversion is MIT, Copyright (c)
2018-2023 bat-developers, in `LICENSE-bat-MIT`.
