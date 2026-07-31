//! Run with: `MCP_API_KEY=development-key cargo run -p mcp-usage-kit --example rmcp_server`.

use std::sync::Arc;

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
use tower::Layer;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct AddRequest {
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
    fn add(
        &self,
        Parameters(AddRequest { a, b }): Parameters<AddRequest>,
    ) -> Result<String, String> {
        a.checked_add(b)
            .map(|sum| sum.to_string())
            .ok_or_else(|| "integer addition overflow".to_owned())
    }
}

#[tool_handler]
impl ServerHandler for Calculator {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("A metered calculator")
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let api_key = std::env::var("MCP_API_KEY").map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "MCP_API_KEY must be set to a high-entropy development key",
        )
    })?;
    let tenants = Arc::new(InMemoryTenantStore::new());
    tenants.insert(
        &api_key,
        Tenant::new("development", "cus_replace_me")
            .with_prices(PriceBook::flat(1).with_name("add", 2)),
    );

    let billing = Arc::new(BillingPipeline::new(LogExporter::new()));
    let edge = EdgeConfig::new(tenants).with_recorder(billing.clone());
    let cancellation = CancellationToken::new();
    let rmcp = StreamableHttpService::<Calculator, LocalSessionManager>::new(
        || Ok(Calculator::new()),
        Default::default(),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true)
            .with_cancellation_token(cancellation.child_token()),
    );
    let metered = MeterLayer::new(edge).layer(rmcp);
    let app = axum::Router::new().nest_service("/mcp", metered);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
    println!("metered MCP server listening on http://127.0.0.1:3000/mcp");

    let flush = tokio::spawn({
        let billing = billing.clone();
        let cancellation = cancellation.clone();
        async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        if let Err(error) = billing.flush().await {
                            eprintln!("billing flush failed; retained for retry: {error}");
                        }
                    }
                    () = cancellation.cancelled() => break,
                }
            }
        }
    });

    axum::serve(listener, app)
        .with_graceful_shutdown({
            let cancellation = cancellation.clone();
            async move {
                if let Err(error) = tokio::signal::ctrl_c().await {
                    eprintln!("failed to listen for shutdown signal: {error}");
                }
                cancellation.cancel();
            }
        })
        .await?;
    flush.await?;
    billing.flush().await?;
    Ok(())
}
