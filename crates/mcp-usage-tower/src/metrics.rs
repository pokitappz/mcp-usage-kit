//! Low-cardinality atomic metrics for the edge hot path.

use std::sync::atomic::{AtomicU64, Ordering};

use mcp_usage_core::FreeReason;

/// One counter slot per [`FreeReason`].
const REASONS: usize = FreeReason::ALL.len();

/// One counter slot per [`UnaccountedReason`].
const UNACCOUNTED: usize = UnaccountedReason::ALL.len();

/// Why a response the edge observed produced no accounting at all.
///
/// Four unrelated causes shared one counter, so an operator watching
/// `mcp_usage_unrecognized_responses_total` climb could tell that responses
/// were going unaccounted and nothing about why. They need opposite responses:
/// a media type means the origin is misconfigured and revenue is being lost, a
/// capture overflow means the capture bound is set too low for this traffic,
/// and a task-id mismatch means the origin answered the wrong question.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnaccountedReason {
    /// The origin declared a media type the meter does not read as MCP.
    MediaType,
    /// The body was read but was not a JSON-RPC response.
    Unparseable,
    /// A `tasks/get` was answered with a different task than it asked for.
    TaskIdMismatch,
    /// The captured body exceeded the configured capture bound, so the
    /// terminal result was never seen.
    Oversized,
}

impl UnaccountedReason {
    /// Every reason, in a stable order.
    ///
    /// Callers index metric slots by position, so appending is safe and
    /// reordering is not.
    pub const ALL: [Self; 4] = [
        Self::MediaType,
        Self::Unparseable,
        Self::TaskIdMismatch,
        Self::Oversized,
    ];

    /// The stable wire name, written out so renaming a variant cannot silently
    /// change what an operator's dashboard is keyed on.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MediaType => "media_type",
            Self::Unparseable => "unparseable",
            Self::TaskIdMismatch => "task_id_mismatch",
            Self::Oversized => "oversized",
        }
    }

    /// Position in [`Self::ALL`], and so in the metric slots.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::MediaType => 0,
            Self::Unparseable => 1,
            Self::TaskIdMismatch => 2,
            Self::Oversized => 3,
        }
    }
}

/// Process-local edge counters.
#[derive(Debug, Default)]
pub struct EdgeMetrics {
    classified: AtomicU64,
    rejected: AtomicU64,
    unauthenticated: AtomicU64,
    throttled: AtomicU64,
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
    billed: AtomicU64,
    billed_units: AtomicU64,
    free: AtomicU64,
    free_by_reason: [AtomicU64; REASONS],
    duplicates: AtomicU64,
    record_failures: AtomicU64,
    deferred: AtomicU64,
    unrecognized_responses: AtomicU64,
    unrecognized_by_reason: [AtomicU64; UNACCOUNTED],
}

/// Point-in-time metric values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MetricsSnapshot {
    /// Successfully classified requests.
    pub classified: u64,
    /// Requests rejected for malformed or ambiguous MCP headers.
    pub rejected: u64,
    /// Requests refused for a missing, malformed, or unknown API key. Tracked
    /// apart from `rejected` so credential stuffing is distinguishable from
    /// clients that simply send bad protocol headers.
    pub unauthenticated: u64,
    /// Requests refused because the authentication-failure budget was spent.
    pub throttled: u64,
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
    /// Free exchanges split by reason, indexed by [`FreeReason::index`].
    ///
    /// The single `free` total cannot answer the question the enum exists for:
    /// a spike in `missing_task_attribution` means durable-task charges are
    /// being lost, and collapsed into one counter it is indistinguishable
    /// from ordinary discovery traffic.
    pub free_by_reason: [u64; REASONS],
    /// Repeated once-only charges suppressed by the recorder.
    pub duplicates: u64,
    /// Local recorder failures.
    pub record_failures: u64,
    /// Terminal accounting parked because it could not finish without awaiting.
    /// Only a durable task store produces these.
    pub deferred: u64,
    /// Bodies that ended without a parseable terminal response.
    pub unrecognized_responses: u64,
    /// Unaccounted responses split by cause, indexed by
    /// [`UnaccountedReason::index`].
    pub unrecognized_by_reason: [u64; UNACCOUNTED],
}

