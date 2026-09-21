//! Deciding whether a call may proceed, and offering a way to pay when it may not.
//!
//! This sits *outside* the meter, so a refused call is rejected before it is
//! classified, priced or forwarded, and nothing is recorded for it. A tenant
//! over its cap should cost its operator nothing, not a free upstream call.
//!
//! Two ways past the gate:
//!
//! - **Prepaid.** The tenant is inside the quota the control plane published.
//! - **Paid.** The tenant presents a settled MPP credential for this exact
//!   call. The gate refuses with `402 Payment Required` and a
//!   `WWW-Authenticate: Payment` challenge; the agent pays and retries.
//!
//! ## What quota does and does not promise
//!
//! Admission uses the totals the control plane last published, so its
//! resolution is one refresh interval and a tenant can overshoot by whatever it
//! spends inside that window. The alternative is a synchronous reservation call
//! to the plane on every MCP request, which would put the plane's availability
//! directly in front of customer traffic, and the whole design exists to avoid
//! that. With payments enabled the overshoot matters less: past the cap the
//! next call is priced rather than refused.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use http::{HeaderMap, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use mcp_usage_kit::{
    API_KEY_HEADER, LimitDecision, LimitReason, METHOD_HEADER, MeterBody, Method, NAME_HEADER,
    PriceBook, assess_limits,
};
use tower::{Layer, Service};

use crate::control_plane::ControlPlaneTenantStore;
use crate::mpp::{PaymentError, Payments, chrono_lite::Rfc3339};

/// Largest request body the gate will buffer to bind a payment challenge to.
const DEFAULT_MAX_BODY: usize = 1024 * 1024;

/// Installs [`AdmissionService`].
///
/// Always present in the stack, enabled or not, so the sidecar has one service
/// type regardless of configuration. With neither quota nor payments it is a
/// pass-through that only buffers the body.
#[derive(Clone)]
pub struct AdmissionLayer {
    store: Option<Arc<ControlPlaneTenantStore>>,
    payments: Option<Arc<Payments>>,
    max_body: usize,
}

impl std::fmt::Debug for AdmissionLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdmissionLayer")
            .field("quota", &self.store.is_some())
            .field("payments", &self.payments.as_ref().map(|p| p.method()))
            .finish_non_exhaustive()
    }
}

impl AdmissionLayer {
    /// A gate that admits everything.
    ///
    /// Quota needs authoritative counters, which a statically configured
    /// sidecar does not have.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            store: None,
            payments: None,
            max_body: DEFAULT_MAX_BODY,
        }
    }

    /// Gate admissions against a control-plane-backed cache.
    #[must_use]
    pub fn with_quota(mut self, store: Arc<ControlPlaneTenantStore>) -> Self {
        self.store = Some(store);
        self
    }

    /// Offer MPP payment instead of refusing an over-quota call.
    #[must_use]
    pub fn with_payments(mut self, payments: Arc<Payments>) -> Self {
        self.payments = Some(payments);
        self
    }

    /// Bound the body the gate buffers.
    #[must_use]
    pub const fn with_max_body(mut self, bytes: usize) -> Self {
        self.max_body = bytes;
        self
    }
}

impl<S> Layer<S> for AdmissionLayer {
    type Service = AdmissionService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AdmissionService {
            inner,
            store: self.store.clone(),
            payments: self.payments.clone(),
            max_body: self.max_body,
        }
    }
}

/// Refuses, prices, or admits a call.
#[derive(Clone)]
pub struct AdmissionService<S> {
    inner: S,
    store: Option<Arc<ControlPlaneTenantStore>>,
    payments: Option<Arc<Payments>>,
    max_body: usize,
}

/// The single presented tenant credential, or `None` when absent or ambiguous.
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

/// What this call would be metered at, from the headers the meter itself uses.
///
/// Returns `None` for a request the meter would refuse to classify, which the
/// gate leaves to the meter rather than guessing a price for.
fn priced_units(headers: &HeaderMap, prices: &PriceBook) -> Option<u64> {
    let method = headers
        .get(METHOD_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(Method::parse)?;
    let name = headers
        .get(NAME_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| mcp_usage_kit::core::name::decode(value).ok());
    Some(prices.units_for(&method, name.as_deref()))
}

/// Convert metered units into the currency's minor unit, rounding up.
///
/// `unit_price_micros` is millionths of a currency unit and a minor unit is a
/// hundredth of one, so this is a divide by 10,000. Rounding up because the
/// alternative is serving a call for less than it costs.
const fn minor_units(units: u64, unit_price_micros: u64) -> Option<u64> {
    let Some(micros) = units.checked_mul(unit_price_micros) else {
        return None;
    };
    Some(micros.div_ceil(10_000))
}

/// Static, low-cardinality code, so an agent can branch without parsing prose.
const fn reason_code(reason: LimitReason) -> &'static str {
    match reason {
        LimitReason::QuotaExceeded => "quota_exceeded",
        LimitReason::SpendCapExceeded => "spend_cap_exceeded",
        LimitReason::ArithmeticOverflow => "usage_unrepresentable",
    }
}

