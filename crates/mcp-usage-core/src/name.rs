//! Decoding the `Mcp-Name` header value.
//!
//! `Mcp-Name` mirrors `params.name` (a tool or prompt name) or `params.uri` (a
//! resource URI). Tool names are only SHOULD-constrained to header-safe
//! characters and URIs are not constrained at all, so the spec defines a sentinel
//! escape: a value that cannot ride in an ASCII header field arrives as
//!
//! ```text
//! Mcp-Name: =?base64?{Base64EncodedValue}?=
//! ```
//!
//! The markers are lowercase and case-sensitive, and the payload is Base64 over
//! the UTF-8 bytes using the **standard** alphabet (the spec's own example
//! `=?base64?PT9iYXNlNjQ/bGl0ZXJhbD89?=` contains `/`, so it is not URL-safe
//! Base64).
//!
//! This matters to a meter for one blunt reason: the price book is keyed on tool
//! name. Matching `=?base64?...?=` against it does not merely fail to find the
//! tool, it silently prices every non-ASCII-named tool at the default rate. The
//! spec requires servers and intermediaries that inspect these values to "decode
//! them accordingly" before comparing.
//!
//! Clients MUST also encode any plain-ASCII value that happens to look like the
//! sentinel, so decoding is unambiguous: anything wearing the markers is encoded.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use std::borrow::Cow;
use std::fmt;

/// Opening marker of the Base64 sentinel form.
pub const SENTINEL_PREFIX: &str = "=?base64?";
/// Closing marker of the Base64 sentinel form.
pub const SENTINEL_SUFFIX: &str = "?=";

/// Why a `Mcp-Name` value could not be decoded.
///
/// Both variants mean the header wore the sentinel markers but its payload was
/// not a valid Base64-encoded UTF-8 string. A conforming client cannot produce
/// either, so the right response is to reject the request rather than guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameError {
    /// The payload between the markers was not valid standard-alphabet Base64.
    NotBase64,
    /// The payload decoded, but the bytes were not valid UTF-8.
    NotUtf8,
}

impl fmt::Display for NameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotBase64 => f.write_str("Mcp-Name sentinel payload is not valid base64"),
            Self::NotUtf8 => f.write_str("Mcp-Name sentinel payload is not valid UTF-8"),
        }
    }
}

impl std::error::Error for NameError {}

/// Decode a raw `Mcp-Name` header value to the name the body actually carries.
///
/// A plain value is returned borrowed, so the common path allocates nothing.
/// Only the sentinel form allocates, and only because it must.
///
/// A value that opens with the prefix but does not close with the suffix is
/// treated as a literal rather than an error: it is not wearing the complete
/// sentinel, so by the spec's own encoding rule it cannot be an encoded value.
///
/// # Errors
///
/// Returns [`NameError`] when the value wears both markers but the payload
/// between them is not Base64-encoded UTF-8.
pub fn decode(raw: &str) -> Result<Cow<'_, str>, NameError> {
    let Some(rest) = raw.strip_prefix(SENTINEL_PREFIX) else {
        return Ok(Cow::Borrowed(raw));
    };
    let Some(payload) = rest.strip_suffix(SENTINEL_SUFFIX) else {
        return Ok(Cow::Borrowed(raw));
    };

    let bytes = STANDARD.decode(payload).map_err(|_| NameError::NotBase64)?;
    let decoded = String::from_utf8(bytes).map_err(|_| NameError::NotUtf8)?;
    Ok(Cow::Owned(decoded))
}

/// Whether a raw header value is wearing the complete Base64 sentinel.
#[must_use]
pub fn is_sentinel(raw: &str) -> bool {
    raw.strip_prefix(SENTINEL_PREFIX)
        .is_some_and(|rest| rest.ends_with(SENTINEL_SUFFIX))
}