impl EdgeMetrics {
    pub(crate) fn classified(&self) {
        saturating_add(&self.classified, 1);
    }
    pub(crate) fn rejected(&self) {
        saturating_add(&self.rejected, 1);
    }
    pub(crate) fn unauthenticated(&self) {
        saturating_add(&self.unauthenticated, 1);
    }
    pub(crate) fn throttled(&self) {
        saturating_add(&self.throttled, 1);
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
    pub(crate) fn free(&self, reason: FreeReason) {
        saturating_add(&self.free, 1);
        saturating_add(&self.free_by_reason[reason.index()], 1);
    }
    pub(crate) fn duplicate(&self) {
        saturating_add(&self.duplicates, 1);
    }
    pub(crate) fn record_failure(&self) {
        saturating_add(&self.record_failures, 1);
    }
    pub(crate) fn deferred(&self) {
        saturating_add(&self.deferred, 1);
    }
    pub(crate) fn unrecognized_response(&self, reason: UnaccountedReason) {
        saturating_add(&self.unrecognized_responses, 1);
        saturating_add(&self.unrecognized_by_reason[reason.index()], 1);
    }

    /// Read every counter using relaxed ordering.
    #[must_use]
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            classified: self.classified.load(Ordering::Relaxed),
            rejected: self.rejected.load(Ordering::Relaxed),
            unauthenticated: self.unauthenticated.load(Ordering::Relaxed),
            throttled: self.throttled.load(Ordering::Relaxed),
            cache_hits: self.cache_hits.load(Ordering::Relaxed),
            cache_misses: self.cache_misses.load(Ordering::Relaxed),
            billed: self.billed.load(Ordering::Relaxed),
            billed_units: self.billed_units.load(Ordering::Relaxed),
            free: self.free.load(Ordering::Relaxed),
            free_by_reason: std::array::from_fn(|slot| {
                self.free_by_reason[slot].load(Ordering::Relaxed)
            }),
            duplicates: self.duplicates.load(Ordering::Relaxed),
            record_failures: self.record_failures.load(Ordering::Relaxed),
            deferred: self.deferred.load(Ordering::Relaxed),
            unrecognized_responses: self.unrecognized_responses.load(Ordering::Relaxed),
            unrecognized_by_reason: std::array::from_fn(|slot| {
                self.unrecognized_by_reason[slot].load(Ordering::Relaxed)
            }),
        }
    }

    /// Render all counters in the Prometheus text exposition format.
    ///
    /// Metric names and help strings are fixed. The only label is `reason`, on
    /// the two breakdowns, and its values come from the closed [`FreeReason`]
    /// and [`UnaccountedReason`] enums. Nothing carries a tenant, customer,
    /// method, or tool label, which keeps cardinality and privacy risk
    /// bounded.
    #[must_use]
    pub fn render_prometheus(&self) -> String {
        self.snapshot().render_prometheus()
    }
}

