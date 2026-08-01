//! Tower hot path for MCP authentication, caching, and terminal-delivery billing.
//!
//! Request metadata is classified from the mandatory MCP headers. Bodies are
//! deserialized only for cache keys, durable-task attribution, and the terminal
//! response decision. Response bodies remain streaming: the wrapper observes
//! frames as the client consumes them and records usage only after a terminal
//! JSON-RPC response reaches end-of-stream.

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

mod auth;
mod cache;
mod classify;
mod deferred;
mod layer;
mod metrics;
#[cfg(feature = "opentelemetry")]
mod opentelemetry_metrics;
#[cfg(test)]
mod properties;
mod task;

pub use auth::{
    InMemoryTenantStore, MIN_API_KEY_BYTES, Tenant, TenantStore, WeakApiKey, hash_api_key,
    validate_api_key_strength,
};
pub use classify::{ClassificationError, ProtocolHeaders, classify_protocol_headers};
pub use deferred::DeferredCompletions;
pub use layer::{EdgeConfig, MeterBody, MeterLayer, MeterService};
pub use metrics::{EdgeMetrics, MetricsSnapshot};
#[cfg(feature = "opentelemetry")]
pub use opentelemetry_metrics::{OpenTelemetryMetrics, install_opentelemetry};
pub use task::{InMemoryTaskStore, TaskAttributionStore, TaskStoreError, TaskStoreFuture};

/// Standard protocol-version request header.
pub const PROTOCOL_VERSION_HEADER: &str = "mcp-protocol-version";
/// Mirrored JSON-RPC method request header.
pub const METHOD_HEADER: &str = "mcp-method";
/// Mirrored tool, prompt, or resource name request header.
pub const NAME_HEADER: &str = "mcp-name";
/// API-key header accepted in addition to `Authorization: Bearer`.
pub const API_KEY_HEADER: &str = "x-api-key";
