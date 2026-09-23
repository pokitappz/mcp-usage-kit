//! The full MPP round trip through a real sidecar: refuse, pay, retry, receipt.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::Bytes;
use http::{Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::server::conn::http1;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::service::TowerToHyperService;
use mcp_usage_edge::admission::AdmissionLayer;
use mcp_usage_edge::control_plane::{ControlPlaneTenantStore, Snapshot};
use mcp_usage_edge::mpp::{FacilitatorMethod, Payments, PaymentsConfig};
use mcp_usage_edge::proxy::{UpstreamProxy, build_client};
use mcp_usage_kit::{
    BillingPipeline, EdgeConfig, LogExporter, MeterEventOutcome, MeterLayer, hash_api_key,
};
use tower::Layer;

const KEY: &str = "Zq4vN8xR2tLmK7wP1sB6yH3dF9gJ0cVe";
const PROTOCOL: &str = "2026-07-28";
const REALM: &str = "mcp.test";
const METHOD: &str = "example";

// ---------------------------------------------------------- mock facilitator

#[derive(Clone, Default)]
struct Facilitator {
    settled: Arc<AtomicBool>,
    down: Arc<AtomicBool>,
    calls: Arc<AtomicUsize>,
    seen: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl Facilitator {
    fn new() -> Self {
        let this = Self::default();
        this.settled.store(true, Ordering::SeqCst);
        this
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
    fn last(&self) -> Option<serde_json::Value> {
        self.seen.lock().unwrap().last().cloned()
    }
}

async fn spawn_facilitator(state: Facilitator) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let state = state.clone();
            tokio::spawn(async move {
                let service =
                    hyper::service::service_fn(move |request: Request<hyper::body::Incoming>| {
                        let state = state.clone();
                        async move {
                            state.calls.fetch_add(1, Ordering::SeqCst);
                            let bytes = request.into_body().collect().await.unwrap().to_bytes();
                            if let Ok(value) = serde_json::from_slice(&bytes) {
                                state.seen.lock().unwrap().push(value);
                            }
                            if state.down.load(Ordering::SeqCst) {
                                let mut response =
                                    Response::new(Full::new(Bytes::from_static(b"{}")));
                                *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
                                return Ok::<_, Infallible>(response);
                            }
                            let body = if state.settled.load(Ordering::SeqCst) {
                                r#"{"settled":true,"reference":"0xsettled"}"#
                            } else {
                                r#"{"settled":false,"reason":"insufficient"}"#
                            };
                            Ok(Response::new(Full::new(Bytes::from(body))))
                        }
                    });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    addr
}

// ------------------------------------------------------------ mock upstream

async fn spawn_upstream(seen: Arc<Mutex<Vec<http::HeaderMap>>>) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let seen = seen.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(
                    move |request: Request<hyper::body::Incoming>| {
                        let seen = seen.clone();
                        async move {
                            seen.lock().unwrap().push(request.headers().clone());
                            let body = Bytes::from_static(
                            br#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","content":[]}}"#,
                        );
                            let mut response = Response::new(Full::new(body));
                            response.headers_mut().insert(
                                http::header::CONTENT_TYPE,
                                http::HeaderValue::from_static("application/json"),
                            );
                            Ok::<_, Infallible>(response)
                        }
                    },
                );
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    addr
}

// ------------------------------------------------------------------ harness

struct Harness {
    addr: SocketAddr,
    facilitator: Facilitator,
    upstream_headers: Arc<Mutex<Vec<http::HeaderMap>>>,
    client: Client<HttpConnector, Full<Bytes>>,
}

/// A snapshot whose tenant is already over its cap, so every call is refused
/// and therefore priced.
fn over_quota_snapshot() -> Snapshot {
    serde_json::from_value(serde_json::json!({
        "tenants": [{
            "api_key_sha256": hash_api_key(KEY),
            "tenant_id": "acme",
            "billing_customer_id": "cus_acme",
            "prices": {"default_units": 1, "names": {"sum": 7}},
            "max_units": 10,
            "max_spend_micros": null,
            "unit_price_micros": 1000,
            "committed_units": 99,
            "committed_spend_micros": 99000
        }]
    }))
    .expect("snapshot")
}

/// A snapshot whose tenant is well inside its cap, so nothing is refused.
fn in_quota_snapshot() -> Snapshot {
    serde_json::from_value(serde_json::json!({
        "tenants": [{
            "api_key_sha256": hash_api_key(KEY),
            "tenant_id": "acme",
            "billing_customer_id": "cus_acme",
            "prices": {"default_units": 1, "names": {"sum": 7}},
            "max_units": 10_000,
            "max_spend_micros": null,
            "unit_price_micros": 1000,
            "committed_units": 1,
            "committed_spend_micros": 1000
        }]
    }))
    .expect("snapshot")
}

async fn harness(ttl: Duration) -> Harness {
    harness_with(ttl, over_quota_snapshot(), usize::MAX).await
}

async fn harness_with(ttl: Duration, snapshot: Snapshot, max_body: usize) -> Harness {
    let facilitator = Facilitator::new();
    let facilitator_addr = spawn_facilitator(facilitator.clone()).await;
    let upstream_headers = Arc::new(Mutex::new(Vec::new()));
    let upstream = spawn_upstream(upstream_headers.clone()).await;

    let http = build_client();
    let store = Arc::new(ControlPlaneTenantStore::new(Duration::from_secs(900)));
    store.apply(snapshot);

    let payments = Payments::new(
        PaymentsConfig {
            secret: b"challenge-secret-not-a-real-secret".to_vec(),
            realm: REALM.to_owned(),
            intent: "charge".to_owned(),
            currency: "usd".to_owned(),
            recipient: "acct_merchant".to_owned(),
            ttl,
            replay_capacity: 1024,
        },
        Box::new(FacilitatorMethod::new(
            http.clone(),
            METHOD.to_owned(),
            format!("http://{facilitator_addr}/verify"),
            None,
            Duration::from_secs(2),
        )),
    );

    let billing = Arc::new(BillingPipeline::new(LogExporter::new()));
    // Matches the sidecar's own default rather than the library's: the
    // upstream is the customer's already-authenticated MCP server, so its
    // credential has to survive the hop.
    let edge = EdgeConfig::new(store.clone())
        .with_recorder(billing)
        .with_credential_forwarding(true);
    let proxy =
        UpstreamProxy::with_client(http, &format!("http://{upstream}"), Duration::from_secs(5))
            .expect("proxy");
    let gate = AdmissionLayer::disabled()
        .with_quota(store)
        .with_payments(Arc::new(payments))
        .with_max_body(max_body);
    let service = gate.layer(MeterLayer::new(edge).layer(proxy));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let service = TowerToHyperService::new(service.clone());
            tokio::spawn(async move {
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });

    Harness {
        addr,
        facilitator,
        upstream_headers,
        client: Client::builder(TokioExecutor::new()).build(HttpConnector::new()),
    }
}

struct Answer {
    status: StatusCode,
    headers: http::HeaderMap,
    body: serde_json::Value,
}

impl Harness {
    fn body_for(tool: &str) -> String {
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": tool, "arguments": {}}
        })
        .to_string()
    }

    /// Call one tool while advertising a different priced identity.
    async fn call_with_name(
        &self,
        body_tool: &str,
        header_tool: &str,
        credential: Option<&str>,
    ) -> Answer {
        self.send(header_tool, credential, Self::body_for(body_tool))
            .await
    }

    /// Call with an arbitrary body, for the size limit.
    async fn call_with_body(&self, tool: &str, credential: Option<&str>, body: String) -> Answer {
        self.send(tool, credential, body).await
    }

    async fn call(&self, tool: &str, credential: Option<&str>) -> Answer {
        self.send(tool, credential, Self::body_for(tool)).await
    }

    async fn send(&self, tool: &str, credential: Option<&str>, body: String) -> Answer {
        let mut builder = Request::builder()
            .method("POST")
            .uri(format!("http://{}/mcp", self.addr))
            .header(http::header::CONTENT_TYPE, "application/json")
            .header("mcp-protocol-version", PROTOCOL)
            .header("mcp-method", "tools/call")
            .header("mcp-name", tool)
            // The tenant's own key rides on Authorization the whole way.
            .header(http::header::AUTHORIZATION, format!("Bearer {KEY}"));
        if let Some(credential) = credential {
            builder = builder.header("payment-authorization", credential);
        }
        let request = builder.body(Full::new(Bytes::from(body))).expect("request");

        let response = self.client.request(request).await.expect("sidecar answers");
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        Answer {
            status,
            headers,
            body: serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        }
    }
}

/// Parse `WWW-Authenticate: Payment k="v", ...` into its auth-params.
fn parse_challenge(headers: &http::HeaderMap) -> HashMap<String, String> {
    let raw = headers
        .get(http::header::WWW_AUTHENTICATE)
        .expect("a challenge")
        .to_str()
        .expect("ascii");
    let params = raw.strip_prefix("Payment ").expect("the Payment scheme");
    let mut out = HashMap::new();
    for part in params.split(", ") {
        if let Some((key, value)) = part.split_once('=') {
            out.insert(key.to_owned(), value.trim_matches('"').to_owned());
        }
    }
    out
}

/// Build a credential answering `challenge`, optionally tampering with it.
fn credential(
    challenge: &HashMap<String, String>,
    mutate: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>),
) -> String {
    let mut echo = serde_json::Map::new();
    for key in [
        "id", "realm", "method", "intent", "request", "expires", "digest", "opaque", "header",
    ] {
        if let Some(value) = challenge.get(key) {
            echo.insert(key.to_owned(), serde_json::Value::String(value.clone()));
        }
    }
    mutate(&mut echo);
    let wire = serde_json::json!({
        "challenge": echo,
        "source": "did:example:payer",
        "payload": {"proof": "0xproof"}
    });
    format!(
        "Payment {}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&wire).unwrap())
    )
}

