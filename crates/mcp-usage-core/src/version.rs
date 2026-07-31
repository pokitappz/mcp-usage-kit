//! Protocol version, and the guard that decides whether the mirrored headers may
//! be trusted at all.
//!
//! The mirrored headers (`Mcp-Method`, `Mcp-Name`, `Mcp-Param-*`) are only
//! usable because 2026-07-28 obliges the origin server to reject any request
//! whose headers disagree with its body, with `-32020 HeaderMismatch`. On an
//! earlier revision no such obligation exists, so the client controls those
//! values. The specification gives intermediaries this requirement:
//!
//! > Intermediaries that enforce policy based on mirrored headers (e.g., routing
//! > or rate-limiting by tenant) SHOULD verify that the `MCP-Protocol-Version`
//! > header indicates a version that requires header-body validation. If the
//! > version is older or the header is absent, the intermediary SHOULD reject the
//! > request rather than trusting unvalidated header values.
//!
//! Note "reject", not "fall back to parsing the body". A meter that quietly
//! degrades to body parsing on a legacy request is metering a request the origin
//! will never validate, and a client that wants to be billed for a cheap tool
//! while calling an expensive one only has to claim an old protocol version.

use std::fmt;

/// The revision from which header/body validation is mandatory, and therefore
/// from which mirrored headers can be trusted.
pub const HEADER_VALIDATION_SINCE: &str = "2026-07-28";

/// A dated MCP protocol version, e.g. `2026-07-28`.
///
/// Dated versions are `YYYY-MM-DD`, so byte ordering is chronological ordering
/// and comparison needs no date parsing. That only holds for well-formed values,
/// which is why [`ProtocolVersion::parse`] validates the shape rather than
/// accepting any string.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProtocolVersion(String);

impl ProtocolVersion {
    /// Parse a dated version string, returning `None` if it is not `YYYY-MM-DD`.
    ///
    /// Non-dated labels such as `draft` are rejected. They carry no
    /// ordering, so admitting them would make the comparison below meaningless.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        valid_date(raw).then(|| Self(raw.to_owned()))
    }

    /// The wire string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this revision obliges the origin to validate headers against the
    /// body, and therefore whether an intermediary may price on those headers.
    #[must_use]
    pub fn requires_header_validation(&self) -> bool {
        self.0.as_str() >= HEADER_VALIDATION_SINCE
    }
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The verdict on whether mirrored headers may drive billing for this request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderTrust {
    /// The request declares a revision that mandates header/body validation.
    Trusted(ProtocolVersion),
    /// The request must be rejected rather than metered on its headers.
    Reject(TrustFailure),
}

impl HeaderTrust {
    /// Whether the headers may be used to price this request.
    #[must_use]
    pub const fn is_trusted(&self) -> bool {
        matches!(self, Self::Trusted(_))
    }
}

/// Why mirrored headers could not be trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustFailure {
    /// No `MCP-Protocol-Version` header. Pre-2025-06-18 clients omitted it, so
    /// its absence means an era with no validation obligation.
    MissingVersionHeader,
    /// A well-formed revision that predates the validation requirement.
    UnvalidatedRevision(ProtocolVersion),
    /// Not a dated version at all. Unorderable, so it cannot be shown to be new
    /// enough, and an intermediary must not assume in the client's favour.
    MalformedVersion(String),
}

impl fmt::Display for TrustFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingVersionHeader => {
                f.write_str("missing MCP-Protocol-Version header; mirrored headers are unvalidated")
            }
            Self::UnvalidatedRevision(v) => write!(
                f,
                "protocol version {v} predates {HEADER_VALIDATION_SINCE}; \
                 mirrored headers are not validated against the body"
            ),
            Self::MalformedVersion(v) => write!(f, "malformed MCP-Protocol-Version value {v:?}"),
        }
    }
}

/// Decide whether this request's mirrored headers may drive billing.
///
/// Pass the raw `MCP-Protocol-Version` header value, or `None` when it is absent.
#[must_use]
pub fn assess(version_header: Option<&str>) -> HeaderTrust {
    match validate_header(version_header) {
        Ok(()) => HeaderTrust::Trusted(ProtocolVersion(
            version_header.unwrap_or_default().to_owned(),
        )),
        Err(failure) => HeaderTrust::Reject(failure),
    }
}

