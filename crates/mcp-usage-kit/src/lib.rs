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
    TaskAttribution, TaskOriginKind, Usage, assess_limits, decide, decide_with_task_attribution,
    decide_with_task_origin,
};
// `ExportError`, `ExportFuture`, and `RecordError` appear in the signatures of
// `BatchExporter` and `UsageRecorder`, so an application cannot implement either
// re-exported trait without them. `MeterBody` is the response body of the
// re-exported `MeterService` and is unavoidable in an application's own type
// signatures. The point of this crate is one `cargo add`, so anything reachable
// from a re-exported item's signature is re-exported too.
pub use mcp_usage_export::{
    AggregatedUsage, BatchExporter, BillingPipeline, CompositeExporter, ExportError, ExportFuture,
    FnExporter, LogExporter, NoopRecorder, RecordError, RecordOutcome, SharedExporter,
    SharedRecorder, UsageBuffer, UsageEvent, UsageRecorder,
};
#[cfg(feature = "stripe")]
pub use mcp_usage_export::{StripeDeadLetter, StripeDeadLetterReason, StripeExporter};
pub use mcp_usage_tower::{
    API_KEY_HEADER, DeferredCompletions, EdgeConfig, EdgeConfigError, EdgeMetrics,
    InMemoryTaskStore, InMemoryTenantStore, METHOD_HEADER, MIN_API_KEY_BYTES, MeterBody,
    MeterLayer, MeterService, MetricsSnapshot, NAME_HEADER, PROTOCOL_VERSION_HEADER,
    TaskAttributionStore, TaskStoreError, TaskStoreFuture, Tenant, TenantStore, WeakApiKey,
    hash_api_key, validate_api_key_strength,
};

#[cfg(feature = "opentelemetry")]
pub use mcp_usage_tower::{OpenTelemetryMetrics, install_opentelemetry};

/// Distributed task-attribution adapters.
#[cfg(any(feature = "postgres", feature = "redis"))]
pub use mcp_usage_store as store;