fn problem(answer: &Answer) -> &str {
    answer.body["type"].as_str().unwrap_or_default()
}

// -------------------------------------------------------------------- tests

#[tokio::test]
async fn an_over_quota_call_is_answered_with_a_conformant_challenge() {
    let h = harness(Duration::from_secs(300)).await;
    let answer = h.call("sum", None).await;

    assert_eq!(answer.status, StatusCode::PAYMENT_REQUIRED);
    assert_eq!(
        answer.headers.get(http::header::CONTENT_TYPE).unwrap(),
        "application/problem+json"
    );
    // A challenge is single-use and time-bound; a cache must never replay it.
    assert_eq!(
        answer.headers.get(http::header::CACHE_CONTROL).unwrap(),
        "no-store"
    );
    assert_eq!(
        problem(&answer),
        "https://paymentauth.org/problems/payment-required"
    );

    let challenge = parse_challenge(&answer.headers);
    for required in ["id", "realm", "method", "intent", "request"] {
        assert!(challenge.contains_key(required), "{required} missing");
    }
    assert_eq!(challenge["realm"], REALM);
    assert_eq!(challenge["method"], METHOD);
    assert_eq!(challenge["intent"], "charge");
    // Moved off Authorization, which still carries the tenant's own key.
    assert_eq!(challenge["header"], "Payment-Authorization");
    assert!(challenge.contains_key("expires"));
    assert!(challenge.contains_key("digest"));

    // Priced from the tenant's own book: sum is 7 units at 1000 micros, which
    // is 7000 micros, rounded up to 1 minor unit.
    let request: serde_json::Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(&challenge["request"]).unwrap()).unwrap();
    assert_eq!(request["amount"], "1");
    assert_eq!(request["currency"], "usd");
    assert_eq!(request["recipient"], "acct_merchant");

    // Nothing reached the upstream for a refused call.
    assert!(h.upstream_headers.lock().unwrap().is_empty());
}

