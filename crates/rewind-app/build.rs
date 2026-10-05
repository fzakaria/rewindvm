//! Compiles the grammars in vendor/syntaxes into one syntax set and dumps
//! it where src/syntax.rs includes it. Parsing the grammars' YAML takes
//! longer than loading the dump, and the dump holds only the languages the
//! app highlights, where syntect's own default set holds every language
//! Sublime Text shipped.

use std::path::Path;

use syntect::parsing::SyntaxSetBuilder;

/// Where the grammars are, by the crate's directory, and the dump's name
/// in the build's output directory.
const GRAMMARS: &str = "vendor/syntaxes";
const DUMP: &str = "syntaxes.packdump";

/// The grammars match lines with their newline, as Sublime Text's do.
const LINES_INCLUDE_NEWLINE: bool = true;

fn main() {
    println!("cargo::rerun-if-changed={GRAMMARS}");

    // Plain text for files of no known language, then every grammar.
    let mut builder = SyntaxSetBuilder::new();
    builder.add_plain_text_syntax();
    builder
        .add_from_folder(GRAMMARS, LINES_INCLUDE_NEWLINE)
        .expect("the vendored grammars load");
    let set = builder.build();

    let out = std::env::var("OUT_DIR").expect("cargo sets OUT_DIR");
    std::fs::write(
        Path::new(&out).join(DUMP),
        syntect::dumps::dump_binary(&set),
    )
    .expect("the syntax dump is written");
}
