//! Low-cardinality atomic metrics for the edge hot path.

use std::sync::atomic::{AtomicU64, Ordering};

/// Process-local edge counters.
#[derive(Debug, Default)]
pub struct EdgeMetrics {
    classified: AtomicU64,
    rejected: AtomicU64,
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
    billed: AtomicU64,
    billed_units: AtomicU64,
    free: AtomicU64,
    duplicates: AtomicU64,
    record_failures: AtomicU64,
    unrecognized_responses: AtomicU64,
}

/// Point-in-time metric values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MetricsSnapshot {
    /// Successfully classified requests.
    pub classified: u64,
    /// Requests rejected before reaching the origin.
    pub rejected: u64,
    /// Responses served from cache.
    pub cache_hits: u64,
    /// Cacheable requests not served from cache.
    pub cache_misses: u64,
    /// Delivered calls accepted by the recorder.
    pub billed: u64,
    /// Sum of delivered units accepted by the recorder.
    pub billed_units: u64,
    /// Exchanges classified as free.
    pub free: u64,
    /// Repeated once-only charges suppressed by the recorder.
    pub duplicates: u64,
    /// Local recorder failures.
    pub record_failures: u64,
    /// Bodies that ended without a parseable terminal response.
    pub unrecognized_responses: u64,
}

impl EdgeMetrics {
    pub(crate) fn classified(&self) {
        saturating_add(&self.classified, 1);
    }
    pub(crate) fn rejected(&self) {
        saturating_add(&self.rejected, 1);
    }
    pub(crate) fn cache_hit(&self) {
        saturating_add(&self.cache_hits, 1);
    }
    pub(crate) fn cache_miss(&self) {
        saturating_add(&self.cache_misses, 1);
    }
    pub(crate) fn billed(&self, units: u64) {
        saturating_add(&self.billed, 1);
        saturating_add(&self.billed_units, units);
    }
    pub(crate) fn free(&self) {
        saturating_add(&self.free, 1);
    }
    pub(crate) fn duplicate(&self) {
        saturating_add(&self.duplicates, 1);
    }
    pub(crate) fn record_failure(&self) {
        saturating_add(&self.record_failures, 1);
    }
    pub(crate) fn unrecognized_response(&self) {
        saturating_add(&self.unrecognized_responses, 1);
    }

    /// Read every counter using relaxed ordering.
    #[must_use]
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            classified: self.classified.load(Ordering::Relaxed),
            rejected: self.rejected.load(Ordering::Relaxed),
            cache_hits: self.cache_hits.load(Ordering::Relaxed),
            cache_misses: self.cache_misses.load(Ordering::Relaxed),
            billed: self.billed.load(Ordering::Relaxed),
            billed_units: self.billed_units.load(Ordering::Relaxed),
            free: self.free.load(Ordering::Relaxed),
            duplicates: self.duplicates.load(Ordering::Relaxed),
            record_failures: self.record_failures.load(Ordering::Relaxed),
            unrecognized_responses: self.unrecognized_responses.load(Ordering::Relaxed),
        }
    }

    /// Render all counters in the Prometheus text exposition format.
    ///
    /// Metric names and help strings are fixed and carry no tenant, customer,
    /// method, or tool labels, which keeps cardinality and privacy risk bounded.
    #[must_use]
    pub fn render_prometheus(&self) -> String {
        self.snapshot().render_prometheus()
    }
}

impl MetricsSnapshot {
    /// Render this snapshot in the Prometheus text exposition format.
    #[must_use]
    pub fn render_prometheus(self) -> String {
        let mut output = String::with_capacity(1_600);
        append_metric(
            &mut output,
            "mcp_usage_classified_total",
            "Successfully classified MCP requests.",
            self.classified,
        );
        append_metric(
            &mut output,
            "mcp_usage_rejected_total",
            "MCP requests rejected before reaching the origin.",
            self.rejected,
        );
        append_metric(
            &mut output,
            "mcp_usage_cache_hits_total",
            "MCP responses served from the authorization-aware cache.",
            self.cache_hits,
        );
        append_metric(
            &mut output,
            "mcp_usage_cache_misses_total",
            "Cacheable MCP requests not served from cache.",
            self.cache_misses,
        );
        append_metric(
            &mut output,
            "mcp_usage_billable_deliveries_total",
            "Delivered MCP results accepted by the usage recorder.",
            self.billed,
        );
        append_metric(
            &mut output,
            "mcp_usage_recorded_units_total",
            "Delivered MCP units accepted by the usage recorder.",
            self.billed_units,
        );
        append_metric(
            &mut output,
            "mcp_usage_free_deliveries_total",
            "MCP exchanges classified as free.",
            self.free,
        );
        append_metric(
            &mut output,
            "mcp_usage_duplicates_total",
            "Repeated once-only charges suppressed by the recorder.",
            self.duplicates,
        );
        append_metric(
            &mut output,
            "mcp_usage_record_failures_total",
            "Local recorder or attribution-store failures.",
            self.record_failures,
        );
        append_metric(
            &mut output,
            "mcp_usage_unrecognized_responses_total",
            "Bodies that ended without a recognized terminal MCP response.",
            self.unrecognized_responses,
        );
        output
    }
}

fn append_metric(output: &mut String, name: &str, help: &str, value: u64) {
    output.push_str("# HELP ");
    output.push_str(name);
    output.push(' ');
    output.push_str(help);
    output.push('\n');
    output.push_str("# TYPE ");
    output.push_str(name);
    output.push_str(" counter\n");
    output.push_str(name);
    output.push(' ');
    output.push_str(&value.to_string());
    output.push('\n');
}

fn saturating_add(counter: &AtomicU64, increment: u64) {
    let mut current = counter.load(Ordering::Relaxed);
    loop {
        let next = current.saturating_add(increment);
        match counter.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(observed) => current = observed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_saturate_instead_of_wrapping() {
        let counter = AtomicU64::new(u64::MAX - 1);
        saturating_add(&counter, 10);
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
    }

    #[test]
    fn prometheus_output_has_stable_low_cardinality_names() {
        let metrics = EdgeMetrics::default();
        metrics.classified();
        metrics.billed(7);
        let output = metrics.render_prometheus();
        assert!(output.contains("mcp_usage_classified_total 1\n"));
        assert!(output.contains("mcp_usage_recorded_units_total 7\n"));
        assert!(!output.contains('{'));
    }
}
