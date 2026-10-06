//! Licenses: signed text blocks, checked offline.
//!
//! A license is a block of `Key: value` lines between BEGIN and END lines,
//! ending in an Ed25519 signature over the other lines. The app checks the
//! signature against a public key compiled in, so no server is involved and
//! a license keeps working offline forever. Without one the app works fully
//! and now and then reminds the user that it is unregistered; see
//! LICENSING.md for the format, the keys and how licenses are issued.

use std::fmt;
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

/// The lines a license block starts and ends with.
pub const BEGIN: &str = "----- BEGIN REWIND VM LICENSE -----";
pub const END: &str = "----- END REWIND VM LICENSE -----";

/// The key licenses are checked against. Its signing half is kept in
/// 1Password and never touches this repository or a disk; LICENSING.md
/// says how licenses are issued with it and how to replace the pair.
pub const PUBLIC_KEY: [u8; 32] = [
    0x38, 0x14, 0x5a, 0x44, 0x46, 0x40, 0x0d, 0x25, 0x18, 0x32, 0x50, 0x28, 0x7b, 0xe7, 0x8e, 0x70,
    0x93, 0x66, 0x54, 0x46, 0x06, 0x3c, 0xe1, 0xf6, 0x53, 0x92, 0xb2, 0x23, 0xe6, 0x12, 0x14, 0x33,
];

/// License ids the app refuses: refunded or leaked keys.
pub const REVOKED: &[&str] = &[];

/// The day this version of the app was released. A license whose updates
/// ended before it registers only the versions released until then; this
/// one runs as an evaluation, fully, with the reminders.
pub const RELEASE_DATE: Date = Date {
    year: 2026,
    month: 10,
    day: 4,
};

/// Years of updates a license of either edition includes, from the day
/// it was issued.
pub const UPDATE_YEARS: i32 = 3;

/// The file a license is kept in, under the user's config directory.
const CONFIG_SUBDIR: &str = "rewind";
const LICENSE_FILE: &str = "license.txt";
const XDG_CONFIG_ENV: &str = "XDG_CONFIG_HOME";

/// The license file's permissions: its owner reads and writes it, nobody
/// else reads it.
const LICENSE_MODE: u32 = 0o600;
const HOME_CONFIG_DIR: &str = ".config";

/// The license's fields, in the order the signature covers them.
mod field {
    pub const NAME: &str = "Name";
    pub const EMAIL: &str = "Email";
    pub const EDITION: &str = "Edition";
    pub const SEATS: &str = "Seats";
    pub const ID: &str = "Id";
    pub const ISSUED: &str = "Issued";
    pub const UPDATES_UNTIL: &str = "Updates-Until";
    pub const SIGNATURE: &str = "Signature";

    pub const SIGNED: [&str; 7] = [NAME, EMAIL, EDITION, SEATS, ID, ISSUED, UPDATES_UNTIL];
}

/// Who a license is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edition {
    Personal,
    Commercial,
}

impl Edition {
    pub fn as_str(self) -> &'static str {
        match self {
            Edition::Personal => "Personal",
            Edition::Commercial => "Commercial",
        }
    }

    /// Reads an edition, in any case.
    pub fn parse(text: &str) -> Option<Edition> {
        match text.trim().to_ascii_lowercase().as_str() {
            "personal" => Some(Edition::Personal),
            "commercial" => Some(Edition::Commercial),
            _ => None,
        }
    }
}

/// A calendar day. Field order makes the derived ordering chronological.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Date {
    pub year: i32,
    pub month: u8,
    pub day: u8,
}

/// Days between 0000-03-01 and 1970-01-01 in the proleptic Gregorian
/// calendar, for the day-count conversions below.
const EPOCH_SHIFT_DAYS: i64 = 719_468;
const DAYS_PER_ERA: i64 = 146_097;
const YEARS_PER_ERA: i64 = 400;
const SECONDS_PER_DAY: u64 = 86_400;
const MONTHS: u8 = 12;

impl Date {
    /// Reads YYYY-MM-DD.
    pub fn parse(text: &str) -> Option<Date> {
        let mut parts = text.trim().splitn(3, '-');
        let year: i32 = parts.next()?.parse().ok()?;
        let month: u8 = parts.next()?.parse().ok()?;
        let day: u8 = parts.next()?.parse().ok()?;
        let valid = (1..=MONTHS).contains(&month) && day >= 1 && day <= days_in_month(year, month);
        valid.then_some(Date { year, month, day })
    }