fn body_from(text: String) -> MeterBody {
    Full::new(Bytes::from(text))
        .map_err(|never: Infallible| match never {})
        .boxed_unsync()
}

/// A plain refusal, for a deployment with no payment method configured.
fn refuse_quota(reason: LimitReason) -> Response<MeterBody> {
    let code = reason_code(reason);
    let mut response = Response::new(body_from(format!(
        r#"{{"error":"{code}","retryable":false}}"#
    )));
    *response.status_mut() = StatusCode::TOO_MANY_REQUESTS;
    response.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    response
}

/// `402 Payment Required` carrying a fresh challenge and a Problem Details body.
///
/// The draft requires a fresh challenge both when payment is required and when
/// a presented credential fails validation, so the agent always has something
/// current to answer.
fn refuse_payment(
    payments: &Payments,
    error: PaymentError,
    amount_minor: u64,
    body: &[u8],
) -> Response<MeterBody> {
    let challenge = payments.challenge(amount_minor, body, &Rfc3339::now());
    let problem = serde_json::json!({
        "type": error.problem_type(),
        "title": error.title(),
        "status": 402,
    });

    let mut response = Response::new(body_from(problem.to_string()));
    *response.status_mut() = StatusCode::PAYMENT_REQUIRED;
    let headers = response.headers_mut();
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/problem+json"),
    );
    // A challenge is single-use and time-bound; a cache must never serve it again.
    headers.insert(
        http::header::CACHE_CONTROL,
        http::HeaderValue::from_static("no-store"),
    );
    if let Ok(value) = http::HeaderValue::from_str(&challenge.header_value()) {
        headers.insert(http::header::WWW_AUTHENTICATE, value);
    }
    response
}

fn too_large() -> Response<MeterBody> {
    let mut response = Response::new(body_from(
        r#"{"error":"request_body_too_large","retryable":false}"#.to_owned(),
    ));
    *response.status_mut() = StatusCode::PAYLOAD_TOO_LARGE;
    response.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    response
}

/// What the gate decided before touching the inner service.
enum Verdict {
    /// Proceed, optionally attaching a receipt to the response.
    Admit(Option<String>),
    /// Answer with this instead.
    Refuse(Box<Response<MeterBody>>),
}

