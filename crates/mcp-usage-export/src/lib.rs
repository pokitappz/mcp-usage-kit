//! Buffered usage accounting for MCP calls.
//!
//! Recording is synchronous, memory-only, and never performs network I/O. A
//! separate [`BillingPipeline::flush`] drains aggregates into an exporter. If an
//! export fails, the exact same event identifiers and quantities are retained
//! for retry, which makes ambiguous provider failures safe.

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use thiserror::Error;

mod meter_event;

pub use meter_event::{
    MeterEventDeadLetter, MeterEventDeadLetterReason, MeterEventExporter, MeterEventOutcome,
    MeterEventProvider, MeterEventProviderError, MeterEventProviderFuture,
};

/// Boxed future returned by object-safe billing exporters.
pub type ExportFuture<'a> = Pin<Box<dyn Future<Output = Result<(), ExportError>> + Send + 'a>>;

/// One delivered unit event emitted by the request path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageEvent {
    /// Internal tenant identifier used for deduplication and reconciliation.
    pub tenant_id: String,
    /// Billing-provider customer identifier.
    pub customer_id: String,
    /// Provider meter event name.
    pub meter: String,
    /// Integer quantity to add to the meter.
    pub units: u64,
    /// Stable key for once-only work such as a completed durable task.
    pub idempotency_key: Option<String>,
    /// Event time in Unix seconds.
    pub timestamp: u64,
}

impl UsageEvent {
    /// Construct a usage event stamped with the current wall-clock time.
    #[must_use]
    pub fn now(
        tenant_id: impl Into<String>,
        customer_id: impl Into<String>,
        meter: impl Into<String>,
        units: u64,
        idempotency_key: Option<String>,
    ) -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            tenant_id: tenant_id.into(),
            customer_id: customer_id.into(),
            meter: meter.into(),
            units,
            idempotency_key,
            timestamp,
        }
    }
}

/// One pre-aggregated provider event ready for export.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AggregatedUsage {
    /// Globally unique provider-side idempotency identifier.
    pub identifier: String,
    /// Provider customer identifier.
    pub customer_id: String,
    /// Provider meter event name.
    pub meter: String,
    /// Summed integer quantity.
    pub units: u64,
    /// Timestamp of the oldest event represented by the aggregate.
    pub timestamp: u64,
}

/// Result of attempting to record an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordOutcome {
    /// The event was added to its aggregate.
    Recorded,
    /// The event's idempotency key was already observed.
    Duplicate,
    /// Zero units need no provider event.
    ZeroUnits,
}

/// A synchronous usage sink suitable for the HTTP hot path.
pub trait UsageRecorder: Send + Sync {
    /// Add one event without performing I/O.
    ///
    /// # Errors
    ///
    /// Returns [`RecordError`] only when the local synchronization primitive is
    /// unusable; provider outages cannot surface on this path.
    fn record(&self, event: UsageEvent) -> Result<RecordOutcome, RecordError>;
}

/// Exports a prepared batch away from the request path.
pub trait BatchExporter: Send + Sync {
    /// Export every aggregate in `batch`.
    ///
    /// Retrying the same batch must be safe because identifiers are stable.
    fn export<'a>(&'a self, batch: &'a [AggregatedUsage]) -> ExportFuture<'a>;
}

/// Convenient trait-object alias for composing exporters at runtime.
pub type SharedExporter = Arc<dyn BatchExporter>;

/// Adapter for an application-owned async export function.
pub struct FnExporter<F> {
    export: F,
}

impl<F> FnExporter<F> {
    /// Wrap an async export function without defining a new type.
    #[must_use]
    pub const fn new(export: F) -> Self {
        Self { export }
    }
}

impl<F> fmt::Debug for FnExporter<F> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("FnExporter").finish_non_exhaustive()
    }
}

impl<F> BatchExporter for FnExporter<F>
where
    F: for<'a> Fn(&'a [AggregatedUsage]) -> ExportFuture<'a> + Send + Sync,
{
    fn export<'a>(&'a self, batch: &'a [AggregatedUsage]) -> ExportFuture<'a> {
        (self.export)(batch)
    }
}

