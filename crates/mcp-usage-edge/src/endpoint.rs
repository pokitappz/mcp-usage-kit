//! Validated outbound endpoints with sanitized diagnostics.

use http::Uri;

/// An invalid or insecure outbound endpoint. Never contains the supplied URL.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error(
    "endpoint must use HTTPS (HTTP is allowed only on literal loopback), without credentials, query or fragment"
)]
pub struct EndpointError;

/// Validate an HTTP endpoint, optionally permitting plaintext upstream traffic.
///
/// # Errors
/// Returns a sanitized error for an invalid or insecure endpoint.
pub fn validate(value: &str, allow_plaintext_upstream: bool) -> Result<Uri, EndpointError> {
    let uri: Uri = value.parse().map_err(|_| EndpointError)?;
    let authority = uri.authority().ok_or(EndpointError)?;
    let loopback = uri
        .host()
        .and_then(|host| {
            host.trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .ok()
        })
        .is_some_and(|ip| ip.is_loopback());
    if authority.as_str().contains('@') || value.contains('#') || uri.query().is_some() {
        return Err(EndpointError);
    }
    match uri.scheme_str() {
        Some("https") => {
            #[cfg(not(feature = "tls"))]
            return Err(EndpointError);
        }
        Some("http") if allow_plaintext_upstream || loopback => {}
        _ => return Err(EndpointError),
    }
    Ok(uri)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_endpoints_require_tls_except_literal_loopback() {
        for endpoint in ["http://127.0.0.1:9000/api", "http://[::1]:9000/api"] {
            assert!(validate(endpoint, false).is_ok());
        }
        for endpoint in [
            "http://localhost/api",
            "http://plane.example/api",
            "ftp://127.0.0.1/api",
            "https://user:secret@example.com",
            "https://example.com/?token=secret",
            "https://example.com/#secret",
        ] {
            let error = validate(endpoint, false).unwrap_err();
            assert!(!format!("{error:?} {error}").contains("secret@example"));
        }
        assert!(validate("http://private-upstream:3000/mcp", true).is_ok());
    }
}
