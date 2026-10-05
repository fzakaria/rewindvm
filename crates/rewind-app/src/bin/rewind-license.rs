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
use rewind_app::license::{self, Date, Edition, License};

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

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("keygen") => keygen(&options(&args[1..])),
        Some("issue") => issue(&options(&args[1..])),
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

/// `--flag value` pairs.
fn options(args: &[String]) -> HashMap<String, String> {
    args.chunks(2)
        .filter_map(|pair| match pair {
            [flag, value] => Some((flag.trim_start_matches("--").to_string(), value.clone())),
            _ => None,
        })
        .collect()
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

/// Signs a license and prints the block.
fn issue(opts: &HashMap<String, String>) -> Result<(), String> {
    let key_path = required(opts, "key")?;
    let text = if key_path == STDIN {
        read_secret_from_stdin()?
    } else {
        std::fs::read_to_string(key_path).map_err(|e| format!("{key_path}: {e}"))?
    };
    let key = read_key(text.trim())
        .ok_or_else(|| format!("{key_path}: not a signing key, 64 hex digits"))?;

    let edition_text = required(opts, "edition")?;
    let edition = Edition::parse(edition_text)
        .ok_or_else(|| format!("--edition must be personal or commercial, not {edition_text}"))?;
    let seats = match opts.get("seats") {
        Some(s) => s
            .parse()
            .map_err(|_| format!("--seats: not a number: {s}"))?,
        None => 1,
    };
    let issued = match opts.get("issued") {
        Some(s) => Date::parse(s).ok_or_else(|| format!("--issued: not a YYYY-MM-DD date: {s}"))?,
        None => Date::today(),
    };
    let license = License {
        name: required(opts, "name")?.to_string(),
        email: required(opts, "email")?.to_string(),
        edition,
        seats,
        id: license::new_id().map_err(|e| e.to_string())?,
        issued,
        updates_until: issued.add_years(license::UPDATE_YEARS),
    };
    print!("{}", license.sign(&key));
    Ok(())
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