    /// Today in UTC.
    pub fn today() -> Date {
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs();
        Date::from_days((seconds / SECONDS_PER_DAY) as i64)
    }

    /// The same day `years` later; the 29th of February becomes the 28th
    /// in a year without one.
    pub fn add_years(self, years: i32) -> Date {
        let year = self.year + years;
        let day = self.day.min(days_in_month(year, self.month));
        Date {
            year,
            month: self.month,
            day,
        }
    }

    /// The day `days` after 1970-01-01 (Howard Hinnant's civil_from_days).
    fn from_days(days: i64) -> Date {
        let z = days + EPOCH_SHIFT_DAYS;
        let era = z.div_euclid(DAYS_PER_ERA);
        let doe = z - era * DAYS_PER_ERA;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
        let month = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
        let year = (yoe + era * YEARS_PER_ERA) as i32 + i32::from(month <= 2);
        Date { year, month, day }
    }
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

fn is_leap(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i32, month: u8) -> u8 {
    const FEBRUARY: u8 = 2;
    const LONG: [u8; 7] = [1, 3, 5, 7, 8, 10, 12];
    match month {
        FEBRUARY if is_leap(year) => 29,
        FEBRUARY => 28,
        m if LONG.contains(&m) => 31,
        _ => 30,
    }
}

/// What a license says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct License {
    pub name: String,
    pub email: String,
    pub edition: Edition,
    pub seats: u32,
    pub id: String,
    pub issued: Date,
    pub updates_until: Date,
}

/// Why a license block was refused, in words for the dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LicenseError {
    NoBlock,
    Missing(&'static str),
    Unknown(String),
    BadValue { field: &'static str, value: String },
    BadSignature,
    Revoked(String),
}

impl fmt::Display for LicenseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LicenseError::NoBlock => write!(
                f,
                "No license block found. Paste everything from the BEGIN line to the END line."
            ),
            LicenseError::Missing(name) => write!(f, "The license has no {name} line."),
            LicenseError::Unknown(line) => {
                write!(f, "The license has a line the app does not know: {line}")
            }
            LicenseError::BadValue { field, value } => {
                write!(f, "The license's {field} line does not read: {value}")
            }
            LicenseError::BadSignature => write!(
                f,
                "The signature does not match. The block was changed after it was issued, or it was cut short."
            ),
            LicenseError::Revoked(id) => write!(f, "License {id} has been revoked."),
        }
    }
}

impl std::error::Error for LicenseError {}

/// Whether a valid license covers this version of the app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Coverage {
    Current,
    /// Updates ended before this version was released, which runs as an
    /// evaluation; the license registers the versions released until then.
    EndedBefore(Date),
}

impl License {
    /// The signed payload: every field but the signature, as `Key: value`
    /// lines in the fixed order.
    pub fn payload(&self) -> String {
        let values = [
            self.name.clone(),
            self.email.clone(),
            self.edition.as_str().to_string(),
            self.seats.to_string(),
            self.id.clone(),
            self.issued.to_string(),
            self.updates_until.to_string(),
        ];
        let mut out = String::new();
        for (key, value) in field::SIGNED.iter().zip(values) {
            out.push_str(&format!("{key}: {value}\n"));
        }
        out
    }

    /// Signs the license and returns the block to send the buyer.
    pub fn sign(&self, key: &SigningKey) -> String {
        let signature = key.sign(self.payload().as_bytes());
        self.block(&signature.to_bytes())
    }

    /// The block for the license and its `signature`, as it was issued.
    fn block(&self, signature: &[u8]) -> String {
        format!(
            "{BEGIN}\n{}{}: {}\n{END}\n",
            self.payload(),
            field::SIGNATURE,
            BASE64.encode(signature)
        )
    }

    pub fn coverage(&self) -> Coverage {
        if self.updates_until < RELEASE_DATE {
            return Coverage::EndedBefore(self.updates_until);
        }
        Coverage::Current
    }
}

/// Characters some mail clients put in place of a space.
const NO_BREAK_SPACES: [char; 2] = ['\u{a0}', '\u{202f}'];

/// What a reply puts before each line it quotes.
const QUOTE: char = '>';

/// A pasted line as it was issued: with no-break spaces as spaces, and
/// without the quote marks replies added or the spaces around it.
fn unquoted(line: &str) -> String {
    let line = line.replace(NO_BREAK_SPACES, " ");
    let mut rest = line.trim();
    while let Some(inner) = rest.strip_prefix(QUOTE) {
        rest = inner.trim_start();
    }
    rest.to_string()
}