impl<S> AdmissionService<S> {
    /// Decide on a buffered request.
    ///
    /// Async only because verifying a payment proof is: everything the scheme
    /// itself specifies is checked locally before the method is consulted.
    /// Takes `self` by value rather than by reference: holding `&self`
    /// across the await would force `Self: Sync`, and so the inner service
    /// too, which is a constraint the meter does not owe this layer. The
    /// caller already has a clone, and it is two `Arc`s wide.
    async fn decide(self, headers: &HeaderMap, body: &[u8]) -> Verdict {
        let quota = self.store.as_ref().and_then(|store| {
            presented_key(headers).and_then(|key| store.quota_for(key).map(|q| (key.to_owned(), q)))
        });

        // A presented credential is checked first and on its own terms. It is
        // how an agent gets past a refusal, so making it conditional on the
        // quota verdict would mean a tenant back inside its cap could not spend
        // a credential it had already paid for.
        if let Some(payments) = self.payments.as_ref()
            && let Some(value) = headers
                .get(crate::mpp::PAYMENT_AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
        {
            let amount = quota
                .as_ref()
                .and_then(|(_, (_, unit_price, _))| self.price_for(headers, *unit_price))
                .unwrap_or(0);
            return match payments.verify(value, body, &Rfc3339::now()).await {
                Ok(receipt) => {
                    tracing::info!(method = payments.method(), "admitted a paid call");
                    Verdict::Admit(Some(receipt.header_value()))
                }
                Err(error) => {
                    tracing::info!(problem = %error.problem_type(), "refusing a payment credential");
                    Verdict::Refuse(Box::new(refuse_payment(payments, error, amount, body)))
                }
            };
        }

        // An unknown key, or a cache too stale to trust, yields no verdict
        // here. Both are the meter's business, and it will refuse them.
        let Some((_, (committed, unit_price, limits))) = quota else {
            return Verdict::Admit(None);
        };
        let decision = assess_limits(committed, 0, unit_price, limits);
        let LimitDecision::Rejected(reason) = decision else {
            return Verdict::Admit(None);
        };

        let Some(payments) = self.payments.as_ref() else {
            tracing::info!(reason = reason_code(reason), "refusing a call over quota");
            return Verdict::Refuse(Box::new(refuse_quota(reason)));
        };
        let amount = self.price_for(headers, unit_price).unwrap_or(0);
        tracing::info!(
            reason = reason_code(reason),
            amount_minor = amount,
            "over quota; offering a payment challenge"
        );
        Verdict::Refuse(Box::new(refuse_payment(
            payments,
            PaymentError::Required,
            amount,
            body,
        )))
    }

    /// Price this call from the tenant's own price book.
    fn price_for(&self, headers: &HeaderMap, unit_price_micros: u64) -> Option<u64> {
        let store = self.store.as_ref()?;
        let key = presented_key(headers)?;
        let tenant = store.tenant_for(key)?;
        let units = priced_units(headers, &tenant.prices)?;
        minor_units(units, unit_price_micros)
    }
}

impl<S, B> Service<Request<B>> for AdmissionService<S>
where
    S: Service<Request<Full<Bytes>>, Response = Response<MeterBody>, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
    B: http_body::Body<Data = Bytes> + Send + 'static,
{
    type Response = Response<MeterBody>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request<B>) -> Self::Future {
        let replacement = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, replacement);
        let gate = self.clone();
        let max_body = self.max_body;

        Box::pin(async move {
            let (parts, body) = request.into_parts();

            // The challenge binds to a digest of this body, so the gate has to
            // see it. The meter buffers again downstream, which for a JSON-RPC
            // request is a small copy and keeps the two layers independent.
            let collected = match body.collect().await {
                Ok(collected) => collected.to_bytes(),
                Err(_) => return Ok(too_large()),
            };
            if collected.len() > max_body {
                return Ok(too_large());
            }

            let verdict = gate.decide(&parts.headers, &collected).await;
            let receipt = match verdict {
                Verdict::Refuse(response) => return Ok(*response),
                Verdict::Admit(receipt) => receipt,
            };

            let mut response = inner
                .call(Request::from_parts(parts, Full::new(collected)))
                .await?;

            // The draft forbids a receipt on an error response: it asserts the
            // payment settled AND the resource was served.
            if let Some(receipt) = receipt
                && response.status().is_success()
                && let Ok(value) = http::HeaderValue::from_str(&receipt)
            {
                response.headers_mut().insert("payment-receipt", value);
            }
            Ok(response)
        })
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
    fn a_call_is_priced_from_the_tenants_own_price_book() {
        let prices = PriceBook::flat(1).with_name("sum", 7);
        let request = headers(&[("mcp-method", "tools/call"), ("mcp-name", "sum")]);
        assert_eq!(priced_units(&request, &prices), Some(7));

        let other = headers(&[("mcp-method", "tools/call"), ("mcp-name", "other")]);
        assert_eq!(priced_units(&other, &prices), Some(1));

        // A request the meter would refuse to classify gets no price guess.
        assert_eq!(priced_units(&headers(&[]), &prices), None);
    }

    #[test]
    fn micros_convert_to_minor_units_and_round_up() {
        // 7 units at 1000 micros each = 7000 micros = 0.7 cents -> 1 cent.
        assert_eq!(minor_units(7, 1_000), Some(1));
        // 100 units at 1000 micros = 100_000 micros = 10 cents exactly.
        assert_eq!(minor_units(100, 1_000), Some(10));
        assert_eq!(minor_units(0, 1_000), Some(0));
        // Rounding up, because serving below cost is the worse failure.
        assert_eq!(minor_units(1, 1), Some(1));
        assert_eq!(minor_units(u64::MAX, 2), None, "overflow must not wrap");
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
    fn a_quota_refusal_without_payments_is_a_429() {
        let response = refuse_quota(LimitReason::QuotaExceeded);
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response.headers().get(http::header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        assert!(
            response
                .headers()
                .get(http::header::WWW_AUTHENTICATE)
                .is_none()
        );
    }
}
