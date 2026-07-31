//! Minimal Hyper HTTP/1 service wrapped through Tower compatibility.

use std::convert::Infallible;
use std::sync::Arc;

use bytes::Bytes;
use http::{Request, Response};
use http_body_util::Full;
use hyper::server::conn::http1;
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use mcp_usage_kit::{EdgeConfig, InMemoryTenantStore, MeterLayer, Tenant};
use tower::{Layer, service_fn};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let tenants = Arc::new(InMemoryTenantStore::new());
    tenants.insert("development-key", Tenant::new("local", "customer-local"));
    let service = MeterLayer::new(EdgeConfig::new(tenants)).layer(service_fn(
        |_request: Request<Full<Bytes>>| async move {
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
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
    loop {
        let (stream, _) = listener.accept().await?;
        let connection_service = TowerToHyperService::new(service.clone());
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            if let Err(error) = http1::Builder::new()
                .serve_connection(io, connection_service)
                .await
            {
                eprintln!("connection failed: {error}");
            }
        });
    }
}