/// A line inside the block.
enum Line<'a> {
    /// `Key: value`, for a key the format has.
    Field(&'static str, &'a str),
    /// No key: the rest of the line before, which a mail client wrapped.
    Continues,
}

/// What `line` is, or that it names a key the format does not have.
fn classify(line: &str) -> Result<Line<'_>, LicenseError> {
    let Some((key, value)) = line.split_once(':') else {
        return Ok(Line::Continues);
    };
    let key = key.trim();
    let known = field::SIGNED
        .iter()
        .chain(std::iter::once(&field::SIGNATURE))
        .find(|k| **k == key);
    if let Some(known) = known {
        return Ok(Line::Field(known, value));
    }

    // A word and a colon is a field this build does not know; anything
    // else with a colon in it is wrapped text.
    let a_key = !key.is_empty() && key.chars().all(|c| c.is_ascii_alphabetic() || c == '-');
    if a_key {
        return Err(LicenseError::Unknown(line.to_string()));
    }
    Ok(Line::Continues)
}

/// Reads a license block out of pasted text: anything around the BEGIN and
/// END lines is ignored, as are CR line ends, spaces around lines, the
/// quote marks of a reply, no-break spaces, and lines a mail client
/// wrapped. Returns the license and its signature, unchecked.
pub fn parse(text: &str) -> Result<(License, Vec<u8>), LicenseError> {
    let lines: Vec<String> = text.lines().map(unquoted).collect();
    let begin = lines
        .iter()
        .position(|l| l == BEGIN)
        .ok_or(LicenseError::NoBlock)?;
    let end = lines[begin..]
        .iter()
        .position(|l| l == END)
        .map(|i| begin + i)
        .ok_or(LicenseError::NoBlock)?;

    // Each line inside is `Key: value`, and blank lines are skipped. A
    // line with no key goes on the end of the value before it: the
    // signature's base64 as is, other values after the space the wrap
    // took.
    let mut values: Vec<(&str, String)> = Vec::new();
    for line in &lines[begin + 1..end] {
        if line.is_empty() {
            continue;
        }
        if let Line::Field(key, value) = classify(line)? {
            values.push((key, value.trim().to_string()));
            continue;
        }
        let Some((key, value)) = values.last_mut() else {
            return Err(LicenseError::Unknown(line.clone()));
        };
        if *key != field::SIGNATURE && !value.is_empty() {
            value.push(' ');
        }
        value.push_str(line);
    }
    let get = |name: &'static str| {
        values
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.as_str())
            .ok_or(LicenseError::Missing(name))
    };
    let bad = |field: &'static str, value: &str| LicenseError::BadValue {
        field,
        value: value.to_string(),
    };

    let edition_text = get(field::EDITION)?;
    let seats_text = get(field::SEATS)?;
    let issued_text = get(field::ISSUED)?;
    let until_text = get(field::UPDATES_UNTIL)?;
    let signature_text = get(field::SIGNATURE)?;
    let license = License {
        name: get(field::NAME)?.to_string(),
        email: get(field::EMAIL)?.to_string(),
        edition: Edition::parse(edition_text).ok_or_else(|| bad(field::EDITION, edition_text))?,
        seats: seats_text
            .parse()
            .map_err(|_| bad(field::SEATS, seats_text))?,
        id: get(field::ID)?.to_string(),
        issued: Date::parse(issued_text).ok_or_else(|| bad(field::ISSUED, issued_text))?,
        updates_until: Date::parse(until_text)
            .ok_or_else(|| bad(field::UPDATES_UNTIL, until_text))?,
    };
    let signature = BASE64
        .decode(signature_text)
        .map_err(|_| bad(field::SIGNATURE, signature_text))?;
    Ok((license, signature))
}

/// Parses and checks a license block against `public_key`, refusing the
/// ids in `revoked`.
pub fn verify_with(
    text: &str,
    public_key: &[u8; 32],
    revoked: &[&str],
) -> Result<License, LicenseError> {
    let (license, signature) = parse(text)?;
    let key = VerifyingKey::from_bytes(public_key).map_err(|_| LicenseError::BadSignature)?;
    let signature = Signature::from_slice(&signature).map_err(|_| LicenseError::BadSignature)?;
    key.verify_strict(license.payload().as_bytes(), &signature)
        .map_err(|_| LicenseError::BadSignature)?;
    if revoked.contains(&license.id.as_str()) {
        return Err(LicenseError::Revoked(license.id));
    }
    Ok(license)
}