impl MetricsSnapshot {
    /// Render this snapshot in the Prometheus text exposition format.
    #[must_use]
    pub fn render_prometheus(self) -> String {
        let mut output = String::with_capacity(3_000);
        append_metric(
            &mut output,
            "mcp_usage_classified_total",
            "Successfully classified MCP requests.",
            self.classified,
        );
        append_metric(
            &mut output,
            "mcp_usage_rejected_total",
            "MCP requests rejected for malformed or ambiguous headers.",
            self.rejected,
        );
        append_metric(
            &mut output,
            "mcp_usage_unauthenticated_total",
            "MCP requests refused for a missing, malformed, or unknown API key.",
            self.unauthenticated,
        );
        append_metric(
            &mut output,
            "mcp_usage_throttled_total",
            "MCP requests refused after too many failed authentication attempts.",
            self.throttled,
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
        append_labelled_metric(
            &mut output,
            "mcp_usage_free_deliveries_by_reason_total",
            "MCP exchanges classified as free, split by reason.",
            "reason",
            FreeReason::ALL
                .iter()
                .map(|reason| (reason.as_str(), self.free_by_reason[reason.index()])),
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
            "mcp_usage_deferred_completions_total",
            "Terminal accounting parked to be completed outside the response body.",
            self.deferred,
        );
        append_metric(
            &mut output,
            "mcp_usage_unrecognized_responses_total",
            "Bodies that ended without a recognized terminal MCP response.",
            self.unrecognized_responses,
        );
        append_labelled_metric(
            &mut output,
            "mcp_usage_unrecognized_responses_by_reason_total",
            "Bodies that ended without a recognized terminal MCP response, split by cause.",
            "reason",
            UnaccountedReason::ALL
                .iter()
                .map(|reason| (reason.as_str(), self.unrecognized_by_reason[reason.index()])),
        );
        output
    }
}

/// Write one metric family with a single label whose value set is a closed enum.
///
/// Every series is emitted on every scrape, including the zeroes, so a rate on
/// a reason that has not fired yet still resolves instead of breaking the
/// query. Label values come from [`FreeReason::as_str`], which is a fixed table
/// of lowercase identifiers, so the series count is bounded and no escaping is
/// required.
fn append_labelled_metric<'a>(
    output: &mut String,
    name: &str,
    help: &str,
    label: &str,
    series: impl Iterator<Item = (&'a str, u64)>,
) {
    output.push_str("# HELP ");
    output.push_str(name);
    output.push(' ');
    output.push_str(help);
    output.push('\n');
    output.push_str("# TYPE ");
    output.push_str(name);
    output.push_str(" counter\n");
    for (value, count) in series {
        output.push_str(name);
        output.push('{');
        output.push_str(label);
        output.push_str("=\"");
        output.push_str(value);
        output.push_str("\"} ");
        output.push_str(&count.to_string());
        output.push('\n');
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
    fn every_unaccounted_reason_has_a_distinct_name_and_a_matching_index() {
        let mut names = std::collections::HashSet::new();
        for (slot, reason) in UnaccountedReason::ALL.into_iter().enumerate() {
            assert_eq!(reason.index(), slot, "{reason:?} indexes the wrong slot");
            assert!(names.insert(reason.as_str()), "{reason:?} reuses a name");
        }
        assert_eq!(names.len(), UNACCOUNTED);
    }

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

        // `reason` is the only label the edge ever emits, and it is bounded by
        // the enum. Anything else appearing here is unbounded cardinality.
        for line in output.lines().filter(|line| line.contains('{')) {
            assert!(
                line.starts_with("mcp_usage_free_deliveries_by_reason_total{reason=\"")
                    || line
                        .starts_with("mcp_usage_unrecognized_responses_by_reason_total{reason=\""),
                "unexpected labelled series: {line}"
            );
        }
    }

    #[test]
    fn free_verdicts_are_counted_against_their_own_reason() {
        let metrics = EdgeMetrics::default();
        metrics.free(FreeReason::Discovery);
        metrics.free(FreeReason::Discovery);
        metrics.free(FreeReason::MissingTaskAttribution);

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.free, 3);
        assert_eq!(snapshot.free_by_reason[FreeReason::Discovery.index()], 2);
        assert_eq!(
            snapshot.free_by_reason[FreeReason::MissingTaskAttribution.index()],
            1
        );
        assert_eq!(snapshot.free_by_reason.iter().sum::<u64>(), snapshot.free);

        let output = snapshot.render_prometheus();
        assert!(
            output.contains("mcp_usage_free_deliveries_by_reason_total{reason=\"discovery\"} 2\n")
        );
        assert!(output.contains(
            "mcp_usage_free_deliveries_by_reason_total{reason=\"missing_task_attribution\"} 1\n"
        ));
    }

    #[test]
    fn unaccounted_responses_are_counted_against_their_own_cause() {
        let metrics = EdgeMetrics::default();
        metrics.unrecognized_response(UnaccountedReason::MediaType);
        metrics.unrecognized_response(UnaccountedReason::MediaType);
        metrics.unrecognized_response(UnaccountedReason::Oversized);

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.unrecognized_responses, 3);
        assert_eq!(
            snapshot.unrecognized_by_reason[UnaccountedReason::MediaType.index()],
            2
        );
        assert_eq!(
            snapshot.unrecognized_by_reason[UnaccountedReason::Oversized.index()],
            1
        );
        assert_eq!(
            snapshot.unrecognized_by_reason.iter().sum::<u64>(),
            snapshot.unrecognized_responses
        );

        let output = snapshot.render_prometheus();
        assert!(output.contains(
            "mcp_usage_unrecognized_responses_by_reason_total{reason=\"media_type\"} 2\n"
        ));
    }

    #[test]
    fn every_reason_is_scraped_even_before_it_fires() {
        // A reason missing from the exposition until its first occurrence makes
        // `rate(...)` on it fail exactly when an operator first goes looking.
        let output = MetricsSnapshot::default().render_prometheus();
        for reason in FreeReason::ALL {
            let series = format!(
                "mcp_usage_free_deliveries_by_reason_total{{reason=\"{}\"}} 0\n",
                reason.as_str()
            );
            assert!(output.contains(&series), "missing zero series for {series}");
        }
        for reason in UnaccountedReason::ALL {
            let series = format!(
                "mcp_usage_unrecognized_responses_by_reason_total{{reason=\"{}\"}} 0\n",
                reason.as_str()
            );
            assert!(output.contains(&series), "missing zero series for {series}");
        }
    }
}