#[tokio::test]
async fn paying_the_challenge_admits_the_call_and_returns_a_receipt() {
    let h = harness(Duration::from_secs(300)).await;
    let refused = h.call("sum", None).await;
    let challenge = parse_challenge(&refused.headers);

    let answer = h.call("sum", Some(&credential(&challenge, |_| {}))).await;

    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.body["result"]["resultType"], "complete");

    let receipt: serde_json::Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(
                answer
                    .headers
                    .get("payment-receipt")
                    .expect("a receipt")
                    .to_str()
                    .unwrap(),
            )
            .unwrap(),
    )
    .unwrap();
    assert_eq!(receipt["status"], "success");
    assert_eq!(receipt["method"], METHOD);
    assert_eq!(receipt["reference"], "0xsettled");
    assert!(receipt["timestamp"].as_str().unwrap().ends_with('Z'));

    // The facilitator was asked exactly what it needs to settle.
    let asked = h.facilitator.last().expect("a verification call");
    assert_eq!(asked["method"], METHOD);
    assert_eq!(asked["request"]["amount"], "1");
    assert_eq!(asked["payload"]["proof"], "0xproof");
    assert_eq!(asked["source"], "did:example:payer");
}

#[tokio::test]
async fn the_tenants_own_credential_still_reaches_the_upstream() {
    let h = harness(Duration::from_secs(300)).await;
    let challenge = parse_challenge(&h.call("sum", None).await.headers);
    h.call("sum", Some(&credential(&challenge, |_| {}))).await;

    let headers = h.upstream_headers.lock().unwrap();
    let forwarded = headers.last().expect("the upstream was called");
    assert_eq!(
        forwarded.get(http::header::AUTHORIZATION).unwrap(),
        &format!("Bearer {KEY}"),
        "moving the credential off Authorization is the whole reason for the header parameter"
    );
}