/// Checks a license block against the key compiled into the app.
pub fn verify(text: &str) -> Result<License, LicenseError> {
    verify_with(text, &PUBLIC_KEY, REVOKED)
}

/// Where the license is kept: $XDG_CONFIG_HOME/rewind/license.txt, else
/// ~/.config/rewind/license.txt.
pub fn license_path() -> Option<PathBuf> {
    let config = std::env::var_os(XDG_CONFIG_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(HOME_CONFIG_DIR)))?;
    Some(config.join(CONFIG_SUBDIR).join(LICENSE_FILE))
}

/// What the app knows about its license at start-up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Registration {
    Unregistered,
    Registered(License),
    /// A license file is there but does not check out.
    Invalid(String),
}

impl Registration {
    /// Whether this version runs registered: under a license that checks
    /// out and whose updates had not ended when this version was released.
    pub fn covers_this_version(&self) -> bool {
        match self {
            Registration::Registered(license) => license.coverage() == Coverage::Current,
            Registration::Unregistered | Registration::Invalid(_) => false,
        }
    }
}

/// Reads and checks the stored license.
pub fn load() -> Registration {
    let Some(path) = license_path() else {
        return Registration::Unregistered;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Registration::Unregistered;
    };
    match verify(&text) {
        Ok(license) => Registration::Registered(license),
        Err(e) => Registration::Invalid(format!("{}: {e}", path.display())),
    }
}

/// Stores the license block in `text`, which must check out, where the
/// app looks for it.
pub fn save(text: &str) -> std::io::Result<PathBuf> {
    let path =
        license_path().ok_or_else(|| std::io::Error::other("no HOME to keep the license in"))?;
    save_to(&path, text, &PUBLIC_KEY)?;
    Ok(path)
}

/// Stores the license block in `text` at `path`, checked against
/// `public_key`: the block alone, as it was issued, whatever was pasted
/// around it, readable by its owner only. The file is written whole or
/// not at all, to a temporary file renamed over the old one.
pub fn save_to(path: &Path, text: &str, public_key: &[u8; 32]) -> std::io::Result<()> {
    let invalid = |e: LicenseError| std::io::Error::new(std::io::ErrorKind::InvalidData, e);
    let license = verify_with(text, public_key, REVOKED).map_err(invalid)?;
    let (_, signature) = parse(text).map_err(invalid)?;
    let block = license.block(&signature);

    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("the license path has no directory"))?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{LICENSE_FILE}.{}", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(LICENSE_MODE)
        .open(&tmp)?;

    // A file left by an earlier attempt keeps its own permissions, which
    // the mode above does not change.
    file.set_permissions(std::fs::Permissions::from_mode(LICENSE_MODE))?;
    file.write_all(block.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)
}

/// When to remind an unregistered user: after every
/// `ACTIONS_PER_REMINDER` engine actions or `USE_PER_REMINDER` of use,
/// whichever comes first. Either one resets both.
#[derive(Clone, Debug, Default)]
pub struct Reminder {
    actions: u32,
    last: Duration,
}

pub const ACTIONS_PER_REMINDER: u32 = 20;
pub const USE_PER_REMINDER: Duration = Duration::from_secs(45 * 60);

impl Reminder {
    /// Counts an engine action at `now` (time in use so far). Returns
    /// whether to remind.
    pub fn action(&mut self, now: Duration) -> bool {
        self.actions += 1;
        self.due(now)
    }

    /// Checks the clock at `now`. Returns whether to remind.
    pub fn tick(&mut self, now: Duration) -> bool {
        self.due(now)
    }

    fn due(&mut self, now: Duration) -> bool {
        let due = self.actions >= ACTIONS_PER_REMINDER
            || now.saturating_sub(self.last) >= USE_PER_REMINDER;
        if due {
            self.actions = 0;
            self.last = now;
        }
        due
    }
}