/// Encode a value into the sentinel form. Test and client-side helper.
#[must_use]
pub fn encode(value: &str) -> String {
    format!(
        "{SENTINEL_PREFIX}{}{SENTINEL_SUFFIX}",
        STANDARD.encode(value.as_bytes())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_ascii_passes_through_borrowed() {
        let decoded = decode("get_weather").expect("plain value decodes");
        assert_eq!(decoded, "get_weather");
        assert!(
            matches!(decoded, Cow::Borrowed(_)),
            "plain path must not allocate"
        );
    }

    #[test]
    fn decodes_the_specs_own_examples() {
        // Straight from the Value Encoding table in the Streamable HTTP page.
        assert_eq!(
            decode("=?base64?SGVsbG8sIOS4lueVjA==?=").unwrap(),
            "Hello, 世界"
        );
        assert_eq!(decode("=?base64?IHBhZGRlZCA=?=").unwrap(), " padded ");
        assert_eq!(
            decode("=?base64?bGluZTEKbGluZTI=?=").unwrap(),
            "line1\nline2"
        );
        // A literal that merely looks like the sentinel, which clients must encode
        // which keeps this case unambiguous.
        assert_eq!(
            decode("=?base64?PT9iYXNlNjQ/bGl0ZXJhbD89?=").unwrap(),
            "=?base64?literal?="
        );
    }

    #[test]
    fn uses_the_standard_alphabet_not_url_safe() {
        // The literal example above encodes to a payload containing '/'. Under the
        // URL-safe alphabet that byte would be '_' and the decode would disagree.
        let encoded = encode("=?base64?literal?=");
        assert!(
            encoded.contains('/'),
            "expected standard-alphabet output: {encoded}"
        );
        assert_eq!(decode(&encoded).unwrap(), "=?base64?literal?=");
    }

    #[test]
    fn resource_uris_survive_the_round_trip() {
        let uri = "file:///projects/myapp/config.json";
        assert_eq!(decode(uri).unwrap(), uri);
        assert_eq!(decode(&encode(uri)).unwrap(), uri);
    }

    #[test]
    fn empty_sentinel_payload_decodes_to_empty() {
        assert_eq!(decode("=?base64??=").unwrap(), "");
    }

    #[test]
    fn partial_markers_are_literals_not_errors() {
        // Opens the sentinel but never closes it: not an encoded value.
        assert_eq!(decode("=?base64?SGVsbG8=").unwrap(), "=?base64?SGVsbG8=");
        // Closes but never opens.
        assert_eq!(decode("SGVsbG8=?=").unwrap(), "SGVsbG8=?=");
        // Prefix immediately followed by the suffix marker's tail only.
        assert_eq!(decode("=?base64?=").unwrap(), "=?base64?=");
    }

    #[test]
    fn markers_are_case_sensitive() {
        // "These markers are case-sensitive and MUST appear exactly as shown
        // (lowercase)." An uppercase spelling is a literal name.
        assert_eq!(
            decode("=?BASE64?SGVsbG8=?=").unwrap(),
            "=?BASE64?SGVsbG8=?="
        );
        assert!(!is_sentinel("=?BASE64?SGVsbG8=?="));
    }

    #[test]
    fn malformed_payloads_are_rejected_rather_than_guessed() {
        assert_eq!(
            decode("=?base64?not!valid!base64?=").unwrap_err(),
            NameError::NotBase64
        );
        // 0xFF is not valid UTF-8.
        let bad_utf8 = format!(
            "{SENTINEL_PREFIX}{}{SENTINEL_SUFFIX}",
            STANDARD.encode([0xFF])
        );
        assert_eq!(decode(&bad_utf8).unwrap_err(), NameError::NotUtf8);
    }

    #[test]
    fn is_sentinel_agrees_with_decode() {
        for raw in [
            "get_weather",
            "=?base64?SGVsbG8=?=",
            "=?base64?SGVsbG8=",
            "=?base64??=",
        ] {
            let sentinel = is_sentinel(raw);
            let changed = !matches!(decode(raw), Ok(Cow::Borrowed(_)));
            assert_eq!(sentinel, changed, "disagreement on {raw}");
        }
    }

    #[test]
    fn round_trips_arbitrary_unicode() {
        for value in [
            "",
            "a",
            "日本語",
            "emoji 🦀 tool",
            "  spaced  ",
            "tab\there",
        ] {
            assert_eq!(
                decode(&encode(value)).unwrap(),
                value,
                "failed on {value:?}"
            );
        }
    }
}