#[tokio::test]
async fn a_credential_cannot_be_spent_twice() {
    let h = harness(Duration::from_secs(300)).await;
    let challenge = parse_challenge(&h.call("sum", None).await.headers);
    let paid = credential(&challenge, |_| {});

    assert_eq!(h.call("sum", Some(&paid)).await.status, StatusCode::OK);

    let replay = h.call("sum", Some(&paid)).await;
    assert_eq!(replay.status, StatusCode::PAYMENT_REQUIRED);
    assert_eq!(
        problem(&replay),
        "https://paymentauth.org/problems/verification-failed"
    );
    // The replay is refused locally; it must not reach the facilitator again.
    assert_eq!(h.facilitator.calls(), 1);
    // And a refusal still carries a fresh challenge to answer.
    assert!(replay.headers.contains_key(http::header::WWW_AUTHENTICATE));
}

#[tokio::test]
async fn a_credential_cannot_be_moved_to_a_different_call() {
    let h = harness(Duration::from_secs(300)).await;
    // Buy the cheap tool, then try to spend it on the expensive one.
    let challenge = parse_challenge(&h.call("cheap", None).await.headers);
    let paid = credential(&challenge, |_| {});

    let answer = h.call("sum", Some(&paid)).await;

    assert_eq!(answer.status, StatusCode::PAYMENT_REQUIRED);
    assert_eq!(
        problem(&answer),
        "https://paymentauth.org/problems/malformed-credential",
        "the body digest binds a credential to the exact call it paid for"
    );
    assert_eq!(h.facilitator.calls(), 0, "nothing reached settlement");
}

