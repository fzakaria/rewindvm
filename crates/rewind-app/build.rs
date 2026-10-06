//! Compiles the grammars in vendor/syntaxes into one syntax set and dumps
//! it where src/syntax.rs includes it. Parsing the grammars' YAML takes
//! longer than loading the dump, and the dump holds only the languages the
//! app highlights, where syntect's own default set holds every language
//! Sublime Text shipped.
//!
//! Also writes the app's release date, which src/license.rs includes: the
//! day the commit it is built from was made. That is REWIND_RELEASE_DATE
//! when the build sets it, as the Nix packages do from the flake, else the
//! date of the commit git has checked out, in UTC.

use std::path::Path;
use std::process::Command;

use syntect::parsing::SyntaxSetBuilder;

/// Where the grammars are, by the crate's directory, and the dump's name
/// in the build's output directory.
const GRAMMARS: &str = "vendor/syntaxes";
const DUMP: &str = "syntaxes.packdump";

/// The grammars match lines with their newline, as Sublime Text's do.
const LINES_INCLUDE_NEWLINE: bool = true;

/// The variable the Nix packages set the release date in, YYYY-MM-DD, and
/// the file in the output directory the date is written to as a `Date`.
const RELEASE_DATE_ENV: &str = "REWIND_RELEASE_DATE";
const RELEASE_DATE_FILE: &str = "release_date.rs";

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

    let (year, month, day) = release_date();
    std::fs::write(
        Path::new(&out).join(RELEASE_DATE_FILE),
        format!("Date {{ year: {year}, month: {month}, day: {day} }}"),
    )
    .expect("the release date is written");
}

/// The day the app's commit was made, as year, month and day.
fn release_date() -> (i32, u8, u8) {
    println!("cargo::rerun-if-env-changed={RELEASE_DATE_ENV}");

    // A checkout's commit moves with HEAD, which jj moves on every commit
    // too, so the build script runs again when it does.
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .env("TZ", "UTC")
            .output()
            .ok()
            .filter(|o| o.status.success())?;
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    if let Some(dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        println!("cargo::rerun-if-changed={dir}/HEAD");
    }

    let given = std::env::var(RELEASE_DATE_ENV)
        .ok()
        .filter(|d| !d.is_empty());
    let date = given
        .or_else(|| git(&["log", "-1", "--format=%cd", "--date=format-local:%Y-%m-%d"]))
        .unwrap_or_else(|| {
            panic!("no git checkout to date the build by; set {RELEASE_DATE_ENV} to YYYY-MM-DD")
        });
    let parts: Vec<&str> = date.split('-').collect();
    let parsed = match parts[..] {
        [year, month, day] => year
            .parse()
            .ok()
            .zip(month.parse().ok())
            .zip(day.parse().ok())
            .map(|((y, m), d)| (y, m, d)),
        _ => None,
    };
    parsed.unwrap_or_else(|| panic!("{RELEASE_DATE_ENV} is not YYYY-MM-DD: {date}"))
}
