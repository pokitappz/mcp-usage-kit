//! Tower metering for `rmcp` Streamable HTTP servers.
//!
//! This is the small public facade intended for application dependencies. It
//! re-exports the protocol semantics, billing pipeline, and edge configuration
//! so an application needs only one `cargo add`.
//!
//! ```no_run
//! use std::sync::Arc;
//! use mcp_usage_kit::{EdgeConfig, InMemoryTenantStore, MeterLayer, Tenant};
//! # use tower::Layer;
//! # let rmcp_service = tower::service_fn(|_: http::Request<http_body_util::Full<bytes::Bytes>>| async {
//! #     Ok::<_, std::convert::Infallible>(http::Response::new(http_body_util::Full::new(bytes::Bytes::new())))
//! # });
//!
//! let tenants = Arc::new(InMemoryTenantStore::new());
//! tenants.insert_unchecked("development-key", Tenant::new("acme", "cus_acme"));
//! let metered = MeterLayer::new(EdgeConfig::new(tenants)).layer(rmcp_service);
//! # let _ = metered;
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::pedantic)]

/// Pure MCP billing semantics.
pub use mcp_usage_core as core;
/// Buffered billing types and exporters.
pub use mcp_usage_export as export;
/// Lower-level Tower edge implementation.
pub use mcp_usage_tower as tower;

pub use mcp_usage_core::{
    Billable, Call, Charge, FreeReason, LimitDecision, LimitReason, Limits, Method, PriceBook,
    Usage, assess_limits, decide, decide_with_task_origin,
};
pub use mcp_usage_export::{
    AggregatedUsage, BatchExporter, BillingPipeline, CompositeExporter, FnExporter, LogExporter,
    NoopRecorder, RecordOutcome, SharedExporter, UsageEvent, UsageRecorder,
};
pub use mcp_usage_tower::{
    DeferredCompletions, EdgeConfig, EdgeMetrics, InMemoryTaskStore, InMemoryTenantStore,
    MIN_API_KEY_BYTES, MeterLayer, MeterService, MetricsSnapshot, TaskAttributionStore,
    TaskStoreError, TaskStoreFuture, Tenant, TenantStore, WeakApiKey, validate_api_key_strength,
};

#[cfg(feature = "opentelemetry")]
pub use mcp_usage_tower::{OpenTelemetryMetrics, install_opentelemetry};

/// Distributed task-attribution adapters.
#[cfg(any(feature = "postgres", feature = "redis"))]
pub use mcp_usage_store as store;