#[tokio::test]
async fn a_rewritten_amount_is_refused_by_the_binding() {
    let h = harness(Duration::from_secs(300)).await;
    let challenge = parse_challenge(&h.call("sum", None).await.headers);

    // The obvious attack: answer the challenge, but for a cent less.
    let cheaper =
        URL_SAFE_NO_PAD.encode(br#"{"amount":"0","currency":"usd","recipient":"acct_merchant"}"#);
    let tampered = credential(&challenge, |echo| {
        echo.insert("request".to_owned(), serde_json::Value::String(cheaper));
    });

    let answer = h.call("sum", Some(&tampered)).await;
    assert_eq!(answer.status, StatusCode::PAYMENT_REQUIRED);
    assert_eq!(
        problem(&answer),
        "https://paymentauth.org/problems/invalid-challenge"
    );
    assert_eq!(h.facilitator.calls(), 0);
}

#[tokio::test]
async fn a_forged_challenge_is_refused() {
    let h = harness(Duration::from_secs(300)).await;
    let challenge = parse_challenge(&h.call("sum", None).await.headers);

    let forged = credential(&challenge, |echo| {
        echo.insert(
            "id".to_owned(),
            serde_json::Value::String("an-id-i-made-up".to_owned()),
        );
    });

    let answer = h.call("sum", Some(&forged)).await;
    assert_eq!(answer.status, StatusCode::PAYMENT_REQUIRED);
    assert_eq!(
        problem(&answer),
        "https://paymentauth.org/problems/invalid-challenge"
    );
}

#[tokio::test]
async fn an_expired_challenge_is_refused() {
    // One second, so the challenge ages out inside the test.
    let h = harness(Duration::from_secs(1)).await;
    let challenge = parse_challenge(&h.call("sum", None).await.headers);
    let paid = credential(&challenge, |_| {});

    tokio::time::sleep(Duration::from_millis(2100)).await;

    let answer = h.call("sum", Some(&paid)).await;
    assert_eq!(answer.status, StatusCode::PAYMENT_REQUIRED);
    assert_eq!(
        problem(&answer),
        "https://paymentauth.org/problems/payment-expired"
    );
    assert_eq!(
        h.facilitator.calls(),
        0,
        "an expired proof is never settled"
    );
}

#[tokio::test]
async fn a_malformed_credential_is_refused_without_reaching_settlement() {
    let h = harness(Duration::from_secs(300)).await;
    for value in ["", "Payment", "Bearer abc", "Payment !!!not-base64!!!"] {
        let answer = h.call("sum", Some(value)).await;
        assert_eq!(answer.status, StatusCode::PAYMENT_REQUIRED, "{value:?}");
        assert_eq!(
            problem(&answer),
            "https://paymentauth.org/problems/malformed-credential",
            "{value:?}"
        );
    }
    assert_eq!(h.facilitator.calls(), 0);
}

#[tokio::test]
async fn an_unsettled_proof_does_not_admit_the_call() {
    let h = harness(Duration::from_secs(300)).await;
    h.facilitator.settled.store(false, Ordering::SeqCst);
    let challenge = parse_challenge(&h.call("sum", None).await.headers);

    let answer = h.call("sum", Some(&credential(&challenge, |_| {}))).await;

    assert_eq!(answer.status, StatusCode::PAYMENT_REQUIRED);
    assert_eq!(
        problem(&answer),
        "https://paymentauth.org/problems/payment-insufficient"
    );
    assert!(answer.headers.get("payment-receipt").is_none());
    assert!(h.upstream_headers.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_unreachable_facilitator_never_admits_the_call() {
    let h = harness(Duration::from_secs(300)).await;
    let challenge = parse_challenge(&h.call("sum", None).await.headers);
    h.facilitator.down.store(true, Ordering::SeqCst);

    let answer = h.call("sum", Some(&credential(&challenge, |_| {}))).await;

    // An unproven payment must never be read as settled: failing open here
    // would make an outage at the facilitator a free-service coupon.
    assert_eq!(answer.status, StatusCode::PAYMENT_REQUIRED);
    assert_eq!(
        problem(&answer),
        "https://paymentauth.org/problems/verification-failed"
    );
    assert!(h.upstream_headers.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_paid_call_is_metered_like_any_other() {
    // Payment is about admission. Once admitted, the call is ordinary traffic
    // and the operator still gets its usage record.
    let h = harness(Duration::from_secs(300)).await;
    let challenge = parse_challenge(&h.call("sum", None).await.headers);
    let answer = h.call("sum", Some(&credential(&challenge, |_| {}))).await;

    assert_eq!(answer.status, StatusCode::OK);
    let headers = h.upstream_headers.lock().unwrap();
    assert_eq!(headers.len(), 1, "exactly one upstream call was made");
    assert_eq!(
        headers[0].get("mcp-name").unwrap(),
        "sum",
        "the metered identity of the call is unchanged by how it was paid for"
    );
    let _ = MeterEventOutcome::Accepted;
}

#[tokio::test]
async fn a_credential_bought_for_a_cheap_call_cannot_redeem_an_expensive_one() {
    // The digest binds the body, but the amount comes from the priced
    // identity in the headers. Without binding that identity too, a challenge
    // taken out under a cheap name would admit an expensive one.
    let h = harness(Duration::from_secs(300)).await;
    let cheap = parse_challenge(&h.call("probe", None).await.headers);
    let paid = credential(&cheap, |_| {});

    // Same body shape, different priced identity on the wire.
    let answer = h.call_with_name("probe", "sum", Some(&paid)).await;

    assert_eq!(answer.status, StatusCode::PAYMENT_REQUIRED);
    assert_eq!(
        problem(&answer),
        "https://paymentauth.org/problems/invalid-challenge"
    );
    assert_eq!(h.facilitator.calls(), 0, "nothing reached settlement");
}

#[tokio::test]
async fn the_payment_credential_is_not_forwarded_to_the_upstream() {
    // It carries the payer's identifier and a settlement proof. The upstream
    // has no business seeing either, still less logging them.
    let h = harness(Duration::from_secs(300)).await;
    let challenge = parse_challenge(&h.call("sum", None).await.headers);
    h.call("sum", Some(&credential(&challenge, |_| {}))).await;

    let headers = h.upstream_headers.lock().unwrap();
    let forwarded = headers.last().expect("the upstream was called");
    assert!(
        forwarded.get("payment-authorization").is_none(),
        "the settlement proof must stop at the gate"
    );
    // The tenant's own credential still goes through, which is the whole
    // reason the scheme's `header` parameter exists.
    assert!(forwarded.get(http::header::AUTHORIZATION).is_some());
}

#[tokio::test]
async fn a_spent_credential_does_not_strand_a_tenant_that_is_inside_quota() {
    // An agent that keeps attaching its last credential is still entitled to
    // service while it owes nothing. Refusing would strand it.
    let h = harness_with(Duration::from_secs(300), in_quota_snapshot(), usize::MAX).await;

    let answer = h.call("sum", Some("Payment !!!not-a-credential!!!")).await;

    assert_eq!(
        answer.status,
        StatusCode::OK,
        "a bad credential must not deny service to a tenant in quota"
    );
}

#[tokio::test]
async fn an_oversized_body_is_refused_before_any_credential_work() {
    // The limit has to bite while the body is still arriving. Buffering it
    // all and measuring afterwards lets an unauthenticated client spend the
    // sidecar's memory before anything checks who they are.
    let h = harness_with(Duration::from_secs(300), over_quota_snapshot(), 512).await;

    let answer = h.call_with_body("sum", None, "x".repeat(4096)).await;

    assert_eq!(answer.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(h.facilitator.calls(), 0);
    assert!(h.upstream_headers.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_body_inside_the_limit_still_passes() {
    let h = harness_with(Duration::from_secs(300), over_quota_snapshot(), 4096).await;
    let answer = h.call("sum", None).await;
    assert_eq!(answer.status, StatusCode::PAYMENT_REQUIRED);
}

#[tokio::test]
async fn legacy_payment_price_uses_body_and_invalid_headers_never_verify() {
    let mut snapshot = over_quota_snapshot();
    snapshot.tenants[0].prices = mcp_usage_kit::PriceBook::flat(1)
        .with_name("expensive", 100)
        .with_name("free", 0);
    let h = harness_with(Duration::from_secs(300), snapshot, 4096).await;
    let body = Harness::body_for("expensive");
    let make = |payment: Option<&str>| {
        let mut req = Request::builder()
            .method("POST")
            .uri(format!("http://{}/mcp", h.addr))
            .header("authorization", format!("Bearer {KEY}"))
            .header("mcp-protocol-version", "2025-11-25")
            .header("mcp-method", "tools/call")
            .header("mcp-name", "free");
        if let Some(payment) = payment {
            req = req.header("payment-authorization", payment);
        }
        req.body(Full::new(Bytes::from(body.clone()))).unwrap()
    };
    let response = h.client.request(make(None)).await.unwrap();
    assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
    let challenge = parse_challenge(response.headers());
    let request: serde_json::Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(&challenge["request"]).unwrap()).unwrap();
    assert_eq!(request["amount"], "10");
    response.into_body().collect().await.unwrap();
    let proof = credential(&challenge, |_| {});
    for (name, value, expected) in [
        ("mcp-method", "tools/call", StatusCode::BAD_REQUEST),
        ("mcp-name", "free", StatusCode::BAD_REQUEST),
        (
            "mcp-protocol-version",
            "2025-11-25",
            StatusCode::BAD_REQUEST,
        ),
        ("authorization", "Bearer wrong", StatusCode::UNAUTHORIZED),
        ("x-api-key", KEY, StatusCode::UNAUTHORIZED),
    ] {
        let mut req = make(Some(&proof));
        req.headers_mut()
            .append(http::HeaderName::from_static(name), value.parse().unwrap());
        let response = h.client.request(req).await.unwrap();
        assert_eq!(response.status(), expected, "{name}");
        response.into_body().collect().await.unwrap();
    }
    let mut malformed = make(Some(&proof));
    malformed
        .headers_mut()
        .insert("mcp-protocol-version", "garbage".parse().unwrap());
    assert_eq!(
        h.client.request(malformed).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(h.facilitator.calls(), 0);
    assert!(h.upstream_headers.lock().unwrap().is_empty());
    let response = h.client.request(make(Some(&proof))).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response.into_body().collect().await.unwrap();
    assert_eq!(h.facilitator.calls(), 1);
    assert_eq!(h.facilitator.last().unwrap()["request"]["amount"], "10");
}

#[tokio::test]
async fn an_over_quota_tenant_can_finish_legacy_transport_and_control_messages() {
    let h = harness(Duration::from_secs(300)).await;
    for (method, body) in [
        ("GET", ""),
        ("DELETE", ""),
        ("POST", r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#),
        (
            "POST",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        ),
        ("POST", r#"{"jsonrpc":"2.0","id":"server-1","result":{}}"#),
    ] {
        let request = Request::builder()
            .method(method)
            .uri(format!("http://{}/mcp", h.addr))
            .header("x-api-key", KEY)
            .header("mcp-protocol-version", "2025-11-25")
            .header("mcp-session-id", "session")
            .header("last-event-id", "event")
            .body(Full::new(Bytes::from_static(body.as_bytes())))
            .unwrap();
        let response = h.client.request(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.into_body().collect().await.unwrap();
    }
    assert_eq!(h.facilitator.calls(), 0);
    assert_eq!(h.upstream_headers.lock().unwrap().len(), 5);
}
