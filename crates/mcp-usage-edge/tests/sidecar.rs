//! End-to-end verification that metering survives the proxy hop.
//!
//! These tests run a real upstream HTTP server, a real sidecar listener, and a
//! real client, because the thing under test is precisely the transport the
//! in-process library never exercises. Asserting against the published billing
//! rule table means a regression here shows up as a billing difference, which
//! is the only kind of regression that matters for this crate.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderMap, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::server::conn::http1;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::service::TowerToHyperService;
use mcp_usage_edge::proxy::UpstreamProxy;
use mcp_usage_kit::export::ExportFuture;
use mcp_usage_kit::{
    AggregatedUsage, BatchExporter, BillingPipeline, EdgeConfig, InMemoryTenantStore, MeterLayer,
    PriceBook, Tenant,
};
use tokio::net::TcpListener;
use tower::Layer;

const API_KEY: &str = "Zq4vN8xR2tLmK7wP1sB6yH3dF9gJ0cVe";
const PROTOCOL: &str = "2026-07-28";
const TOOL_UNITS: u64 = 7;

// ---------------------------------------------------------------- exporter

#[derive(Debug, Default)]
struct CaptureExporter {
    exported: Mutex<Vec<AggregatedUsage>>,
}

impl CaptureExporter {
    fn total_units(&self) -> u64 {
        self.exported
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|usage| usage.units)
            .sum()
    }
}

impl BatchExporter for CaptureExporter {
    fn export<'a>(&'a self, batch: &'a [AggregatedUsage]) -> ExportFuture<'a> {
        Box::pin(async move {
            self.exported
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(batch);
            Ok(())
        })
    }
}

// ---------------------------------------------------------------- upstream

/// A scripted MCP server. Each request pops the next queued response body, so a
/// test states the exchange it wants rather than reimplementing a server.
#[derive(Clone, Default)]
struct Script {
    bodies: Arc<Mutex<VecDeque<String>>>,
    seen: Arc<Mutex<Vec<HeaderMap>>>,
}

impl Script {
    fn queue(&self, body: serde_json::Value) {
        self.bodies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(body.to_string());
    }

    fn headers_seen(&self) -> Vec<HeaderMap> {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn next_body(&self, headers: HeaderMap) -> String {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(headers);
        self.bodies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .unwrap_or_else(|| r#"{"jsonrpc":"2.0","id":1,"result":{}}"#.to_owned())
    }
}

async fn spawn_upstream(script: Script) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let script = script.clone();
            tokio::spawn(async move {
                let service =
                    hyper::service::service_fn(move |request: Request<hyper::body::Incoming>| {
                        let script = script.clone();
                        async move {
                            let body = script.next_body(request.headers().clone());
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

// ----------------------------------------------------------------- sidecar

struct Harness {
    addr: SocketAddr,
    billing: Arc<BillingPipeline<CaptureExporter>>,
    script: Script,
    client: Client<HttpConnector, Full<Bytes>>,
}

async fn spawn_sidecar() -> Harness {
    let script = Script::default();
    let upstream = spawn_upstream(script.clone()).await;

    let tenants = Arc::new(InMemoryTenantStore::new());
    tenants.insert_unchecked(
        API_KEY,
        Tenant::new("acme", "cus_acme")
            .with_prices(PriceBook::flat(1).with_name("sum", TOOL_UNITS)),
    );
    let billing = Arc::new(BillingPipeline::new(CaptureExporter::default()));
    let edge = EdgeConfig::new(tenants)
        .with_recorder(billing.clone())
        .with_credential_forwarding(true);
    let proxy = UpstreamProxy::new(&format!("http://{upstream}"), Duration::from_secs(5))
        .expect("proxy builds");
    let service = MeterLayer::new(edge).layer(proxy);

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
        billing,
        script,
        client: Client::builder(TokioExecutor::new()).build(HttpConnector::new()),
    }
}

impl Harness {
    /// Drive one MCP exchange through the sidecar and return the response.
    async fn call(
        &self,
        method: &str,
        name: Option<&str>,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let mut builder = Request::builder()
            .method("POST")
            .uri(format!("http://{}/mcp", self.addr))
            .header(http::header::CONTENT_TYPE, "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", PROTOCOL)
            .header("mcp-method", method)
            .header(http::header::AUTHORIZATION, format!("Bearer {API_KEY}"));
        if let Some(name) = name {
            builder = builder.header("mcp-name", name);
        }
        let request = builder
            .body(Full::new(Bytes::from(body.to_string())))
            .expect("request builds");

        let response = self
            .client
            .request(request)
            .await
            .expect("sidecar responds");
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, value)
    }

    async fn billed_units(&self) -> u64 {
        self.billing.flush().await.ok();
        self.billing.exporter().total_units()
    }
}

fn call_body(name: &str) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {"name": name, "arguments": {}}
    })
}

// ------------------------------------------------------------------- tests

#[tokio::test]
async fn a_delivered_tool_call_bills_the_configured_units_through_the_proxy() {
    let harness = spawn_sidecar().await;
    harness.script.queue(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {"resultType": "complete", "content": []}
    }));

