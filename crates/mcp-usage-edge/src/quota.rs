//! Admission gate for tenants that are over quota.
//!
//! This sits *outside* the meter, so a refused call is rejected before it is
//! classified, priced, or forwarded. Nothing is recorded for it, which is the
//! property that matters: a tenant over its cap should cost its operator
//! nothing, not a free upstream call.
//!
//! ## What this does and does not promise
//!
//! The gate admits on the totals the control plane last published, so its
//! resolution is one refresh interval. A tenant can overshoot by whatever it
//! spends inside that window. That is the correct trade for a sidecar: the
//! alternative is a synchronous reservation call to the plane on every MCP
//! request, which would put the plane's availability directly in front of the
//! customer's traffic, and the whole design exists to avoid that.
//!
//! Tightening the window is a matter of lowering the refresh interval. Making
//! the bound exact would need a reservation hook inside the meter itself, which
//! is a library change rather than something to bolt on here.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use http::{HeaderMap, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use mcp_usage_kit::{API_KEY_HEADER, LimitDecision, LimitReason, MeterBody, assess_limits};
use tower::{Layer, Service};

use crate::control_plane::ControlPlaneTenantStore;

/// Installs [`QuotaService`].
///
/// Always present in the stack, enabled or not, so the sidecar has one service
/// type regardless of configuration. A disabled gate is a plain pass-through.
#[derive(Debug, Clone)]
pub struct QuotaLayer {
    store: Option<Arc<ControlPlaneTenantStore>>,
}

impl QuotaLayer {
    /// Gate admissions against a control-plane-backed cache.
    #[must_use]
    pub const fn new(store: Arc<ControlPlaneTenantStore>) -> Self {
        Self { store: Some(store) }
    }

    /// A gate that admits everything.
    ///
    /// Used when there is no control plane, because quota needs authoritative
    /// counters and a statically configured sidecar has none.
    #[must_use]
    pub const fn disabled() -> Self {
        Self { store: None }
    }
}

impl<S> Layer<S> for QuotaLayer {
    type Service = QuotaService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        QuotaService {
            inner,
            store: self.store.clone(),
        }
    }
}

/// Refuses requests from tenants already over their quota or spend cap.
#[derive(Debug, Clone)]
pub struct QuotaService<S> {
    inner: S,
    store: Option<Arc<ControlPlaneTenantStore>>,
}

/// The single presented credential, or `None` when it is absent or ambiguous.
///
/// Deliberately lenient: anything this cannot resolve is passed through so the
/// meter's own credential gate produces the refusal. Two components disagreeing
/// about what counts as a valid credential is worse than one doing the work.
fn presented_key(headers: &HeaderMap) -> Option<&str> {
    let api_key = headers.get(API_KEY_HEADER).and_then(|v| v.to_str().ok());
    let bearer = headers
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|value| {
            let (scheme, token) = value.split_once(' ')?;
            scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
        });

    match (api_key, bearer) {
        (Some(key), None) | (None, Some(key)) if !key.is_empty() => Some(key),
        _ => None,
    }
}

/// Static, low-cardinality code for a refusal, so an agent can branch on it
/// without parsing prose.
const fn reason_code(reason: LimitReason) -> &'static str {
    match reason {
        LimitReason::QuotaExceeded => "quota_exceeded",
        LimitReason::SpendCapExceeded => "spend_cap_exceeded",
        LimitReason::ArithmeticOverflow => "usage_unrepresentable",
    }
}

fn refuse(reason: LimitReason) -> Response<MeterBody> {
    let code = reason_code(reason);
    let body = Full::new(Bytes::from(format!(
        r#"{{"error":"{code}","retryable":false}}"#
    )));
    let mut response = Response::new(
        body.map_err(|never: Infallible| match never {})
            .boxed_unsync(),
    );
    *response.status_mut() = StatusCode::TOO_MANY_REQUESTS;
    response.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    response
}

impl<S, B> Service<Request<B>> for QuotaService<S>
where
    S: Service<Request<B>, Response = Response<MeterBody>, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
    B: Send + 'static,
{
    type Response = Response<MeterBody>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request<B>) -> Self::Future {
        // An unknown key, or a cache too stale to trust, yields no verdict here.
        // Both are the meter's business, and it will refuse them.
        let verdict = self
            .store
            .as_ref()
            .and_then(|store| presented_key(request.headers()).and_then(|key| store.quota_for(key)))
            .map(|(committed, unit_price_micros, limits)| {
                assess_limits(committed, 0, unit_price_micros, limits)
            });

        if let Some(LimitDecision::Rejected(reason)) = verdict {
            tracing::info!(reason = reason_code(reason), "refusing a call over quota");
            return Box::pin(async move { Ok(refuse(reason)) });
        }

        let replacement = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, replacement);
        Box::pin(async move { inner.call(request).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        headers
    }

    #[test]
    fn either_credential_header_is_read() {
        assert_eq!(presented_key(&headers(&[("x-api-key", "k1")])), Some("k1"));
        assert_eq!(
            presented_key(&headers(&[("authorization", "Bearer k2")])),
            Some("k2")
        );
        assert_eq!(
            presented_key(&headers(&[("authorization", "bearer k3")])),
            Some("k3"),
            "the scheme is case insensitive"
        );
    }

    #[test]
    fn an_ambiguous_or_absent_credential_yields_no_verdict() {
        // Passing through lets the meter produce one refusal with one set of
        // rules, instead of two components disagreeing.
        assert_eq!(presented_key(&headers(&[])), None);
        assert_eq!(
            presented_key(&headers(&[
                ("x-api-key", "k1"),
                ("authorization", "Bearer k2")
            ])),
            None
        );
        assert_eq!(
            presented_key(&headers(&[("authorization", "Basic k")])),
            None
        );
        assert_eq!(presented_key(&headers(&[("x-api-key", "")])), None);
    }

    #[test]
    fn refusal_codes_are_static_and_distinct() {
        assert_eq!(reason_code(LimitReason::QuotaExceeded), "quota_exceeded");
        assert_eq!(
            reason_code(LimitReason::SpendCapExceeded),
            "spend_cap_exceeded"
        );
        assert_eq!(
            reason_code(LimitReason::ArithmeticOverflow),
            "usage_unrepresentable"
        );
    }

    #[test]
    fn a_refusal_is_a_429_with_a_machine_readable_code() {
        let response = refuse(LimitReason::QuotaExceeded);
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response.headers().get(http::header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
    }
}
