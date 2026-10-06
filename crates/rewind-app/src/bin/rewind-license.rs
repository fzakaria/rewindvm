//! `rewind-license`: makes the signing key and issues licenses.
//!
//!     rewind-license keygen [--out <dir>]
//!     rewind-license issue --key <signing.key | -> --name <name> --email <email>
//!         --edition personal|commercial [--seats N] [--issued YYYY-MM-DD]
//!
//! The signing key is the 32 byte Ed25519 seed in hex. `keygen` prints it
//! once on standard error, for a password manager, or with `--out` writes
//! it to `signing.key` readable by its owner only; either way it prints
//! the public key as the Rust array to paste into src/license.rs. `issue`
//! reads the key from a file, or with `--key -` from standard input, at a
//! hidden prompt when that is a terminal, and prints a license block.
//! Built only with `--features issuer`, so it never ships with the app.
//! See LICENSING.md.

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::ExitCode;

use ed25519_dalek::SigningKey;
use rewind_app::license::{self, Date, Edition, License, LicenseError};

const USAGE: &str = "usage:
  rewind-license keygen [--out <dir>]
  rewind-license issue --key <signing.key | -> --name <name> --email <email> --edition personal|commercial [--seats N] [--issued YYYY-MM-DD]";

/// The signing key's file name and permissions: owner read and write only.
const KEY_FILE: &str = "signing.key";
const KEY_MODE: u32 = 0o600;
const SEED_LEN: usize = 32;
const HEX_RADIX: u32 = 16;

/// Public key bytes per line of the printed Rust array.
const BYTES_PER_LINE: usize = 16;

/// The `--key` value that means standard input.
const STDIN: &str = "-";

/// The flags each command takes, each with a value.
const KEYGEN_FLAGS: &[&str] = &["out"];
const ISSUE_FLAGS: &[&str] = &["key", "name", "email", "edition", "seats", "issued"];

/// The prefix a flag starts with.
const FLAG_PREFIX: &str = "--";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("keygen") => options(&args[1..], KEYGEN_FLAGS).and_then(|o| keygen(&o)),
        Some("issue") => options(&args[1..], ISSUE_FLAGS).and_then(|o| issue(&o)),
        _ => Err(USAGE.to_string()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rewind-license: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `--flag value` pairs, each flag one of `allowed` and given once.
fn options(args: &[String], allowed: &[&str]) -> Result<HashMap<String, String>, String> {
    let mut opts = HashMap::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let Some(name) = arg.strip_prefix(FLAG_PREFIX) else {
            return Err(format!("not a flag: {arg}\n{USAGE}"));
        };
        if !allowed.contains(&name) {
            return Err(format!("unknown flag --{name}\n{USAGE}"));
        }
        let Some(value) = args.next() else {
            return Err(format!("--{name} needs a value"));
        };
        if opts.insert(name.to_string(), value.clone()).is_some() {
            return Err(format!("--{name} is given twice"));
        }
    }
    Ok(opts)
}

fn required<'a>(opts: &'a HashMap<String, String>, name: &str) -> Result<&'a str, String> {
    opts.get(name)
        .map(String::as_str)
        .ok_or_else(|| format!("--{name} is required\n{USAGE}"))
}