/// Fan-out exporter for logs, billing, analytics, and audit sinks.
///
/// Exporters run in registration order. If one fails, a later retry starts from
/// the first exporter. Provider implementations must therefore honor the stable
/// aggregate identifiers, as required by [`BatchExporter`].
#[derive(Default)]
pub struct CompositeExporter {
    exporters: Vec<SharedExporter>,
}

impl CompositeExporter {
    /// Construct an empty fan-out exporter.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            exporters: Vec::new(),
        }
    }

    /// Append an exporter to the fan-out sequence.
    #[must_use]
    pub fn with_exporter(mut self, exporter: SharedExporter) -> Self {
        self.exporters.push(exporter);
        self
    }

    /// Number of configured exporters.
    #[must_use]
    pub fn len(&self) -> usize {
        self.exporters.len()
    }

    /// Whether no exporters are configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.exporters.is_empty()
    }
}

impl fmt::Debug for CompositeExporter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompositeExporter")
            .field("exporters", &self.exporters.len())
            .finish()
    }
}

impl BatchExporter for CompositeExporter {
    fn export<'a>(&'a self, batch: &'a [AggregatedUsage]) -> ExportFuture<'a> {
        Box::pin(async move {
            for exporter in &self.exporters {
                exporter.export(batch).await?;
            }
            Ok(())
        })
    }
}

/// Recording failures are internal synchronization failures only.
#[derive(Debug, Error)]
pub enum RecordError {
    /// A synchronization primitive was poisoned by a panicking thread.
    #[error("usage buffer lock was poisoned")]
    Poisoned,
    /// Adding an event would exceed the representable integer quantity.
    #[error("usage aggregate exceeds u64::MAX units")]
    UnitsOverflow,
}

/// Provider export failure.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ExportError {
    /// The provider or transport rejected an event.
    #[error("billing export failed: {0}")]
    Provider(String),
    /// Another flush already owns the current retry batch.
    #[error("a billing flush is already in progress")]
    FlushInProgress,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct AggregateKey {
    customer_id: String,
    meter: String,
}

#[derive(Debug, Clone)]
struct PendingAggregate {
    units: u64,
    oldest_timestamp: u64,
}

#[derive(Debug, Default)]
struct BufferState {
    pending: HashMap<AggregateKey, PendingAggregate>,
    retry: Vec<AggregatedUsage>,
    seen: HashSet<(String, String)>,
    seen_order: VecDeque<(String, String)>,
}

/// Thread-safe in-memory aggregator with bounded idempotency memory.
pub struct UsageBuffer {
    state: Mutex<BufferState>,
    max_idempotency_keys: usize,
}

impl fmt::Debug for UsageBuffer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        formatter
            .debug_struct("UsageBuffer")
            .field(
                "pending_buckets",
                &(state.pending.len() + state.retry.len()),
            )
            .field("retained_idempotency_keys", &state.seen.len())
            .field("max_idempotency_keys", &self.max_idempotency_keys)
            .finish_non_exhaustive()
    }
}

impl Default for UsageBuffer {
    fn default() -> Self {
        Self::new(100_000)
    }
}

