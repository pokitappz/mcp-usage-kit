//! Minimal Axum service wrapped by UsageKit.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Response, StatusCode};
use axum::routing::post;
use mcp_usage_kit::{EdgeConfig, InMemoryTenantStore, MeterLayer, Tenant};

async fn endpoint() -> Response<Body> {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","content":[]}}"#,
        ))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let tenants = Arc::new(InMemoryTenantStore::new());
    tenants.insert("development-key", Tenant::new("local", "customer-local"));
    let app = Router::new()
        .route("/mcp", post(endpoint))
        .layer(MeterLayer::new(EdgeConfig::new(tenants)));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
    axum::serve(listener, app).await?;
    Ok(())
}