/// Makes a new key pair: the signing seed to standard error or to a file
/// only its owner can read, the public key to standard output.
fn keygen(opts: &HashMap<String, String>) -> Result<(), String> {
    let seed = license::random_bytes::<SEED_LEN>().map_err(|e| e.to_string())?;
    let key = SigningKey::from_bytes(&seed);
    let hex: String = seed.iter().map(|b| format!("{b:02x}")).collect();

    match opts.get("out") {
        // Shown once, for a password manager; nothing touches the disk.
        None => {
            eprintln!("signing key, shown once; store it in your password manager:");
            eprintln!();
            eprintln!("    {hex}");
            eprintln!();
        }
        Some(dir) => {
            let dir = PathBuf::from(dir);
            std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            let path = dir.join(KEY_FILE);

            // create_new refuses to overwrite a key that already signed
            // licenses.
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(KEY_MODE)
                .open(&path)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            writeln!(file, "{hex}").map_err(|e| e.to_string())?;
            eprintln!("wrote {}; keep it offline", path.display());
        }
    }

    eprintln!("public key, for PUBLIC_KEY in crates/rewind-app/src/license.rs:");
    println!("pub const PUBLIC_KEY: [u8; 32] = [");
    for chunk in key.verifying_key().to_bytes().chunks(BYTES_PER_LINE) {
        let line: Vec<String> = chunk.iter().map(|b| format!("0x{b:02x}")).collect();
        println!("    {},", line.join(", "));
    }
    println!("];");
    Ok(())
}

/// Signs a license and prints the block. What it is for is checked
/// before the signing key is asked for.
fn issue(opts: &HashMap<String, String>) -> Result<(), String> {
    let license = licensed(opts)?;
    let key_path = required(opts, "key")?;
    let text = if key_path == STDIN {
        read_secret_from_stdin()?
    } else {
        std::fs::read_to_string(key_path).map_err(|e| format!("{key_path}: {e}"))?
    };
    let key = read_key(text.trim())
        .ok_or_else(|| format!("{key_path}: not a signing key, 64 hex digits"))?;
    print!("{}", signed_for(&license, &key, &license::PUBLIC_KEY)?);
    Ok(())
}

/// The license `opts` ask for, with a new id.
fn licensed(opts: &HashMap<String, String>) -> Result<License, String> {
    let edition_text = required(opts, "edition")?;
    let edition = Edition::parse(edition_text)
        .ok_or_else(|| format!("--edition must be personal or commercial, not {edition_text}"))?;
    let seats = match opts.get("seats") {
        Some(s) => s
            .parse()
            .map_err(|_| format!("--seats: not a number: {s}"))?,
        None => 1,
    };
    if seats == 0 {
        return Err("--seats must be at least 1".to_string());
    }
    let issued = match opts.get("issued") {
        Some(s) => Date::parse(s).ok_or_else(|| format!("--issued: not a YYYY-MM-DD date: {s}"))?,
        None => Date::today(),
    };
    Ok(License {
        name: trimmed(opts, "name")?,
        email: trimmed(opts, "email")?,
        edition,
        seats,
        id: license::new_id().map_err(|e| e.to_string())?,
        issued,
        updates_until: issued.add_years(license::UPDATE_YEARS),
    })
}

/// A required flag's value without spaces around it, refused when that
/// leaves nothing.
fn trimmed(opts: &HashMap<String, String>, name: &str) -> Result<String, String> {
    let value = required(opts, name)?.trim();
    if value.is_empty() {
        return Err(format!("--{name} is blank"));
    }
    Ok(value.to_string())
}

/// The block for `license` signed with `key`, read back the way the app
/// reads it, with `public` the key the app checks against. A block that
/// does not read back as the license, such as one signed with another key
/// or with a line break in a value, is refused rather than printed.
fn signed_for(license: &License, key: &SigningKey, public: &[u8; 32]) -> Result<String, String> {
    let block = license.sign(key);
    let read = license::verify_with(&block, public, &[]).map_err(|e| match e {
        LicenseError::BadSignature => {
            "the signing key is not the one the app checks against, PUBLIC_KEY in src/license.rs"
                .to_string()
        }
        e => format!("the app would refuse this license: {e}"),
    })?;
    if read != *license {
        return Err("the license reads back differently from what was asked for".to_string());
    }
    Ok(block)
}

