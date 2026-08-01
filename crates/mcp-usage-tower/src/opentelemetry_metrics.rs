//! Pull-based OpenTelemetry instruments for edge counters.

use std::sync::{Arc, Weak};

use opentelemetry::metrics::{Meter, ObservableCounter};

use crate::{EdgeMetrics, MetricsSnapshot};

type MetricDefinition = (&'static str, &'static str, fn(MetricsSnapshot) -> u64);

/// Registration guard for the OpenTelemetry observable instruments.
///
/// Keep this value alive for as long as metrics should be exported. Dropping it
/// unregisters the callbacks according to the active OpenTelemetry provider.
#[derive(Debug)]
pub struct OpenTelemetryMetrics {
    instruments: Vec<ObservableCounter<u64>>,
}

impl OpenTelemetryMetrics {
    /// Number of registered observable counters.
    #[must_use]
    pub fn len(&self) -> usize {
        self.instruments.len()
    }

    /// Whether no observable counters are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.instruments.is_empty()
    }
}

/// Register low-cardinality observable counters with an application-owned meter.
#[must_use]
pub fn install_opentelemetry(metrics: &Arc<EdgeMetrics>, meter: &Meter) -> OpenTelemetryMetrics {
    let definitions: [MetricDefinition; 11] = [
        ("mcp.usage.classified", "Classified MCP requests", |s| {
            s.classified
        }),
        ("mcp.usage.rejected", "Rejected MCP requests", |s| {
            s.rejected
        }),
        (
            "mcp.usage.unauthenticated",
            "MCP requests refused for an invalid API key",
            |s| s.unauthenticated,
        ),
        ("mcp.usage.cache_hits", "MCP response cache hits", |s| {
            s.cache_hits
        }),
        ("mcp.usage.cache_misses", "MCP response cache misses", |s| {
            s.cache_misses
        }),
        (
            "mcp.usage.billable_deliveries",
            "Billable MCP deliveries",
            |s| s.billed,
        ),
        (
            "mcp.usage.recorded_units",
            "Recorded MCP usage units",
            |s| s.billed_units,
        ),
        ("mcp.usage.free_deliveries", "Free MCP deliveries", |s| {
            s.free
        }),
        (
            "mcp.usage.duplicates",
            "Suppressed MCP usage duplicates",
            |s| s.duplicates,
        ),
        (
            "mcp.usage.record_failures",
            "MCP usage record failures",
            |s| s.record_failures,
        ),
        (
            "mcp.usage.unrecognized_responses",
            "Unrecognized terminal MCP responses",
            |s| s.unrecognized_responses,
        ),
    ];
    let instruments = definitions
        .into_iter()
        .map(|(name, description, value)| {
            observable_counter(meter, Arc::downgrade(metrics), name, description, value)
        })
        .collect();
    OpenTelemetryMetrics { instruments }
}

fn observable_counter(
    meter: &Meter,
    metrics: Weak<EdgeMetrics>,
    name: &'static str,
    description: &'static str,
    value: fn(MetricsSnapshot) -> u64,
) -> ObservableCounter<u64> {
    meter
        .u64_observable_counter(name)
        .with_description(description)
        .with_callback(move |observer| {
            if let Some(metrics) = metrics.upgrade() {
                observer.observe(value(metrics.snapshot()), &[]);
            }
        })
        .build()
}