    let (status, value) = harness
        .call("tools/call", Some("sum"), call_body("sum"))
        .await;

    assert_eq!(status, StatusCode::OK);
    // The upstream's own response reaches the client unchanged.
    assert_eq!(value["result"]["resultType"], "complete");
    assert_eq!(harness.billed_units().await, TOOL_UNITS);
}

#[tokio::test]
async fn an_interim_input_required_result_is_free() {
    let harness = spawn_sidecar().await;
    harness.script.queue(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {"resultType": "input_required"}
    }));

    harness
        .call("tools/call", Some("sum"), call_body("sum"))
        .await;

    assert_eq!(harness.billed_units().await, 0);
}

#[tokio::test]
async fn a_json_rpc_error_is_free() {
    let harness = spawn_sidecar().await;
    harness.script.queue(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "error": {"code": -32000, "message": "upstream tool failed"}
    }));

    harness
        .call("tools/call", Some("sum"), call_body("sum"))
        .await;

    assert_eq!(harness.billed_units().await, 0);
}

#[tokio::test]
async fn discovery_traffic_is_free() {
    let harness = spawn_sidecar().await;
    harness.script.queue(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {"resultType": "complete", "tools": []}
    }));

    harness
        .call(
            "tools/list",
            None,
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        )
        .await;

    assert_eq!(harness.billed_units().await, 0);
}

#[tokio::test]
async fn task_creation_is_free_and_the_completed_poll_bills_once() {
    let harness = spawn_sidecar().await;

    // Creation: work accepted, not performed.
    harness.script.queue(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {"resultType": "task", "taskId": "task-1", "status": "working"}
    }));
    harness
        .call("tools/call", Some("sum"), call_body("sum"))
        .await;
    assert_eq!(
        harness.billed_units().await,
        0,
        "task creation must be free"
    );

    // A progress poll is still free.
    harness.script.queue(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {"resultType": "complete", "taskId": "task-1", "status": "working"}
    }));
    harness
        .call(
            "tasks/get",
            None,
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tasks/get"}),
        )
        .await;
    assert_eq!(
        harness.billed_units().await,
        0,
        "progress polls must be free"
    );

    // Completion bills the originating tool's price.
    harness.script.queue(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 3,
        "result": {"resultType": "complete", "taskId": "task-1", "status": "completed"}
    }));
    harness
        .call(
            "tasks/get",
            None,
            serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tasks/get"}),
        )
        .await;
    assert_eq!(
        harness.billed_units().await,
        TOOL_UNITS,
        "a completed task bills the originating tool once"
    );

    // Polling a terminal task again must not bill again. This is the failure
    // mode that makes naive per-request metering produce disputes.
    harness.script.queue(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 4,
        "result": {"resultType": "complete", "taskId": "task-1", "status": "completed"}
    }));
    harness
        .call(
            "tasks/get",
            None,
            serde_json::json!({"jsonrpc":"2.0","id":4,"method":"tasks/get"}),
        )
        .await;
    assert_eq!(
        harness.billed_units().await,
        TOOL_UNITS,
        "repeat polls of a terminal task must stay idempotent"
    );
}