/// Validate a protocol-version header without allocating on the trusted path.
///
/// This is the entry point for latency-sensitive intermediaries that need only
/// a yes/no trust decision. Owned strings are created solely for rejection
/// diagnostics.
///
/// # Errors
///
/// Returns the precise [`TrustFailure`] for missing, malformed, or legacy
/// revisions.
pub fn validate_header(version_header: Option<&str>) -> Result<(), TrustFailure> {
    let Some(raw) = version_header else {
        return Err(TrustFailure::MissingVersionHeader);
    };
    if !valid_date(raw) {
        return Err(TrustFailure::MalformedVersion(raw.to_owned()));
    }
    if raw < HEADER_VALIDATION_SINCE {
        return Err(TrustFailure::UnvalidatedRevision(ProtocolVersion(
            raw.to_owned(),
        )));
    }
    Ok(())
}

fn valid_date(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    if ![0, 1, 2, 3, 5, 6, 8, 9]
        .iter()
        .all(|&i| bytes[i].is_ascii_digit())
    {
        return false;
    }
    let year = parse_digits(&bytes[0..4]);
    let month = parse_digits(&bytes[5..7]);
    let day = parse_digits(&bytes[8..10]);
    let max_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => return false,
    };
    day != 0 && day <= max_day
}

fn parse_digits(bytes: &[u8]) -> u32 {
    bytes
        .iter()
        .fold(0, |value, byte| value * 10 + u32::from(byte - b'0'))
}

const fn is_leap_year(year: u32) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_revisions_parse_and_order_chronologically() {
        let revisions = [
            "2024-11-05",
            "2025-03-26",
            "2025-06-18",
            "2025-11-25",
            "2026-07-28",
        ];
        let parsed: Vec<ProtocolVersion> = revisions
            .iter()
            .map(|r| ProtocolVersion::parse(r).expect("known revision parses"))
            .collect();

        let mut sorted = parsed.clone();
        sorted.sort();
        assert_eq!(parsed, sorted, "byte order must equal chronological order");
    }

    #[test]
    fn only_the_current_revision_mandates_validation() {
        for old in ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"] {
            let v = ProtocolVersion::parse(old).unwrap();
            assert!(
                !v.requires_header_validation(),
                "{old} must not be treated as validating"
            );
        }
        assert!(
            ProtocolVersion::parse("2026-07-28")
                .unwrap()
                .requires_header_validation()
        );
    }

    #[test]
    fn future_revisions_are_trusted() {
        // A later dated revision cannot remove the validation requirement without
        // a new spec, and refusing to meter traffic from a newer client would take
        // the gateway down on release day.
        for future in ["2026-08-01", "2027-01-01", "2099-12-31"] {
            assert!(
                ProtocolVersion::parse(future)
                    .unwrap()
                    .requires_header_validation(),
                "{future} should be trusted"
            );
        }
    }

    #[test]
    fn malformed_versions_are_rejected_not_parsed() {
        for bad in [
            "draft",
            "latest",
            "2026-7-28",
            "2026/07/28",
            "20260728",
            "2026-07-28 ",
            "",
            "202X-07-28",
            "2026-00-10",
            "2026-13-01",
            "2026-04-31",
            "2026-02-29",
            "2024-02-30",
        ] {
            assert!(
                ProtocolVersion::parse(bad).is_none(),
                "{bad:?} should not parse"
            );
        }
    }

    #[test]
    fn leap_days_are_calendar_validated() {
        assert!(ProtocolVersion::parse("2024-02-29").is_some());
        assert!(ProtocolVersion::parse("2000-02-29").is_some());
        assert!(ProtocolVersion::parse("2100-02-29").is_none());
    }

    #[test]
    fn assess_rejects_absent_header() {
        assert_eq!(
            assess(None),
            HeaderTrust::Reject(TrustFailure::MissingVersionHeader)
        );
    }

    #[test]
    fn assess_rejects_legacy_rather_than_falling_back() {
        // The failure mode this guards: a client claiming an old revision so its
        // unvalidated Mcp-Name is believed by the meter.
        let verdict = assess(Some("2025-11-25"));
        assert!(!verdict.is_trusted());
        assert!(matches!(
            verdict,
            HeaderTrust::Reject(TrustFailure::UnvalidatedRevision(_))
        ));
    }

    #[test]
    fn assess_rejects_garbage_without_assuming_in_the_clients_favour() {
        let verdict = assess(Some("not-a-version"));
        assert!(!verdict.is_trusted());
        assert!(matches!(
            verdict,
            HeaderTrust::Reject(TrustFailure::MalformedVersion(_))
        ));
    }

    #[test]
    fn assess_trusts_the_current_revision() {
        let verdict = assess(Some("2026-07-28"));
        assert!(verdict.is_trusted());
        assert_eq!(
            verdict,
            HeaderTrust::Trusted(ProtocolVersion::parse("2026-07-28").unwrap())
        );
    }
}
