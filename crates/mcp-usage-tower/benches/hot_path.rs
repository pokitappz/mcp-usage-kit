//! End-to-end cost of one metered request, through the public tower service.
//!
//! Measured through `MeterLayer` rather than against the internal parsers,
//! because what matters is the latency the edge adds to a call, not the cost of
//! any one function inside it. The inner service answers from memory, so
//! everything reported here is meter overhead.

use std::convert::Infallible;
use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use http::{Request, Response, header::CONTENT_TYPE};
use http_body_util::{BodyExt, Full};
use mcp_usage_core::PriceBook;
use mcp_usage_tower::{EdgeConfig, InMemoryTenantStore, MeterLayer, MeterService, Tenant};
use tower::{Layer, ServiceExt, service_fn};

const API_KEY: &str = "bHc7pQ2wR9xT4vN6mK1sB8yH3dF5gJ0c";

/// A price book the size a real tenant carries. Every entry used to be deep
/// copied on every request.
fn tenants(priced_tools: usize) -> Arc<InMemoryTenantStore> {
    let mut prices = PriceBook::flat(1);
    for tool in 0..priced_tools {
        prices = prices.with_name(format!("tool_{tool}"), u64::try_from(tool).unwrap_or(1) + 1);
    }
    let tenants = Arc::new(InMemoryTenantStore::new());
    tenants.insert_unchecked(API_KEY, Tenant::new("acme", "cus_acme").with_prices(prices));
    tenants
}

fn json_request(method: &str, name: Option<&str>, body: Vec<u8>) -> Request<Full<Bytes>> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header(CONTENT_TYPE, "application/json")
        .header("mcp-protocol-version", "2026-07-28")
        .header("mcp-method", method)
        .header("x-api-key", API_KEY);
    if let Some(name) = name {
        builder = builder.header("mcp-name", name);
    }
    builder.body(Full::new(Bytes::from(body))).unwrap()
}

fn tools_call_body(argument_bytes: usize) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "tool_0",
            "arguments": {"document": "x".repeat(argument_bytes)}
        }
    }))
    .unwrap()
}

fn complete_response(content_type: &'static str, body: String) -> Response<Full<Bytes>> {
    Response::builder()
        .header(CONTENT_TYPE, content_type)
        .body(Full::new(Bytes::from(body)))
        .unwrap()
}

fn service(
    config: EdgeConfig,
    content_type: &'static str,
    body: String,
) -> MeterService<
    impl tower::Service<
        Request<Full<Bytes>>,
        Response = Response<Full<Bytes>>,
        Error = Infallible,
        Future: Send,
    > + Clone
    + Send
    + 'static,
> {
    MeterLayer::new(config).layer(service_fn(move |_request: Request<Full<Bytes>>| {
        let body = body.clone();
        async move { Ok::<_, Infallible>(complete_response(content_type, body)) }
    }))
}

async fn time<S>(label: &str, iterations: u32, service: S, build: impl Fn() -> Request<Full<Bytes>>)
where
    S: tower::Service<Request<Full<Bytes>>, Response = Response<mcp_usage_tower::MeterBody>>
        + Clone,
    S::Error: std::fmt::Debug,
{
    // One untimed pass so lazily built state is not charged to the first sample.
    let warmup = service.clone().oneshot(build()).await.expect("warmup");
    black_box(warmup.into_body().collect().await.ok());

    let started = Instant::now();
    for _ in 0..iterations {
        let response = service.clone().oneshot(build()).await.expect("metered");
        let collected = response.into_body().collect().await.ok();
        black_box(collected);
    }
    let per = started.elapsed().as_nanos() as f64 / f64::from(iterations);
    println!("{label:<46} {per:>10.0} ns/request");
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    const ITERATIONS: u32 = 20_000;

    let result = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {"resultType": "complete", "content": [{"type": "text", "text": "ok"}]}
    })
    .to_string();

    for priced_tools in [1, 64] {
        let config = EdgeConfig::new(tenants(priced_tools));
        let metered = service(config, "application/json", result.clone());
        time(
            &format!("tools/call, 2 KB body, {priced_tools} priced tools"),
            ITERATIONS,
            metered,
            || json_request("tools/call", Some("tool_0"), tools_call_body(2_048)),
        )
        .await;
    }

    let config = EdgeConfig::new(tenants(64));
    let metered = service(config, "application/json", result.clone());
    time(
        "tools/call, 128 KB body, 64 priced tools",
        ITERATIONS / 4,
        metered,
        || json_request("tools/call", Some("tool_0"), tools_call_body(128 * 1024)),
    )
    .await;

    // An SSE answer: the reader sees the whole captured stream.
    let stream = format!(": keepalive\r\n\r\nevent: message\r\ndata: {result}\r\n\r\n");
    let config = EdgeConfig::new(tenants(64));
    let metered = service(config, "text/event-stream", stream);
    time(
        "tools/call over SSE, 64 priced tools",
        ITERATIONS,
        metered,
        || json_request("tools/call", Some("tool_0"), tools_call_body(2_048)),
    )
    .await;

    // A cached listing: the hit path, where the body is shared and re-rendered.
    let listing = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "resultType": "complete",
            "ttlMs": 600_000,
            "cacheScope": "private",
            "tools": (0..64).map(|tool| serde_json::json!({
                "name": format!("tool_{tool}"),
                "description": "A tool that does a thing, described at realistic length.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "What to look for."},
                        "limit": {"type": "integer", "description": "How many to return."}
                    },
                    "required": ["query"]
                }
            })).collect::<Vec<_>>()
        }
    })
    .to_string();
    let config = EdgeConfig::new(tenants(64));
    let metrics = config.metrics();
    let metered = service(config, "application/json", listing);
    time(
        "tools/list cache hit, 64-tool listing",
        ITERATIONS,
        metered,
        || {
            json_request(
                "tools/list",
                None,
                serde_json::to_vec(&serde_json::json!({
                    "jsonrpc": "2.0", "id": 1, "method": "tools/list"
                }))
                .unwrap(),
            )
        },
    )
    .await;

    // Reporting a miss as a hit would make this the cost of the origin's
    // listing rather than of the cache path.
    let snapshot = metrics.snapshot();
    assert_eq!(
        snapshot.cache_misses, 1,
        "only the warm-up request may miss: {snapshot:?}"
    );
    assert_eq!(snapshot.cache_hits, u64::from(ITERATIONS));
}
