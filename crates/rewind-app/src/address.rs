//! The browser viewer's address: `?url=<rwd>&compare=<rwd>&step=<n>`,
//! the export to open, the one to compare it with, and the step to put
//! the playhead on. The page reads it at start, and the app writes it back
//! as the playhead moves, so the address is always a link to what is on
//! screen.

/// The parameters' names.
const URL: &str = "url";
const COMPARE: &str = "compare";
const STEP: &str = "step";

/// What the address asks the viewer to show.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Address {
    /// The export to open, as given: absolute, or relative to the page.
    pub url: Option<String>,
    /// The export to compare it with.
    pub compare: Option<String>,
    /// Where to put the playhead; None picks the failure, or the end.
    pub step: Option<u64>,
}

impl Address {
    /// The address a query string asks for, with or without its leading
    /// `?`. Unknown parameters, empty values and a step that is not a
    /// number are left out.
    pub fn parse(query: &str) -> Address {
        let mut address = Address::default();
        let query = query.strip_prefix('?').unwrap_or(query);
        for pair in query.split('&') {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            let value = decode(value);

            // Empty values say nothing.
            if value.is_empty() {
                continue;
            }
            match decode(name).as_str() {
                URL => address.url = Some(value),
                COMPARE => address.compare = Some(value),
                STEP => address.step = value.replace(',', "").parse().ok(),
                _ => {}
            }
        }
        address
    }

    /// The query string for this address, with its leading `?`, or the
    /// empty string when there is no export to link to: a step means
    /// nothing without one.
    pub fn query(&self) -> String {
        let Some(url) = &self.url else {
            return String::new();
        };
        let mut query = format!("?{URL}={}", encode(url));
        if let Some(compare) = &self.compare {
            query.push_str(&format!("&{COMPARE}={}", encode(compare)));
        }
        if let Some(step) = self.step {
            query.push_str(&format!("&{STEP}={step}"));
        }
        query
    }
}

/// Characters a value keeps as they are in the query: the unreserved
/// ones, and the slash and colon of a link, so the address stays readable.
fn kept(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"-._~/:".contains(&byte)
}

/// `value` with every other byte percent-encoded.
fn encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if kept(byte) {
            encoded.push(byte as char);
            continue;
        }
        encoded.push_str(&format!("%{byte:02X}"));
    }
    encoded
}

/// `value` with its percent escapes decoded and each + read as a space,
/// as a browser's form encoding writes one. A % that starts no escape is
/// kept as it is, and bytes that are not UTF-8 are replaced.
fn decode(value: &str) -> String {
    const ESCAPE_LEN: usize = 3;
    const HEX: u32 = 16;
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let escaped = bytes
            .get(i + 1..i + ESCAPE_LEN)
            .filter(|_| bytes[i] == b'%')
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, HEX).ok());
        if let Some(byte) = escaped {
            decoded.push(byte);
            i += ESCAPE_LEN;
            continue;
        }
        decoded.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

#[cfg(test)]
mod tests {
    // The address in both directions: query strings the page may be opened
    // with parse to what they ask for, and what the app writes back parses
    // to the same address.
    use super::*;

    #[test]
    fn an_address_names_the_exports_and_the_step() {
        // Both exports and the step, the step written with thousands
        // separators as the app shows steps, and the leading ? optional.
        let address = Address::parse(
            "?url=examples/mylib-fail.rwd&compare=examples/mylib-pass.rwd&step=5,775",
        );
        assert_eq!(address.url.as_deref(), Some("examples/mylib-fail.rwd"));
        assert_eq!(address.compare.as_deref(), Some("examples/mylib-pass.rwd"));
        assert_eq!(address.step, Some(5775));
        assert_eq!(Address::parse("url=a.rwd"), Address::parse("?url=a.rwd"));
    }

    #[test]
    fn values_are_percent_decoded() {
        // A link pasted into the address, its own query encoded inside
        // ours, and a + standing for a space.
        let address =
            Address::parse("?url=https%3A%2F%2Fci.example%2Frun.rwd%3Fsig%3Da%26b&step=7");
        assert_eq!(
            address.url.as_deref(),
            Some("https://ci.example/run.rwd?sig=a&b")
        );
        assert_eq!(
            Address::parse("url=my+run.rwd").url.as_deref(),
            Some("my run.rwd")
        );
        assert_eq!(
            Address::parse("url=caf%C3%A9.rwd").url.as_deref(),
            Some("café.rwd")
        );
    }

    #[test]
    fn what_does_not_read_is_left_out() {
        // A step that is not a number, empty values, unknown parameters and
        // a stray percent sign.
        let address = Address::parse("?url=&step=soon&utm_source=x&compare=b%zz.rwd");
        assert_eq!(address.url, None);
        assert_eq!(address.step, None);
        assert_eq!(address.compare.as_deref(), Some("b%zz.rwd"));
        assert_eq!(Address::parse(""), Address::default());
    }

    #[test]
    fn the_query_written_back_reads_back_the_same() {
        // The app's address for a comparison at a step, with a link whose
        // own query has to be escaped, parses back to itself; slashes and
        // colons stay readable.
        let address = Address {
            url: Some("https://ci.example/run.rwd?sig=a&b".into()),
            compare: Some("examples/mylib-pass.rwd".into()),
            step: Some(5775),
        };
        let query = address.query();
        assert_eq!(
            query,
            "?url=https://ci.example/run.rwd%3Fsig%3Da%26b&compare=examples/mylib-pass.rwd&step=5775"
        );
        assert_eq!(Address::parse(&query), address);
    }

    #[test]
    fn without_an_export_the_query_is_empty() {
        // A run opened from a dropped file has no link to share.
        let address = Address {
            url: None,
            compare: None,
            step: Some(12),
        };
        assert_eq!(address.query(), "");
    }
}