impl UsageBuffer {
    /// Construct a buffer retaining at most `max_idempotency_keys` once-only
    /// keys. Zero disables in-process deduplication.
    #[must_use]
    pub fn new(max_idempotency_keys: usize) -> Self {
        Self {
            state: Mutex::new(BufferState::default()),
            max_idempotency_keys,
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, BufferState>, RecordError> {
        self.state.lock().map_err(|_| RecordError::Poisoned)
    }

    /// Record one event into a `(customer, meter)` aggregate.
    ///
    /// # Errors
    ///
    /// Returns [`RecordError::Poisoned`] if another thread panicked while
    /// mutating the buffer, or [`RecordError::UnitsOverflow`] rather than
    /// silently saturating an invoice quantity.
    pub fn record(&self, event: UsageEvent) -> Result<RecordOutcome, RecordError> {
        if event.units == 0 {
            return Ok(RecordOutcome::ZeroUnits);
        }

        let mut state = self.lock()?;
        let scoped_idempotency = event
            .idempotency_key
            .map(|idempotency_key| (event.tenant_id, idempotency_key));
        if scoped_idempotency
            .as_ref()
            .is_some_and(|scoped_key| state.seen.contains(scoped_key))
        {
            return Ok(RecordOutcome::Duplicate);
        }
        let key = AggregateKey {
            customer_id: event.customer_id,
            meter: event.meter,
        };
        let next_units = state
            .pending
            .get(&key)
            .map_or(Some(event.units), |aggregate| {
                aggregate.units.checked_add(event.units)
            })
            .ok_or(RecordError::UnitsOverflow)?;

        if let Some(scoped_key) = scoped_idempotency
            && self.max_idempotency_keys > 0
        {
            state.seen.insert(scoped_key.clone());
            state.seen_order.push_back(scoped_key);
            while state.seen_order.len() > self.max_idempotency_keys {
                if let Some(expired) = state.seen_order.pop_front() {
                    state.seen.remove(&expired);
                }
            }
        }

        let aggregate = state.pending.entry(key).or_insert(PendingAggregate {
            units: 0,
            oldest_timestamp: event.timestamp,
        });
        aggregate.units = next_units;
        aggregate.oldest_timestamp = aggregate.oldest_timestamp.min(event.timestamp);
        Ok(RecordOutcome::Recorded)
    }

    fn prepare_flush(&self) -> Result<Vec<AggregatedUsage>, RecordError> {
        let mut state = self.lock()?;
        if !state.retry.is_empty() {
            return Ok(std::mem::take(&mut state.retry));
        }
        let mut batch: Vec<_> = std::mem::take(&mut state.pending)
            .into_iter()
            .map(|(key, aggregate)| AggregatedUsage {
                identifier: format!("mcp_usage_{}", uuid::Uuid::new_v4().simple()),
                customer_id: key.customer_id,
                meter: key.meter,
                units: aggregate.units,
                timestamp: aggregate.oldest_timestamp,
            })
            .collect();
        batch.sort_by(|a, b| {
            a.customer_id
                .cmp(&b.customer_id)
                .then_with(|| a.meter.cmp(&b.meter))
        });
        Ok(batch)
    }

    fn restore_after_interruption(&self, batch: Vec<AggregatedUsage>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.retry.is_empty() {
            state.retry = batch;
        } else {
            state.retry.extend(batch);
        }
    }

    /// Number of live aggregate buckets plus retry events.
    #[must_use]
    pub fn pending_buckets(&self) -> usize {
        self.state
            .lock()
            .map(|state| state.pending.len() + state.retry.len())
            .unwrap_or_default()
    }
}

/// Coordinates a [`UsageBuffer`] with an off-path exporter.
pub struct BillingPipeline<E> {
    buffer: UsageBuffer,
    exporter: E,
    flush_in_progress: AtomicBool,
}

impl<E> fmt::Debug for BillingPipeline<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BillingPipeline")
            .field("pending_buckets", &self.pending_buckets())
            .field(
                "flush_in_progress",
                &self.flush_in_progress.load(Ordering::Relaxed),
            )
            .finish_non_exhaustive()
    }
}

impl<E> BillingPipeline<E> {
    /// Construct a pipeline with the default bounded deduplication store.
    #[must_use]
    pub fn new(exporter: E) -> Self {
        Self {
            buffer: UsageBuffer::default(),
            exporter,
            flush_in_progress: AtomicBool::new(false),
        }
    }

    /// Construct a pipeline with an explicit idempotency-key bound.
    #[must_use]
    pub fn with_idempotency_capacity(exporter: E, capacity: usize) -> Self {
        Self {
            buffer: UsageBuffer::new(capacity),
            exporter,
            flush_in_progress: AtomicBool::new(false),
        }
    }