/// The signing key from standard input. At a terminal, asks for it without
/// echoing it; from a pipe, such as `op read`, takes the first line.
fn read_secret_from_stdin() -> Result<String, String> {
    use std::io::IsTerminal;

    let stdin = std::io::stdin();
    let terminal = stdin.is_terminal();
    let echo = |on: bool| {
        // stty acts on its standard input, which is the terminal here.
        let _ = std::process::Command::new("stty")
            .arg(if on { "echo" } else { "-echo" })
            .status();
    };
    if terminal {
        eprint!("signing key: ");
        echo(false);
    }
    let mut line = String::new();
    let read = stdin.read_line(&mut line);
    if terminal {
        echo(true);
        eprintln!();
    }
    read.map_err(|e| format!("reading the signing key: {e}"))?;
    Ok(line)
}

/// A signing key from its hex seed.
fn read_key(hex: &str) -> Option<SigningKey> {
    if hex.len() != SEED_LEN * 2 {
        return None;
    }
    let mut seed = [0u8; SEED_LEN];
    for (i, byte) in seed.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, HEX_RADIX).ok()?;
    }
    Some(SigningKey::from_bytes(&seed))
}

#[cfg(test)]
mod tests {
    // The issuer's checks on what it is asked for and what it prints:
    // flags are parsed as `issue` gets them from the command line, and
    // licenses are signed with a key from a fixed seed.
    use super::*;

    const TEST_SEED: [u8; 32] = [7; 32];

    fn args(line: &str) -> Vec<String> {
        line.split(' ').map(str::to_string).collect()
    }

    #[test]
    fn a_flag_issue_does_not_take_is_refused() {
        // A misspelt --seats would otherwise issue one seat silently, and
        // a flag with no value would be dropped.
        let opts = options(&args("--name Ada --seat 5"), ISSUE_FLAGS);
        assert_eq!(opts.unwrap_err(), format!("unknown flag --seat\n{USAGE}"));
        let opts = options(&args("--name Ada --seats"), ISSUE_FLAGS);
        assert_eq!(opts.unwrap_err(), "--seats needs a value");
        let opts = options(&args("--name Ada --name Bob"), ISSUE_FLAGS);
        assert_eq!(opts.unwrap_err(), "--name is given twice");
    }

    #[test]
    fn zero_seats_and_blank_names_are_refused_and_names_are_trimmed() {
        // What the buyer typed into checkout can carry spaces; a license
        // for nobody or for no seats is a mistake.
        let opts = |line: &str| options(&args(line), ISSUE_FLAGS).unwrap();
        let zero = licensed(&opts(
            "--edition personal --name Ada --email a@b.c --seats 0",
        ));
        assert_eq!(zero.unwrap_err(), "--seats must be at least 1");

        let mut padded = opts("--edition personal --seats 2");
        padded.insert("name".into(), "  Ada Lovelace ".into());
        padded.insert("email".into(), " ada@example.com\t".into());
        let license = licensed(&padded).unwrap();
        assert_eq!(license.name, "Ada Lovelace");
        assert_eq!(license.email, "ada@example.com");
        assert_eq!(license.seats, 2);

        padded.insert("name".into(), "   ".into());
        assert_eq!(licensed(&padded).unwrap_err(), "--name is blank");
    }

    #[test]
    fn a_block_the_app_would_refuse_is_not_printed() {
        // Signed with the key the app checks against, the block reads back
        // as the license; signed with any other key, issuing fails rather
        // than print a block every copy of the app refuses.
        let key = SigningKey::from_bytes(&TEST_SEED);
        let opts = options(
            &args("--edition commercial --name Ada --email a@b.c --seats 3"),
            ISSUE_FLAGS,
        )
        .unwrap();
        let license = licensed(&opts).unwrap();
        let ours = key.verifying_key().to_bytes();
        let block = signed_for(&license, &key, &ours).unwrap();
        assert_eq!(license::verify_with(&block, &ours, &[]).unwrap(), license);

        let other = SigningKey::from_bytes(&[8; 32]).verifying_key().to_bytes();
        assert!(signed_for(&license, &key, &other).is_err());
    }
}
