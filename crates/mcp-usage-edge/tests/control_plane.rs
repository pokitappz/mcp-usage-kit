//! What the sidecar does when the control plane misbehaves.
//!
//! The plane owns pricing and counters; the edge owns the hot path. The whole
//! design rests on one promise: **the plane being down must never fail a
//! customer's MCP call.** These tests take the plane away at the worst moments
//! and assert that promise holds, and that the one deliberate exception (a
//! cache too old to trust) fails closed instead of open.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http::{Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::server::conn::http1;
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use mcp_usage_edge::admission::AdmissionLayer;
use mcp_usage_edge::control_plane::{
    ControlPlaneExporter, ControlPlaneTenantStore, PlaneClient, refresh_forever,
};
use mcp_usage_edge::proxy::{UpstreamProxy, build_client};
use mcp_usage_kit::{BillingPipeline, EdgeConfig, MeterEventExporter, MeterLayer, hash_api_key};
use tokio::net::TcpListener;
use tower::Layer;

const KEY: &str = "Zq4vN8xR2tLmK7wP1sB6yH3dF9gJ0cVe";
const PROTOCOL: &str = "2026-07-28";

// --------------------------------------------------------------- mock plane

/// A control plane the test can break on demand.
#[derive(Clone, Default)]
struct MockPlane {
    /// Body returned by `/v1/edge/snapshot`.
    snapshot: Arc<Mutex<serde_json::Value>>,
    /// When set, every request answers 503.
    down: Arc<AtomicBool>,
    /// Usage batches received, in order.
    received: Arc<Mutex<Vec<serde_json::Value>>>,
    /// Snapshot requests served.
    snapshots_served: Arc<AtomicUsize>,
}

impl MockPlane {
    fn set_snapshot(&self, value: serde_json::Value) {
        *self.snapshot.lock().unwrap() = value;
    }

    fn go_down(&self) {
        self.down.store(true, Ordering::SeqCst);
    }

    fn come_back(&self) {
        self.down.store(false, Ordering::SeqCst);
    }

    fn identifiers_received(&self) -> Vec<String> {
        self.received
            .lock()
            .unwrap()
            .iter()
            .filter_map(|batch| batch["events"].as_array().cloned())
            .flatten()
            .filter_map(|event| event["identifier"].as_str().map(str::to_owned))
            .collect()
    }

    fn snapshots_served(&self) -> usize {
        self.snapshots_served.load(Ordering::SeqCst)
    }
}

fn snapshot_json(entries: &[(&str, u64, Option<u64>)]) -> serde_json::Value {
    let tenants: Vec<_> = entries
        .iter()
        .map(|(key, committed, max_units)| {
            serde_json::json!({
                "api_key_sha256": hash_api_key(key),
                "tenant_id": "acme",
                "billing_customer_id": "cus_acme",
                "prices": {"default_units": 1, "names": {"sum": 7}},
                "max_units": max_units,
                "max_spend_micros": null,
                "unit_price_micros": 1000,
                "committed_units": committed,
                "committed_spend_micros": committed * 1000
            })
        })
        .collect();
    serde_json::json!({ "tenants": tenants })
}