    /// Number of buffered aggregate buckets.
    #[must_use]
    pub fn pending_buckets(&self) -> usize {
        self.buffer.pending_buckets()
    }

    /// Access the exporter for diagnostics and tests.
    #[must_use]
    pub const fn exporter(&self) -> &E {
        &self.exporter
    }
}

impl<E: BatchExporter> BillingPipeline<E> {
    /// Drain and export one batch. Failed batches retain their exact identifiers
    /// for an idempotent retry.
    ///
    /// # Errors
    ///
    /// Returns [`ExportError::FlushInProgress`] for overlapping flushes, or the
    /// provider error after restoring the batch for retry.
    pub async fn flush(&self) -> Result<usize, ExportError> {
        if self
            .flush_in_progress
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return Err(ExportError::FlushInProgress);
        }
        let _guard = FlushGuard(&self.flush_in_progress);
        let batch = self
            .buffer
            .prepare_flush()
            .map_err(|error| ExportError::Provider(error.to_string()))?;
        if batch.is_empty() {
            return Ok(0);
        }
        let retry = RetryGuard::new(&self.buffer, batch);
        self.exporter.export(retry.batch()).await?;
        Ok(retry.commit())
    }
}

struct FlushGuard<'a>(&'a AtomicBool);

impl Drop for FlushGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Owns a prepared batch until the exporter commits successfully.
///
/// Dropping a flush future through cancellation or unwinding drops this guard,
/// which restores the exact batch before another flush can acquire ownership.
struct RetryGuard<'a> {
    buffer: &'a UsageBuffer,
    batch: Option<Vec<AggregatedUsage>>,
}

impl<'a> RetryGuard<'a> {
    fn new(buffer: &'a UsageBuffer, batch: Vec<AggregatedUsage>) -> Self {
        Self {
            buffer,
            batch: Some(batch),
        }
    }

    fn batch(&self) -> &[AggregatedUsage] {
        self.batch.as_deref().unwrap_or_default()
    }

    fn commit(mut self) -> usize {
        self.batch.take().map_or(0, |batch| batch.len())
    }
}

impl Drop for RetryGuard<'_> {
    fn drop(&mut self) {
        if let Some(batch) = self.batch.take() {
            self.buffer.restore_after_interruption(batch);
        }
    }
}

impl<E: BatchExporter> UsageRecorder for BillingPipeline<E> {
    fn record(&self, event: UsageEvent) -> Result<RecordOutcome, RecordError> {
        self.buffer.record(event)
    }
}

/// Stateless exporter that emits structured log events.
#[derive(Debug, Default, Clone, Copy)]
pub struct LogExporter;

impl LogExporter {
    /// Construct a logging exporter.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl BatchExporter for LogExporter {
    fn export<'a>(&'a self, batch: &'a [AggregatedUsage]) -> ExportFuture<'a> {
        Box::pin(async move {
            for usage in batch {
                tracing::info!(
                    identifier = %usage.identifier,
                    meter = %usage.meter,
                    units = usage.units,
                    timestamp = usage.timestamp,
                    "MCP billing usage"
                );
            }
            Ok(())
        })
    }
}

/// Shared, no-op recorder for deployments that want attribution metrics only.
#[derive(Debug, Default)]
pub struct NoopRecorder;

impl UsageRecorder for NoopRecorder {
    fn record(&self, event: UsageEvent) -> Result<RecordOutcome, RecordError> {
        Ok(if event.units == 0 {
            RecordOutcome::ZeroUnits
        } else {
            RecordOutcome::Recorded
        })
    }
}

/// Convenient trait-object alias used by the edge layer.
pub type SharedRecorder = Arc<dyn UsageRecorder>;

