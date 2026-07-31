//! Allocation-free classification of mandatory MCP metadata headers.

use std::borrow::Cow;
use std::fmt;

use mcp_usage_core::{Method, name, validate_header};

/// Trusted protocol metadata lifted from request headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolHeaders<'a> {
    /// Classified method. Known core methods do not allocate.
    pub method: Method,
    /// Decoded `Mcp-Name`, borrowed for ordinary ASCII values.
    pub name: Option<Cow<'a, str>>,
}

/// Why mandatory MCP request metadata could not be trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClassificationError {
    /// Protocol version is missing, legacy, or malformed.
    UntrustedVersion(String),
    /// `Mcp-Method` was absent.
    MissingMethod,
    /// A name-bearing method omitted `Mcp-Name`.
    MissingName,
    /// The sentinel-encoded name was malformed.
    InvalidName(String),
}

impl fmt::Display for ClassificationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UntrustedVersion(reason) => write!(f, "untrusted MCP headers: {reason}"),
            Self::MissingMethod => f.write_str("missing Mcp-Method header"),
            Self::MissingName => f.write_str("missing Mcp-Name header for name-bearing method"),
            Self::InvalidName(reason) => write!(f, "invalid Mcp-Name header: {reason}"),
        }
    }
}

impl std::error::Error for ClassificationError {}

/// Validate and classify raw MCP header values.
///
/// The common path (known method and plain ASCII name) performs no allocation.
///
/// # Errors
///
/// Returns [`ClassificationError`] for untrusted protocol versions, missing
/// mandatory metadata, or malformed Base64 sentinel values.
pub fn classify_protocol_headers<'a>(
    version_header: Option<&str>,
    method_header: Option<&str>,
    name_header: Option<&'a str>,
) -> Result<ProtocolHeaders<'a>, ClassificationError> {
    if let Err(reason) = validate_header(version_header) {
        return Err(ClassificationError::UntrustedVersion(reason.to_string()));
    }
    let method = Method::parse(method_header.ok_or(ClassificationError::MissingMethod)?);
    if method.carries_name() && name_header.is_none() {
        return Err(ClassificationError::MissingName);
    }
    let name = name_header
        .map(name::decode)
        .transpose()
        .map_err(|error| ClassificationError::InvalidName(error.to_string()))?;
    Ok(ProtocolHeaders { method, name })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_path_borrows_the_name() {
        let classified =
            classify_protocol_headers(Some("2026-07-28"), Some("tools/call"), Some("weather"))
                .unwrap();
        assert_eq!(classified.method, Method::ToolsCall);
        assert!(matches!(classified.name, Some(Cow::Borrowed("weather"))));
    }

    #[test]
    fn legacy_requests_are_rejected_before_headers_are_trusted() {
        let error =
            classify_protocol_headers(Some("2025-11-25"), Some("tools/call"), Some("cheap_tool"))
                .unwrap_err();
        assert!(matches!(error, ClassificationError::UntrustedVersion(_)));
    }

    #[test]
    fn name_is_required_only_where_the_spec_requires_it() {
        assert!(matches!(
            classify_protocol_headers(Some("2026-07-28"), Some("tools/call"), None),
            Err(ClassificationError::MissingName)
        ));
        assert!(classify_protocol_headers(Some("2026-07-28"), Some("tools/list"), None).is_ok());
    }

    #[test]
    fn header_hot_path_has_no_body_dependency() {
        // Invalid JSON is used here: classification still succeeds because the
        // common request path never deserializes a body.
        let invalid_body = b"this is not JSON";
        let classified =
            classify_protocol_headers(Some("2026-07-28"), Some("tools/call"), Some("weather"))
                .unwrap();
        assert_eq!(classified.method, Method::ToolsCall);
        assert_eq!(invalid_body, b"this is not JSON");
    }
}