/// A new license id: 16 random hex digits.
pub fn new_id() -> std::io::Result<String> {
    const ID_BYTES: usize = 8;
    let bytes = random_bytes::<ID_BYTES>()?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Bytes from the kernel's random source.
pub fn random_bytes<const N: usize>() -> std::io::Result<[u8; N]> {
    use std::io::Read;
    const RANDOM_DEVICE: &str = "/dev/urandom";
    let mut bytes = [0u8; N];
    std::fs::File::open(RANDOM_DEVICE)?.read_exact(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    // License blocks end to end: licenses are signed with a key derived
    // from a fixed seed, then parsed and checked. The compiled-in key's
    // signing half is not available here, so it is only checked to be a
    // valid public key.
    use super::*;

    const TEST_SEED: [u8; 32] = [7; 32];

    fn sample() -> License {
        License {
            name: "Ada Lovelace".into(),
            email: "ada@example.com".into(),
            edition: Edition::Commercial,
            seats: 3,
            id: "0123456789abcdef".into(),
            issued: Date::parse("2026-09-30").unwrap(),
            updates_until: Date::parse("2027-09-30").unwrap(),
        }
    }

    fn test_key() -> SigningKey {
        SigningKey::from_bytes(&TEST_SEED)
    }

    fn public() -> [u8; 32] {
        test_key().verifying_key().to_bytes()
    }

    #[test]
    fn a_signed_license_verifies() {
        // Sign, then check against the matching public key.
        let block = sample().sign(&test_key());
        assert_eq!(verify_with(&block, &public(), &[]).unwrap(), sample());
    }

    #[test]
    fn the_compiled_in_key_is_a_valid_public_key() {
        // PUBLIC_KEY is a point on the curve, so a pasted key with a typo
        // fails here rather than rejecting every license.
        assert!(VerifyingKey::from_bytes(&PUBLIC_KEY).is_ok());
    }

    #[test]
    fn a_block_from_rewind_license_verifies() {
        // A block in exactly the format `rewind-license issue` prints,
        // signed with the test key, checked the way the app checks.
        let license = verify_with(TEST_ISSUED, &public(), &[]).unwrap();
        assert_eq!(license.name, "Rewind Developer");
        assert_eq!(license.edition, Edition::Personal);
    }

    #[test]
    fn a_tampered_field_fails() {
        // One more seat than was signed for.
        let block = sample().sign(&test_key()).replace("Seats: 3", "Seats: 4");
        assert_eq!(
            verify_with(&block, &public(), &[]),
            Err(LicenseError::BadSignature)
        );
    }

    #[test]
    fn a_revoked_id_fails() {
        // A good signature on an id in the revoked list.
        let block = sample().sign(&test_key());
        assert_eq!(
            verify_with(&block, &public(), &["0123456789abcdef"]),
            Err(LicenseError::Revoked("0123456789abcdef".into()))
        );
    }

    #[test]
    fn pasting_tolerates_crlf_and_surrounding_text() {
        // An email client's CRLF line ends, indentation, and text before
        // and after the block.
        let block = sample().sign(&test_key());
        let pasted = format!(
            "Thanks for buying!\r\n\r\n{}\r\n  -- the Rewind team  \r\n",
            block
                .lines()
                .map(|l| format!("   {l}  "))
                .collect::<Vec<_>>()
                .join("\r\n")
        );
        assert_eq!(verify_with(&pasted, &public(), &[]).unwrap(), sample());
    }

    #[test]
    fn pasting_tolerates_what_mail_clients_do_to_a_block() {
        // A mail client wraps lines near 78 characters, which breaks the
        // 99 character Signature line inside its base64 and a long name
        // at a space; a reply quotes every line with "> ", once or more;
        // and some clients turn spaces into no-break spaces. The block
        // still verifies.
        const WRAP: usize = 78;
        let mut license = sample();
        license.name =
            "Augusta Ada King, Countess of Lovelace, Translator of the Analytical Engine Notes"
                .into();
        let block = license.sign(&test_key());
        let wrapped: String = block
            .lines()
            .flat_map(|line| {
                if line.len() <= WRAP {
                    return vec![line.to_string()];
                }
                // Break at the last space before the limit, else hard.
                let cut = line[..WRAP].rfind(' ').unwrap_or(WRAP);
                let (head, tail) = line.split_at(cut);
                vec![head.to_string(), tail.trim_start().to_string()]
            })
            .map(|l| format!("{l}\n"))
            .collect();
        assert!(wrapped.lines().count() > block.lines().count());
        assert_eq!(verify_with(&wrapped, &public(), &[]).unwrap(), license);

        let quoted: String = block.lines().map(|l| format!("> > {l}\n")).collect();
        assert_eq!(verify_with(&quoted, &public(), &[]).unwrap(), license);

        let no_break = block.replace(' ', "\u{a0}");
        assert_eq!(verify_with(&no_break, &public(), &[]).unwrap(), license);
    }

    #[test]
    fn a_saved_license_is_the_block_alone_readable_only_by_its_owner() {
        // Whatever was pasted around a quoted block, the file holds the
        // block as it was issued, and only its owner can read it. Saving
        // again replaces the file whole and leaves no temporary file.
        use std::os::unix::fs::PermissionsExt;
        const OWNER_ONLY: u32 = 0o600;
        const MODE_BITS: u32 = 0o777;

        let block = sample().sign(&test_key());
        let pasted: String = std::iter::once("Thanks for buying!\n".to_string())
            .chain(block.lines().map(|l| format!("> {l}\n")))
            .collect();
        let dir = std::env::temp_dir().join(format!("rewind-license-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join(LICENSE_FILE);

        save_to(&path, &pasted, &public()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), block);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & MODE_BITS, OWNER_ONLY);

        save_to(&path, &block, &public()).unwrap();
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        assert_eq!(
            save_to(&path, "hello", &public()).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn broken_blocks_say_what_is_wrong() {
        // No block at all, a missing line, and an unknown line.
        assert_eq!(parse("hello").unwrap_err(), LicenseError::NoBlock);
        let block = sample().sign(&test_key());
        let no_email: String = block
            .lines()
            .filter(|l| !l.starts_with("Email"))
            .map(|l| format!("{l}\n"))
            .collect();
        assert_eq!(
            parse(&no_email).unwrap_err(),
            LicenseError::Missing("Email")
        );
        let extra = block.replace("Seats: 3", "Seats: 3\nColor: blue");
        assert!(matches!(
            parse(&extra).unwrap_err(),
            LicenseError::Unknown(_)
        ));
    }

    #[test]
    fn coverage_follows_the_updates_date() {
        // Updates until the day before release no longer cover it; until
        // the release day they do.
        let mut license = sample();
        license.updates_until = Date::parse("2026-09-29").unwrap();
        assert_eq!(
            license.coverage(),
            Coverage::EndedBefore(license.updates_until)
        );
        license.updates_until = RELEASE_DATE;
        assert_eq!(license.coverage(), Coverage::Current);
    }

    #[test]
    fn a_license_whose_updates_ended_leaves_this_version_evaluating() {
        // A license registers the versions released while its updates
        // ran. A version released after they ended runs as an evaluation,
        // as an unregistered copy or one with a license that does not
        // check out does.
        let mut license = sample();
        license.updates_until = RELEASE_DATE;
        assert!(Registration::Registered(license.clone()).covers_this_version());
        license.updates_until = Date::parse("2026-09-29").unwrap();
        assert!(!Registration::Registered(license).covers_this_version());
        assert!(!Registration::Unregistered.covers_this_version());
        assert!(!Registration::Invalid("bad".into()).covers_this_version());
    }

    #[test]
    fn dates_parse_add_years_and_count_days() {
        // Leap days, bad days, and the day count behind `today`.
        assert!(Date::parse("2026-02-29").is_none());
        assert!(Date::parse("2028-02-29").is_some());
        assert_eq!(
            Date::parse("2028-02-29").unwrap().add_years(1).to_string(),
            "2029-02-28"
        );
        assert_eq!(Date::from_days(0).to_string(), "1970-01-01");
        assert_eq!(Date::from_days(20_726).to_string(), "2026-09-30");
    }

    #[test]
    fn reminders_come_after_twenty_actions_or_forty_five_minutes() {
        // Nineteen actions do not remind and the twentieth does; with no
        // actions the clock reminds at forty-five minutes, and each
        // reminder starts both counts over.
        let mut r = Reminder::default();
        let minute = Duration::from_secs(60);
        for _ in 1..ACTIONS_PER_REMINDER {
            assert!(!r.action(minute));
        }
        assert!(r.action(minute));
        assert!(!r.tick(minute * 45));
        assert!(r.tick(minute * 46));
        assert!(!r.tick(minute * 47));
    }

    /// The block `rewind-license issue` prints for this license, signed
    /// with the test key.
    const TEST_ISSUED: &str = "\
----- BEGIN REWIND VM LICENSE -----
Name: Rewind Developer
Email: dev@rewindvm.dev
Edition: Personal
Seats: 1
Id: aeceda4e86dbf3b3
Issued: 2026-09-30
Updates-Until: 2029-09-30
Signature: dZ5wrWQHhkzg1yZ+/tn+d4/TchtDagIAZ0RFz0MBs+OVuv+/rZZTYKBo6bQ93dmEluHRCKn6b3apL4KwxzzPBw==
----- END REWIND VM LICENSE -----
";
}