impl fmt::Display for RecordOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Recorded => "recorded",
            Self::Duplicate => "duplicate",
            Self::ZeroUnits => "zero_units",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::thread;

    #[derive(Debug, Default)]
    struct CaptureExporter {
        exported: Mutex<Vec<AggregatedUsage>>,
    }

    impl CaptureExporter {
        fn exported(&self) -> Vec<AggregatedUsage> {
            self.exported
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    impl BatchExporter for CaptureExporter {
        fn export<'a>(&'a self, batch: &'a [AggregatedUsage]) -> ExportFuture<'a> {
            Box::pin(async move {
                self.exported
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .extend_from_slice(batch);
                Ok(())
            })
        }
    }

    fn event(tenant: &str, units: u64) -> UsageEvent {
        UsageEvent {
            tenant_id: tenant.to_owned(),
            customer_id: format!("cus_{tenant}"),
            meter: "mcp_units".to_owned(),
            units,
            idempotency_key: None,
            timestamp: 1_800_000_000,
        }
    }

    #[tokio::test]
    async fn aggregates_by_customer_and_meter() {
        let pipeline = BillingPipeline::new(CaptureExporter::default());
        pipeline.record(event("acme", 2)).unwrap();
        pipeline.record(event("acme", 3)).unwrap();
        pipeline.record(event("other", 7)).unwrap();
        assert_eq!(pipeline.pending_buckets(), 2);
        assert_eq!(pipeline.flush().await.unwrap(), 2);

        let exported = pipeline.exporter().exported();
        assert_eq!(exported.len(), 2);
        assert_eq!(exported.iter().map(|e| e.units).sum::<u64>(), 12);
    }

    #[test]
    fn duplicate_task_completions_are_suppressed_per_tenant() {
        let pipeline = BillingPipeline::new(CaptureExporter::default());
        let mut first = event("acme", 10);
        first.idempotency_key = Some("task-1".to_owned());
        assert_eq!(
            pipeline.record(first.clone()).unwrap(),
            RecordOutcome::Recorded
        );
        assert_eq!(pipeline.record(first).unwrap(), RecordOutcome::Duplicate);

        let mut other_tenant = event("other", 10);
        other_tenant.idempotency_key = Some("task-1".to_owned());
        assert_eq!(
            pipeline.record(other_tenant).unwrap(),
            RecordOutcome::Recorded
        );
    }

    #[test]
    fn aggregate_overflow_is_reported_without_consuming_the_idempotency_key() {
        let pipeline = BillingPipeline::new(CaptureExporter::default());
        pipeline.record(event("acme", u64::MAX)).unwrap();
        let mut overflowing = event("acme", 1);
        overflowing.idempotency_key = Some("task-overflow".to_owned());
        assert!(matches!(
            pipeline.record(overflowing.clone()),
            Err(RecordError::UnitsOverflow)
        ));
        assert!(matches!(
            pipeline.record(overflowing),
            Err(RecordError::UnitsOverflow)
        ));
    }

    #[derive(Debug, Default)]
    struct FailOnceExporter {
        failed: AtomicBool,
        attempts: Mutex<Vec<Vec<AggregatedUsage>>>,
    }

    impl BatchExporter for FailOnceExporter {
        fn export<'a>(&'a self, batch: &'a [AggregatedUsage]) -> ExportFuture<'a> {
            Box::pin(async move {
                self.attempts.lock().unwrap().push(batch.to_vec());
                if self.failed.swap(true, Ordering::SeqCst) {
                    Ok(())
                } else {
                    Err(ExportError::Provider("temporary outage".to_owned()))
                }
            })
        }
    }

    #[tokio::test]
    async fn failed_flush_retries_identical_provider_events() {
        let pipeline = BillingPipeline::new(FailOnceExporter::default());
        pipeline.record(event("acme", 5)).unwrap();
        assert!(pipeline.flush().await.is_err());
        assert_eq!(pipeline.pending_buckets(), 1);
        assert_eq!(pipeline.flush().await.unwrap(), 1);

        let attempts = pipeline.exporter().attempts.lock().unwrap();
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0], attempts[1]);
    }

    #[derive(Debug, Default)]
    struct CancelOnceExporter {
        attempts: Mutex<Vec<Vec<AggregatedUsage>>>,
        successful: AtomicUsize,
    }

    impl BatchExporter for CancelOnceExporter {
        fn export<'a>(&'a self, batch: &'a [AggregatedUsage]) -> ExportFuture<'a> {
            Box::pin(async move {
                let attempt = {
                    let mut attempts = self.attempts.lock().unwrap();
                    attempts.push(batch.to_vec());
                    attempts.len()
                };
                if attempt == 1 {
                    std::future::pending::<()>().await;
                }
                self.successful.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn cancelled_flush_restores_the_identical_batch() {
        let pipeline = Arc::new(BillingPipeline::new(CancelOnceExporter::default()));
        pipeline.record(event("acme", 9)).unwrap();
        let flush = tokio::spawn({
            let pipeline = Arc::clone(&pipeline);
            async move { pipeline.flush().await }
        });
        while pipeline.exporter().attempts.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
        flush.abort();
        assert!(flush.await.unwrap_err().is_cancelled());

        assert_eq!(pipeline.pending_buckets(), 1);
        assert_eq!(pipeline.flush().await.unwrap(), 1);
        let attempts = pipeline.exporter().attempts.lock().unwrap();
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0], attempts[1]);
        assert_eq!(pipeline.exporter().successful.load(Ordering::SeqCst), 1);
    }

    #[derive(Debug, Default)]
    struct PanicOnceExporter {
        attempts: Mutex<Vec<Vec<AggregatedUsage>>>,
        panicked: AtomicBool,
        successful: AtomicUsize,
    }

    impl BatchExporter for PanicOnceExporter {
        fn export<'a>(&'a self, batch: &'a [AggregatedUsage]) -> ExportFuture<'a> {
            Box::pin(async move {
                self.attempts.lock().unwrap().push(batch.to_vec());
                assert!(
                    self.panicked.swap(true, Ordering::SeqCst),
                    "intentional exporter panic"
                );
                self.successful.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn panicking_flush_restores_the_identical_batch() {
        let pipeline = Arc::new(BillingPipeline::new(PanicOnceExporter::default()));
        pipeline.record(event("acme", 11)).unwrap();
        let flush = tokio::spawn({
            let pipeline = Arc::clone(&pipeline);
            async move { pipeline.flush().await }
        });
        assert!(flush.await.unwrap_err().is_panic());

        assert_eq!(pipeline.pending_buckets(), 1);
        assert_eq!(pipeline.flush().await.unwrap(), 1);
        let attempts = pipeline.exporter().attempts.lock().unwrap();
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0], attempts[1]);
        assert_eq!(pipeline.exporter().successful.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn debug_output_contains_counts_but_not_buffered_identifiers() {
        #[derive(Debug)]
        struct SensitiveExporter(&'static str);

        impl BatchExporter for SensitiveExporter {
            fn export<'a>(&'a self, _batch: &'a [AggregatedUsage]) -> ExportFuture<'a> {
                Box::pin(async { Ok(()) })
            }
        }

        let pipeline = BillingPipeline::new(SensitiveExporter("https://secret.example"));
        let mut sensitive = event("tenant-private", 3);
        sensitive.customer_id = "customer-private".to_owned();
        sensitive.meter = "meter-private".to_owned();
        sensitive.idempotency_key = Some("task-private".to_owned());
        pipeline.record(sensitive).unwrap();

        let debug = format!("{pipeline:?}");
        for secret in [
            "tenant-private",
            "customer-private",
            "meter-private",
            "task-private",
            "https://secret.example",
        ] {
            assert!(!debug.contains(secret));
        }
        assert!(debug.contains("pending_buckets"));
        assert_eq!(pipeline.exporter().0, "https://secret.example");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_recording_reconciles_exactly() {
        let pipeline = Arc::new(BillingPipeline::new(CaptureExporter::default()));
        let threads = 8;
        let per_thread = 2_000;
        let mut handles = Vec::new();
        for _ in 0..threads {
            let pipeline = pipeline.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..per_thread {
                    pipeline.record(event("acme", 1)).unwrap();
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        pipeline.flush().await.unwrap();
        let total: u64 = pipeline.exporter().exported().iter().map(|e| e.units).sum();
        assert_eq!(total, threads * per_thread);
    }
}
