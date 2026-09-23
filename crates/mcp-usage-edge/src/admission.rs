//! Deciding whether a call may proceed, and offering a way to pay when it may not.
//!
//! This sits *outside* the meter, so a refused call is rejected before it is
//! forwarded, and nothing is recorded for it. Classification and authentication
//! are shared with the meter and run before any payment verification. A tenant
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
use http_body_util::{BodyExt, Either, Full, LengthLimitError, Limited};
use mcp_usage_kit::tower::{ClassifiedCall, classify_request_headers, extract_api_key};
use mcp_usage_kit::{LimitDecision, LimitReason, MeterBody, TenantStore, assess_limits};
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
    strict_protocol_version: bool,
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
            strict_protocol_version: false,
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

    /// Use the same protocol policy as the metering layer.
    #[must_use]
    pub const fn with_strict_protocol_version(mut self, strict: bool) -> Self {
        self.strict_protocol_version = strict;
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
            strict_protocol_version: self.strict_protocol_version,
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
    strict_protocol_version: bool,
}

/// The single presented tenant credential, or `None` when absent or ambiguous.
///
/// Uses the meter's exact credential rules so duplicates cannot reach payments.
fn presented_key(headers: &HeaderMap) -> Option<&str> {
    extract_api_key(headers).ok()
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
    priced: &Priced,
    body: &[u8],
) -> Response<MeterBody> {
    let challenge = payments.challenge(
        priced.amount_minor,
        (&priced.method, &priced.name),
        body,
        &Rfc3339::now(),
    );
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

/// A call's priced identity and what it is worth right now.
///
/// Held together because a challenge has to record all three: the amount is
/// derived from the identity, so a credential bought under one identity must
/// not be redeemable under another.
struct Priced {
    method: String,
    name: String,
    amount_minor: u64,
}

impl<S> AdmissionService<S> {
    /// Price this call from the tenant's own price book.
    ///
    /// `None` when the tenant is unknown, the cache is too stale to trust, the
    /// request is one the meter would refuse to classify, or the arithmetic
    /// overflows. A caller must not turn that into a zero-priced challenge: a
    /// facilitator will happily settle zero, which would make a pricing
    /// failure a free pass through the gate.
    fn price_for(
        &self,
        headers: &HeaderMap,
        classified: &ClassifiedCall,
        unit_price_micros: u64,
    ) -> Option<Priced> {
        let store = self.store.as_ref()?;
        let key = presented_key(headers)?;
        let tenant = store.tenant_for(key)?;

        let method = classified.call.method.as_str().to_owned();
        let name = classified.call.name.clone().unwrap_or_default();
        let units = tenant
            .prices
            .units_for(&classified.call.method, classified.call.name.as_deref());
        Some(Priced {
            method,
            name,
            amount_minor: minor_units(units, unit_price_micros)?,
        })
    }

    /// Decide on a buffered request.
    ///
    /// Async only because verifying a payment proof is: everything the scheme
    /// itself specifies is checked locally before the method is consulted.
    ///
    /// Takes `self` by value rather than by reference: holding `&self` across
    /// the await would force `Self: Sync`, and so the inner service too, which
    /// is a constraint the meter does not owe this layer. The caller already
    /// has a clone, and it is two `Arc`s wide.
    async fn decide(
        self,
        headers: &HeaderMap,
        body: &[u8],
        classified: &ClassifiedCall,
    ) -> Verdict {
        if !classified.call.method.delivers_priced_work() {
            return Verdict::Admit(None);
        }
        let quota = self
            .store
            .as_ref()
            .and_then(|store| presented_key(headers).and_then(|key| store.quota_for(key)));
        let priced = quota
            .as_ref()
            .and_then(|(_, unit_price, _)| self.price_for(headers, classified, *unit_price));
        let mut payment_error = None;

        // A presented credential is checked on its own terms and first. It is
        // how an agent gets past a refusal, so making it conditional on the
        // quota verdict would mean a tenant back inside its cap could not
        // spend a credential it had already paid for.
        if let Some(payments) = self.payments.as_ref()
            && let Some(value) = headers
                .get(crate::mpp::PAYMENT_AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
            && let Some(priced) = priced.as_ref()
        {
            match payments
                .verify(
                    value,
                    body,
                    (&priced.method, &priced.name),
                    priced.amount_minor,
                    &Rfc3339::now(),
                )
                .await
            {
                Ok(receipt) => {
                    tracing::info!(method = payments.method(), "admitted a paid call");
                    return Verdict::Admit(Some(receipt.header_value()));
                }
                // Deliberately falls through rather than refusing here. An
                // agent that keeps attaching its last credential is entitled
                // to service while it is inside its quota, and answering 402
                // to a tenant who owes nothing would strand it. The error is
                // carried so that if quota does refuse, the answer names what
                // was actually wrong with the credential rather than the
                // generic "payment required".
                Err(error) => {
                    tracing::info!(
                        problem = %error.problem_type(),
                        "a payment credential did not verify; falling back to quota"
                    );
                    payment_error = Some(error);
                }
            }
        }

        // An unknown key, or a cache too stale to trust, yields no verdict
        // here. Both are the meter's business, and it will refuse them.
        let Some((committed, unit_price, limits)) = quota else {
            return Verdict::Admit(None);
        };
        let LimitDecision::Rejected(reason) = assess_limits(committed, 0, unit_price, limits)
        else {
            return Verdict::Admit(None);
        };

        // Payment is only offered when the call can actually be priced.
        // Charging zero would be worse than refusing.
        let (Some(payments), Some(priced)) = (self.payments.as_ref(), priced) else {
            tracing::info!(reason = reason_code(reason), "refusing a call over quota");
            return Verdict::Refuse(Box::new(refuse_quota(reason)));
        };
        tracing::info!(
            reason = reason_code(reason),
            amount_minor = priced.amount_minor,
            "over quota; offering a payment challenge"
        );
        Verdict::Refuse(Box::new(refuse_payment(
            payments,
            payment_error.unwrap_or(PaymentError::Required),
            &priced,
            body,
        )))
    }
}

/// The body handed to the meter.
///
/// Left when the gate had no reason to read it, so the original stream is
/// passed through untouched; right when a payment challenge had to be bound to
/// a digest of it.
pub type GateBody<B> = Either<B, Full<Bytes>>;

fn bad_request(code: &str) -> Response<MeterBody> {
    let mut response = Response::new(body_from(format!(
        r#"{{"error":"{code}","retryable":false}}"#
    )));
    *response.status_mut() = StatusCode::BAD_REQUEST;
    response.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    response
}

impl<S, B> Service<Request<B>> for AdmissionService<S>
where
    S: Service<Request<GateBody<B>>, Response = Response<MeterBody>, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
    B: http_body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
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
            let (mut parts, body) = request.into_parts();

            // Invalid headers or credentials go directly to the meter's rejection
            // path, including its authentication failure limiter. No payment work runs.
            let classification =
                classify_request_headers(&parts.headers, gate.strict_protocol_version);
            let authenticated = gate.store.as_ref().is_some_and(|store| {
                presented_key(&parts.headers)
                    .and_then(|key| store.authenticate(key))
                    .is_some()
            });
            let (Ok(classification), true) = (classification, authenticated) else {
                parts.headers.remove(crate::mpp::PAYMENT_AUTHORIZATION);
                return inner
                    .call(Request::from_parts(parts, Either::Left(body)))
                    .await;
            };

            // Read a bounded body after authentication. Legacy requests need it for
            // classification; payments also bind their challenge to its digest.
            let (collected, body) = if gate.store.is_some() {
                match Limited::new(body, max_body).collect().await {
                    Ok(collected) => {
                        let bytes = collected.to_bytes();
                        (bytes.clone(), Either::Right(Full::new(bytes)))
                    }
                    Err(error) => {
                        // `Limited` reports the cap and a transport failure
                        // through the same error, so they are told apart by
                        // downcast rather than by conflating the two: telling
                        // a client to shrink a request that was never too big
                        // only wastes its time and pollutes triage.
                        return Ok(if error.downcast_ref::<LengthLimitError>().is_some() {
                            too_large()
                        } else {
                            tracing::debug!("request body could not be read");
                            bad_request("request_body_unreadable")
                        });
                    }
                }
            } else {
                (Bytes::new(), Either::Left(body))
            };

            let Ok(classified) = classification.resolve(&parts.method, &collected) else {
                return Ok(bad_request("invalid_mcp_request"));
            };
            if parts
                .headers
                .get_all(crate::mpp::PAYMENT_AUTHORIZATION)
                .iter()
                .count()
                > 1
            {
                return Ok(bad_request("ambiguous_payment_credential"));
            }
            let verdict = if let Some(classified) = classified {
                gate.decide(&parts.headers, &collected, &classified).await
            } else {
                Verdict::Admit(None)
            };
            let receipt = match verdict {
                Verdict::Refuse(response) => return Ok(*response),
                Verdict::Admit(receipt) => receipt,
            };

            // The gate has consumed the credential. It carries the payer's
            // identifier and a settlement proof, and the upstream has no
            // business seeing either, still less logging them.
            parts.headers.remove(crate::mpp::PAYMENT_AUTHORIZATION);

            let mut response = inner.call(Request::from_parts(parts, body)).await?;

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