#[tokio::test]
async fn the_callers_credential_reaches_the_upstream_when_forwarding_is_on() {
    let harness = spawn_sidecar().await;
    harness.script.queue(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {"resultType": "complete"}
    }));

    harness
        .call("tools/call", Some("sum"), call_body("sum"))
        .await;

    let seen = harness.script.headers_seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0]
            .get(http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
        Some(format!("Bearer {API_KEY}").as_str()),
        "the upstream's own auth must keep working behind the sidecar"
    );
}

#[tokio::test]
async fn the_upstream_host_header_is_rewritten_not_passed_through() {
    let harness = spawn_sidecar().await;
    harness.script.queue(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {"resultType": "complete"}
    }));

    harness
        .call("tools/call", Some("sum"), call_body("sum"))
        .await;

    let seen = harness.script.headers_seen();
    let host = seen[0]
        .get(http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .expect("hyper sets Host from the upstream authority");
    assert_ne!(
        host,
        harness.addr.to_string(),
        "the sidecar must not advertise its own authority upstream"
    );
}

#[tokio::test]
async fn an_unauthenticated_request_never_reaches_the_upstream() {
    let harness = spawn_sidecar().await;

    let request = Request::builder()
        .method("POST")
        .uri(format!("http://{}/mcp", harness.addr))
        .header(http::header::CONTENT_TYPE, "application/json")
        .header("mcp-protocol-version", PROTOCOL)
        .header("mcp-method", "tools/call")
        .header("mcp-name", "sum")
        .body(Full::new(Bytes::from(call_body("sum").to_string())))
        .expect("request builds");

    let response = harness.client.request(request).await.expect("responds");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(
        harness.script.headers_seen().is_empty(),
        "credentials are checked at the edge, before any upstream call"
    );
    assert_eq!(harness.billed_units().await, 0);
}

#[tokio::test]
async fn an_unreachable_upstream_returns_a_gateway_error_and_bills_nothing() {
    // Bind and immediately drop, so the port is almost certainly closed.
    let dead = {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        listener.local_addr().expect("addr")
    };

    let tenants = Arc::new(InMemoryTenantStore::new());
    tenants.insert_unchecked(API_KEY, Tenant::new("acme", "cus_acme"));
    let billing = Arc::new(BillingPipeline::new(CaptureExporter::default()));
    let proxy = UpstreamProxy::new(&format!("http://{dead}"), Duration::from_secs(2))
        .expect("proxy builds");
    let service =
        MeterLayer::new(EdgeConfig::new(tenants).with_recorder(billing.clone())).layer(proxy);

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let service = TowerToHyperService::new(service.clone());
            tokio::spawn(async move {
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });

    let client: Client<HttpConnector, Full<Bytes>> =
        Client::builder(TokioExecutor::new()).build(HttpConnector::new());
    let request = Request::builder()
        .method("POST")
        .uri(format!("http://{addr}/mcp"))
        .header(http::header::CONTENT_TYPE, "application/json")
        .header("mcp-protocol-version", PROTOCOL)
        .header("mcp-method", "tools/call")
        .header("mcp-name", "sum")
        .header(http::header::AUTHORIZATION, format!("Bearer {API_KEY}"))
        .body(Full::new(Bytes::from(call_body("sum").to_string())))
        .expect("request builds");

    let response = client
        .request(request)
        .await
        .expect("sidecar still answers");

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    billing.flush().await.ok();
    assert_eq!(
        billing.exporter().total_units(),
        0,
        "an outage must never bill"
    );
}
