//! The reverse proxy that `MeterLayer` wraps.
//!
//! `MeterService` is generic over its inner service, so the sidecar is the same
//! meter an embedded application gets, with a proxy substituted for the
//! in-process MCP handler. The metering rules in `mcp-usage-core` are untouched
//! and untouchable from here, which is the point: the sidecar cannot bill
//! differently from the library.
//!
//! Upstream failures become HTTP responses rather than service errors. That is
//! ordinary reverse-proxy behaviour, and it also lands the billing outcome in
//! the right place: a 502 carries no JSON-RPC terminal result, so the meter
//! observes no delivery and charges nothing.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http::header::HeaderName;
use http::{Request, Response, StatusCode, Uri};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use tower::Service;

/// Failure while streaming an upstream response body.
///
/// A concrete type rather than a boxed error because `MeterService` requires
/// `ResponseBody::Error: std::error::Error`, and `Box<dyn Error + Send + Sync>`
/// is unsized and therefore does not implement it.
#[derive(Debug, thiserror::Error)]
#[error("upstream response body failed: {0}")]
pub struct UpstreamBodyError(#[from] hyper::Error);

/// Response body produced by the proxy.
///
/// Boxed so a streamed upstream body and a synthesized error body share one
/// type. Boxing does not buffer: frames are still polled lazily, so an SSE
/// stream stays a stream.
pub type ProxyBody = UnsyncBoxBody<Bytes, UpstreamBodyError>;

/// Connector the sidecar dials with. HTTPS-capable when the `tls` feature is on.
#[cfg(feature = "tls")]
pub type Connector = hyper_rustls::HttpsConnector<HttpConnector>;
/// Connector the sidecar dials with. Plaintext only without the `tls` feature.
#[cfg(not(feature = "tls"))]
pub type Connector = HttpConnector;

/// The pooled client the sidecar uses for every outbound call.
///
/// Shared between the upstream proxy and the control-plane adapters so one
/// connection pool and one TLS configuration serve both.
pub type HttpsClient = Client<Connector, Full<Bytes>>;

/// Build a client over the configured connector.
#[must_use]
pub fn build_client() -> HttpsClient {
    Client::builder(TokioExecutor::new()).build(build_connector())
}

/// Hop-by-hop headers, which belong to a single connection and must not be
/// forwarded. See RFC 9110 section 7.6.1.
const HOP_BY_HOP: [HeaderName; 8] = [
    http::header::CONNECTION,
    http::header::PROXY_AUTHENTICATE,
    http::header::PROXY_AUTHORIZATION,
    http::header::TE,
    http::header::TRAILER,
    http::header::TRANSFER_ENCODING,
    http::header::UPGRADE,
    HeaderName::from_static("keep-alive"),
];

fn strip_hop_by_hop(headers: &mut http::HeaderMap) {
    for name in HOP_BY_HOP {
        headers.remove(name);
    }
}

/// Forwards metered requests to the configured upstream MCP server.
#[derive(Clone)]
pub struct UpstreamProxy {
    client: Client<Connector, Full<Bytes>>,
    scheme: http::uri::Scheme,
    authority: http::uri::Authority,
    base_path: String,
    timeout: Duration,
}

impl std::fmt::Debug for UpstreamProxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamProxy")
            .field("scheme", &self.scheme)
            .field("authority", &self.authority.as_str())
            .field("base_path", &self.base_path)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

/// Why an upstream could not be turned into a proxy.
#[derive(Debug, thiserror::Error)]
pub enum ProxyBuildError {
    /// The URL lacked a scheme or an authority.
    #[error("upstream URL {0:?} needs a scheme and a host")]
    NotAbsolute(String),
    /// The URL used a scheme the sidecar cannot dial.
    #[error("upstream scheme {0:?} is not supported; use http or https")]
    UnsupportedScheme(String),
    /// The URL was syntactically invalid.
    #[error("upstream URL {0:?} is not a valid URI")]
    Invalid(String),
}

impl UpstreamProxy {
    /// Build a proxy for an absolute `http` or `https` base URL.
    ///
    /// Any path on the base URL is treated as a prefix and prepended to each
    /// forwarded request path, so a sidecar can front a server mounted under a
    /// sub-path.
    ///
    /// # Errors
    ///
    /// Returns [`ProxyBuildError`] if the URL is not absolute, is malformed, or
    /// names a scheme other than `http` or `https`.
    pub fn new(base_url: &str, timeout: Duration) -> Result<Self, ProxyBuildError> {
        Self::with_client(build_client(), base_url, timeout)
    }

    /// Build a proxy over an existing client, sharing its connection pool.
    ///
    /// # Errors
    ///
    /// Returns [`ProxyBuildError`] if the URL is not absolute, is malformed, or
    /// names a scheme other than `http` or `https`.
    pub fn with_client(
        client: HttpsClient,
        base_url: &str,
        timeout: Duration,
    ) -> Result<Self, ProxyBuildError> {
        let uri: Uri = base_url
            .parse()
            .map_err(|_| ProxyBuildError::Invalid(base_url.to_owned()))?;
        let (Some(scheme), Some(authority)) = (uri.scheme(), uri.authority()) else {
            return Err(ProxyBuildError::NotAbsolute(base_url.to_owned()));
        };
        if scheme != &http::uri::Scheme::HTTP && scheme != &http::uri::Scheme::HTTPS {
            return Err(ProxyBuildError::UnsupportedScheme(scheme.to_string()));
        }
        #[cfg(not(feature = "tls"))]
        if scheme == &http::uri::Scheme::HTTPS {
            return Err(ProxyBuildError::UnsupportedScheme(
                "https (this build has the tls feature disabled)".to_owned(),
            ));
        }

        // A bare "/" carries no prefix, and a trailing slash would double up
        // against the request path, which always starts with one.
        let base_path = uri.path().trim_end_matches('/').to_owned();

        Ok(Self {
            client,
            scheme: scheme.clone(),
            authority: authority.clone(),
            base_path,
            timeout,
        })
    }

    /// Rewrite an inbound origin-form URI onto the upstream authority.
    fn upstream_uri(&self, uri: &Uri) -> Result<Uri, http::Error> {
        let path_and_query = uri.path_and_query().map_or("/", |value| value.as_str());
        let joined = if self.base_path.is_empty() {
            path_and_query.to_owned()
        } else {
            format!("{}{path_and_query}", self.base_path)
        };
        Uri::builder()
            .scheme(self.scheme.clone())
            .authority(self.authority.clone())
            .path_and_query(joined)
            .build()
    }
}

#[cfg(feature = "tls")]
fn build_connector() -> Connector {
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    hyper_rustls::HttpsConnectorBuilder::new()
        .with_native_roots()
        .expect("no native root certificates are available")
        .https_or_http()
        .enable_http1()
        .wrap_connector(http)
}

#[cfg(not(feature = "tls"))]
fn build_connector() -> Connector {
    HttpConnector::new()
}

/// Build a small error response that carries no JSON-RPC result, so the meter
/// observes no delivery and bills nothing.
fn gateway_error(status: StatusCode, detail: &str) -> Response<ProxyBody> {
    let body = Full::new(Bytes::from(format!(r#"{{"error":"{detail}"}}"#)));
    let mut response = Response::new(
        body.map_err(|never: Infallible| match never {})
            .boxed_unsync(),
    );
    *response.status_mut() = status;
    response.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    response
}

impl Service<Request<Full<Bytes>>> for UpstreamProxy {
    type Response = Response<ProxyBody>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: Request<Full<Bytes>>) -> Self::Future {
        let client = self.client.clone();
        let timeout = self.timeout;
        let rewritten = self.upstream_uri(request.uri());

        Box::pin(async move {
            let Ok(uri) = rewritten else {
                tracing::error!("cannot construct an upstream URI for this request path");
                return Ok(gateway_error(StatusCode::BAD_GATEWAY, "bad upstream uri"));
            };

            let (mut parts, body) = request.into_parts();
            parts.uri = uri;
            strip_hop_by_hop(&mut parts.headers);
            // Hyper derives Host from the URI authority. Leaving the inbound
            // value would advertise the sidecar's own hostname upstream.
            parts.headers.remove(http::header::HOST);
            let forwarded = Request::from_parts(parts, body);

            match tokio::time::timeout(timeout, client.request(forwarded)).await {
                Ok(Ok(response)) => Ok(pass_through(response)),
                Ok(Err(error)) => {
                    tracing::warn!(%error, "upstream request failed");
                    Ok(gateway_error(
                        StatusCode::BAD_GATEWAY,
                        "upstream unavailable",
                    ))
                }
                Err(_elapsed) => {
                    tracing::warn!(
                        timeout_seconds = timeout.as_secs(),
                        "upstream did not respond before the configured timeout"
                    );
                    Ok(gateway_error(
                        StatusCode::GATEWAY_TIMEOUT,
                        "upstream timeout",
                    ))
                }
            }
        })
    }
}

/// Adapt an upstream response for the meter without buffering it.
fn pass_through(response: Response<Incoming>) -> Response<ProxyBody> {
    let (mut parts, body) = response.into_parts();
    strip_hop_by_hop(&mut parts.headers);
    Response::from_parts(parts, body.map_err(UpstreamBodyError::from).boxed_unsync())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proxy(base: &str) -> UpstreamProxy {
        UpstreamProxy::new(base, Duration::from_secs(1)).expect("builds")
    }

    fn rewrite(base: &str, request_uri: &str) -> String {
        let uri: Uri = request_uri.parse().expect("valid request uri");
        proxy(base)
            .upstream_uri(&uri)
            .expect("rewrites")
            .to_string()
    }

    #[test]
    fn the_request_path_is_forwarded_onto_the_upstream_authority() {
        assert_eq!(
            rewrite("http://127.0.0.1:3000", "/mcp"),
            "http://127.0.0.1:3000/mcp"
        );
    }

    #[test]
    fn a_query_string_survives_rewriting() {
        assert_eq!(
            rewrite("http://127.0.0.1:3000", "/mcp?sessionId=abc"),
            "http://127.0.0.1:3000/mcp?sessionId=abc"
        );
    }

    #[test]
    fn a_base_path_becomes_a_prefix() {
        assert_eq!(
            rewrite("http://origin.internal/api/v1", "/mcp"),
            "http://origin.internal/api/v1/mcp"
        );
    }

    #[test]
    fn a_trailing_slash_on_the_base_does_not_double_up() {
        assert_eq!(
            rewrite("http://origin.internal/api/", "/mcp"),
            "http://origin.internal/api/mcp"
        );
        assert_eq!(
            rewrite("http://origin.internal/", "/mcp"),
            "http://origin.internal/mcp"
        );
    }

    #[test]
    fn a_relative_base_url_is_refused() {
        assert!(matches!(
            UpstreamProxy::new("/mcp", Duration::from_secs(1)),
            Err(ProxyBuildError::NotAbsolute(_))
        ));
    }

    #[test]
    fn a_non_http_scheme_is_refused() {
        assert!(matches!(
            UpstreamProxy::new("ftp://origin.internal", Duration::from_secs(1)),
            Err(ProxyBuildError::UnsupportedScheme(_))
        ));
    }

    #[test]
    fn hop_by_hop_headers_are_not_forwarded() {
        let mut headers = http::HeaderMap::new();
        headers.insert(http::header::CONNECTION, "keep-alive".parse().unwrap());
        headers.insert(
            HeaderName::from_static("keep-alive"),
            "timeout=5".parse().unwrap(),
        );
        headers.insert(http::header::TRANSFER_ENCODING, "chunked".parse().unwrap());
        // An end-to-end header a metered MCP call depends on.
        headers.insert(
            http::header::CONTENT_TYPE,
            "application/json".parse().unwrap(),
        );

        strip_hop_by_hop(&mut headers);

        assert!(!headers.contains_key(http::header::CONNECTION));
        assert!(!headers.contains_key(HeaderName::from_static("keep-alive")));
        assert!(!headers.contains_key(http::header::TRANSFER_ENCODING));
        assert!(headers.contains_key(http::header::CONTENT_TYPE));
    }

    #[test]
    fn a_gateway_error_carries_no_terminal_result() {
        // The billing rule keys on a JSON-RPC `resultType: complete`. A gateway
        // error must never look like one, or an outage would start billing.
        let response = gateway_error(StatusCode::BAD_GATEWAY, "upstream unavailable");
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }
}
