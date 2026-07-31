use std::convert::Infallible;
use std::sync::Arc;

use bytes::Bytes;
use http::{Request, header::CONTENT_TYPE};
use http_body_util::{BodyExt, Full};
use mcp_usage_kit::{
    BillingPipeline, EdgeConfig, InMemoryTenantStore, LogExporter, MeterLayer, PriceBook, Tenant,
};
use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;
use tower::{Layer, ServiceExt};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SumRequest {
    a: i64,
    b: i64,
}

#[derive(Debug, Clone)]
struct Calculator {
    #[expect(dead_code, reason = "tool_handler macro accesses this router field")]
    tool_router: ToolRouter<Self>,
}

impl Calculator {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl Calculator {
    #[tool(description = "Add two integers")]
    fn sum(&self, Parameters(SumRequest { a, b }): Parameters<SumRequest>) -> String {
        (a + b).to_string()
    }
}

#[tool_handler]
impl ServerHandler for Calculator {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }
}

#[tokio::test]
async fn real_rmcp_tool_call_records_the_configured_units() {
    let tenants = Arc::new(InMemoryTenantStore::new());
    tenants.insert(
        "test-key",
        Tenant::new("acme", "cus_acme").with_prices(PriceBook::flat(1).with_name("sum", 7)),
    );
    let billing = Arc::new(BillingPipeline::new(LogExporter::new()));
    let rmcp = StreamableHttpService::<Calculator, LocalSessionManager>::new(
        || Ok(Calculator::new()),
        Default::default(),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true)
            .with_sse_keep_alive(None)
            .with_cancellation_token(CancellationToken::new()),
    );
    let service =
        MeterLayer::new(EdgeConfig::new(tenants).with_recorder(billing.clone())).layer(rmcp);
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "sum",
            "arguments": {"a": 2, "b": 3},
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientInfo": {"name": "meter-test", "version": "1"},
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        }
    });
    let request = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header(CONTENT_TYPE, "application/json")
        .header("host", "localhost")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2026-07-28")
        .header("mcp-method", "tools/call")
        .header("mcp-name", "sum")
        .header("x-api-key", "test-key")
        .body(Full::new(Bytes::from(serde_json::to_vec(&body).unwrap())))
        .unwrap();

    let response = service
        .oneshot(request)
        .await
        .unwrap_or_else(|never: Infallible| match never {});
    let status = response.status();
    let response_body = response.into_body().collect().await.unwrap().to_bytes();
    assert!(
        status.is_success(),
        "rmcp returned {status}: {}",
        String::from_utf8_lossy(&response_body)
    );
    let value: serde_json::Value = serde_json::from_slice(&response_body).unwrap();
    assert_eq!(value["result"]["resultType"], "complete");

    billing.flush().await.unwrap();
    let exported = billing.exporter().exported();
    assert_eq!(exported.len(), 1);
    assert_eq!(exported[0].units, 7);
}