async fn spawn_plane(plane: MockPlane) -> SocketAddr {
    plane.set_snapshot(snapshot_json(&[(KEY, 0, None)]));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let plane = plane.clone();
            tokio::spawn(async move {
                let service =
                    hyper::service::service_fn(move |request: Request<hyper::body::Incoming>| {
                        let plane = plane.clone();
                        async move {
                            if plane.down.load(Ordering::SeqCst) {
                                let mut response =
                                    Response::new(Full::new(Bytes::from_static(b"{}")));
                                *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
                                return Ok::<_, Infallible>(response);
                            }
                            let path = request.uri().path().to_owned();
                            let body = if path.ends_with("/snapshot") {
                                plane.snapshots_served.fetch_add(1, Ordering::SeqCst);
                                plane.snapshot.lock().unwrap().to_string()
                            } else {
                                let bytes = request.into_body().collect().await.unwrap().to_bytes();
                                let batch: serde_json::Value = serde_json::from_slice(&bytes)
                                    .unwrap_or(serde_json::Value::Null);
                                let outcomes: Vec<_> = batch["events"]
                                    .as_array()
                                    .cloned()
                                    .unwrap_or_default()
                                    .iter()
                                    .map(|event| {
                                        serde_json::json!({
                                            "identifier": event["identifier"],
                                            "outcome": "accepted"
                                        })
                                    })
                                    .collect();
                                plane.received.lock().unwrap().push(batch);
                                serde_json::json!({ "outcomes": outcomes }).to_string()
                            };
                            let mut response = Response::new(Full::new(Bytes::from(body)));
                            response.headers_mut().insert(
                                http::header::CONTENT_TYPE,
                                http::HeaderValue::from_static("application/json"),
                            );
                            Ok::<_, Infallible>(response)
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

async fn spawn_upstream() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let service = hyper::service::service_fn(
                    |_request: Request<hyper::body::Incoming>| async {
                        let body = Bytes::from_static(
                        br#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","content":[]}}"#,
                    );
                        let mut response = Response::new(Full::new(body));
                        response.headers_mut().insert(
                            http::header::CONTENT_TYPE,
                            http::HeaderValue::from_static("application/json"),
                        );
                        Ok::<_, Infallible>(response)
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

// -------------------------------------------------------------- the sidecar

struct Harness {
    addr: SocketAddr,
    plane: MockPlane,
    store: Arc<ControlPlaneTenantStore>,
    billing: Arc<BillingPipeline<MeterEventExporter<ControlPlaneExporter>>>,
    client: hyper_util::client::legacy::Client<
        hyper_util::client::legacy::connect::HttpConnector,
        Full<Bytes>,
    >,
}

async fn harness(max_stale: Duration, refresh: Duration, enforce_quota: bool) -> Harness {
    let plane_mock = MockPlane::default();
    let plane_addr = spawn_plane(plane_mock.clone()).await;
    let upstream = spawn_upstream().await;

    let http = build_client();
    let client = PlaneClient::new(
        http.clone(),
        &format!("http://{plane_addr}"),
        "edge-token".to_owned(),
        Duration::from_secs(2),
    );
    let store = Arc::new(ControlPlaneTenantStore::new(max_stale));
    store.apply(client.snapshot().await.expect("initial snapshot"));
    tokio::spawn(refresh_forever(store.clone(), client.clone(), refresh));

    let billing = Arc::new(BillingPipeline::new(MeterEventExporter::new(
        ControlPlaneExporter::new(client),
    )));
    let edge = EdgeConfig::new(store.clone()).with_recorder(billing.clone());
    let proxy =
        UpstreamProxy::with_client(http, &format!("http://{upstream}"), Duration::from_secs(5))
            .expect("proxy");
    let gate = if enforce_quota {
        AdmissionLayer::disabled().with_quota(store.clone())
    } else {
        AdmissionLayer::disabled()
    };
    let service = gate.layer(MeterLayer::new(edge).layer(proxy));

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
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
        plane: plane_mock,
        store,
        billing,
        client: hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .build(hyper_util::client::legacy::connect::HttpConnector::new()),
    }
}

impl Harness {
    async fn call(&self, key: &str) -> StatusCode {
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "sum", "arguments": {}}
        });
        let request = Request::builder()
            .method("POST")
            .uri(format!("http://{}/mcp", self.addr))
            .header(http::header::CONTENT_TYPE, "application/json")
            .header("mcp-protocol-version", PROTOCOL)
            .header("mcp-method", "tools/call")
            .header("mcp-name", "sum")
            .header("x-api-key", key)
            .body(Full::new(Bytes::from(body.to_string())))
            .expect("request");
        self.client
            .request(request)
            .await
            .expect("the sidecar always answers")
            .status()
    }
}

/// Poll until `check` passes, or fail. Avoids sleeping for a fixed duration and
/// hoping, which is how timing tests become flaky.
async fn eventually(label: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..200 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for: {label}");
}

// ------------------------------------------------------------------- tests

#[tokio::test]
async fn a_key_from_the_snapshot_authenticates_and_its_usage_reaches_the_plane() {
    let h = harness(Duration::from_secs(900), Duration::from_millis(50), false).await;

    assert_eq!(h.call(KEY).await, StatusCode::OK);
    h.billing.flush().await.expect("flush");

    assert_eq!(
        h.plane.identifiers_received().len(),
        1,
        "the aggregate must reach the plane"
    );
}

#[tokio::test]
async fn revoking_a_key_at_the_plane_stops_it_at_the_edge_within_one_refresh() {
    let h = harness(Duration::from_secs(900), Duration::from_millis(50), false).await;
    assert_eq!(h.call(KEY).await, StatusCode::OK);

    // Absence from the next snapshot IS the revocation.
    h.plane.set_snapshot(snapshot_json(&[]));
    eventually("the revocation to propagate", || h.store.is_empty()).await;

    assert_eq!(
        h.call(KEY).await,
        StatusCode::UNAUTHORIZED,
        "a revoked key must stop working without restarting the sidecar"
    );
}

#[tokio::test]
async fn the_sidecar_keeps_serving_while_the_control_plane_is_down() {
    // The single most important property in the design. A plane outage must
    // cost freshness and delayed invoicing, never availability.
    let h = harness(Duration::from_secs(900), Duration::from_millis(50), false).await;
    assert_eq!(h.call(KEY).await, StatusCode::OK);

    let before = h.plane.snapshots_served();
    h.plane.go_down();
    eventually("at least one refresh to fail", || {
        h.plane.snapshots_served() == before
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    for attempt in 0..5 {
        assert_eq!(
            h.call(KEY).await,
            StatusCode::OK,
            "call {attempt} must succeed with the plane down"
        );
    }

    h.plane.come_back();
    eventually("refreshes to resume", || {
        h.plane.snapshots_served() > before
    })
    .await;
    assert_eq!(h.call(KEY).await, StatusCode::OK);
}

#[tokio::test]
async fn usage_recorded_during_an_outage_is_retained_and_delivered_afterwards() {
    let h = harness(Duration::from_secs(900), Duration::from_millis(50), false).await;

    h.plane.go_down();
    assert_eq!(h.call(KEY).await, StatusCode::OK);

    // The export fails, so the pipeline keeps the batch rather than dropping it.
    assert!(
        h.billing.flush().await.is_err(),
        "an unreachable plane must not look like a successful export"
    );
    assert!(h.plane.identifiers_received().is_empty());

    h.plane.come_back();
    h.billing.flush().await.expect("flush after recovery");

    let delivered = h.plane.identifiers_received();
    assert_eq!(
        delivered.len(),
        1,
        "usage recorded during the outage must arrive once the plane returns"
    );
}

#[tokio::test]
async fn a_cache_older_than_max_stale_fails_closed_rather_than_open() {
    // The one deliberate exception: serving hours-old revocations is worse than
    // refusing. `max_stale` is short here so the branch is reachable in a test.
    let h = harness(Duration::from_millis(100), Duration::from_millis(50), false).await;
    assert_eq!(h.call(KEY).await, StatusCode::OK);

    h.plane.go_down();
    eventually("the cache to age out", || h.store.is_stale()).await;

    assert_eq!(
        h.call(KEY).await,
        StatusCode::UNAUTHORIZED,
        "a cache too old to trust must refuse, not fall open"
    );
}

#[tokio::test]
async fn a_tenant_over_quota_is_refused_and_nothing_is_metered() {
    let h = harness(Duration::from_secs(900), Duration::from_millis(50), true).await;

    // Exactly at the limit still admits: `assess_limits` rejects only when the
    // new total would be greater.
    h.plane.set_snapshot(snapshot_json(&[(KEY, 10, Some(10))]));
    eventually("the at-limit snapshot to load", || {
        h.store
            .quota_for(KEY)
            .is_some_and(|(usage, _, _)| usage.units == 10)
    })
    .await;
    assert_eq!(h.call(KEY).await, StatusCode::OK);
    h.billing.flush().await.ok();
    let after_allowed = h.plane.identifiers_received().len();

    // One unit over, and the gate refuses.
    h.plane.set_snapshot(snapshot_json(&[(KEY, 11, Some(10))]));
    eventually("the over-limit snapshot to load", || {
        h.store
            .quota_for(KEY)
            .is_some_and(|(usage, _, _)| usage.units == 11)
    })
    .await;

    assert_eq!(h.call(KEY).await, StatusCode::TOO_MANY_REQUESTS);

    h.billing.flush().await.ok();
    assert_eq!(
        h.plane.identifiers_received().len(),
        after_allowed,
        "a refused call must record no usage at all"
    );
}

#[tokio::test]
async fn quota_is_not_enforced_when_the_gate_is_disabled() {
    let h = harness(Duration::from_secs(900), Duration::from_millis(50), false).await;
    h.plane.set_snapshot(snapshot_json(&[(KEY, 999, Some(10))]));
    eventually("the over-limit snapshot to load", || {
        h.store
            .quota_for(KEY)
            .is_some_and(|(usage, _, _)| usage.units == 999)
    })
    .await;

    assert_eq!(
        h.call(KEY).await,
        StatusCode::OK,
        "a disabled gate must admit everything, including a tenant over its cap"
    );
}
